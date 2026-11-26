//! Streaming HTTP client.
//!
//! The body is consumed chunk by chunk and the transfer is aborted as soon as
//! `max_response_bytes` is crossed, so a hostile (or simply huge) response can
//! never balloon the process RSS. Only textual HTML is accepted, character
//! encodings are converted to UTF-8 lossily, and retries distinguish transient
//! failures (429, 5xx, transport) from permanent ones (403, 404, 410).

use std::fmt;
use std::time::Duration;

use futures::StreamExt;
use once_cell::sync::Lazy;
use rand::Rng;
use regex::Regex;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, RETRY_AFTER};
use reqwest::redirect::Policy;
use reqwest::Client;
use url::Url;

use crate::config::FetchConfig;
use crate::error::Result;

static META_CHARSET: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)charset\s*=\s*["']?\s*([A-Za-z0-9_\-]+)"#).expect("static regex: charset")
});

/// A successfully fetched HTML document.
#[derive(Debug, Clone)]
pub struct FetchedPage {
    /// Final URL after redirects.
    pub url: Url,
    /// HTTP status (always 2xx for a success).
    pub status: u16,
    /// Decoded UTF-8 body.
    pub html: String,
    /// Raw body size in bytes.
    pub bytes: usize,
    /// Response `Content-Type` header value.
    pub content_type: String,
}

/// Why a fetch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchFailureKind {
    /// Network-level problem (DNS, TLS, timeout, connection reset, ...).
    Transport,
    /// Non-2xx status code.
    Status {
        /// HTTP status code.
        code: u16,
        /// Parsed `Retry-After` header in seconds.
        retry_after: Option<u64>,
    },
    /// Body exceeded the configured limit and was aborted.
    BodyTooLarge,
    /// `Content-Type` is not HTML/XHTML.
    UnsupportedContentType,
}

/// Classified fetch failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchError {
    /// Failure class.
    pub kind: FetchFailureKind,
    /// Human readable detail.
    pub message: String,
}

impl FetchError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: FetchFailureKind::Transport,
            message: message.into(),
        }
    }

    fn status(code: u16, retry_after: Option<u64>) -> Self {
        Self {
            kind: FetchFailureKind::Status { code, retry_after },
            message: format!("http status {code}"),
        }
    }

    fn too_large(limit: u64) -> Self {
        Self {
            kind: FetchFailureKind::BodyTooLarge,
            message: format!("response body exceeds {limit} bytes"),
        }
    }

    fn unsupported(content_type: &str) -> Self {
        Self {
            kind: FetchFailureKind::UnsupportedContentType,
            message: format!("unsupported content type {content_type:?}"),
        }
    }

    /// HTTP status code, when the failure came from a response.
    pub fn status_code(&self) -> Option<u16> {
        match self.kind {
            FetchFailureKind::Status { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Whether another attempt may succeed.
    pub fn retryable(&self) -> bool {
        match self.kind {
            FetchFailureKind::Transport => true,
            FetchFailureKind::Status { code, .. } => {
                matches!(code, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 507 | 509)
            }
            FetchFailureKind::BodyTooLarge | FetchFailureKind::UnsupportedContentType => false,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self.kind {
            FetchFailureKind::Status { retry_after, .. } => retry_after.map(Duration::from_secs),
            _ => None,
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FetchError {}

/// HTTP fetcher shared by all fetch workers (the `reqwest::Client` is cheap to clone).
#[derive(Debug, Clone)]
pub struct Fetcher {
    client: Client,
    accept_language: String,
    max_bytes: u64,
    max_retries: u32,
    base_delay: Duration,
}

impl Fetcher {
    /// Build the HTTP client from configuration.
    pub fn new(config: &FetchConfig) -> Result<Self> {
        let client = Client::builder()
            .user_agent(config.user_agent.clone())
            .timeout(Duration::from_secs(config.request_timeout_secs.max(1)))
            .connect_timeout(Duration::from_secs(
                config.request_timeout_secs.clamp(1, 20),
            ))
            .redirect(Policy::limited(config.max_redirects.max(1)))
            .gzip(true)
            .brotli(true)
            .zstd(true)
            .deflate(true)
            .cookie_provider(std::sync::Arc::new(reqwest::cookie::Jar::default()))
            .pool_max_idle_per_host(8)
            .build()?;
        Ok(Self {
            client,
            accept_language: config.accept_language.clone(),
            max_bytes: config.max_response_bytes,
            max_retries: config.max_retries,
            base_delay: Duration::from_millis(config.retry_base_delay_ms.max(1)),
        })
    }

    /// Shared client handle (also used by the robots.txt cache).
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// Fetch a URL with the configured retry policy.
    pub async fn fetch(&self, url: &Url) -> std::result::Result<FetchedPage, FetchError> {
        let mut attempt: u32 = 0;
        loop {
            match self.fetch_once(url).await {
                Ok(page) => return Ok(page),
                Err(error) => {
                    if attempt >= self.max_retries || !error.retryable() {
                        return Err(error);
                    }
                    let backoff = error
                        .retry_after()
                        .unwrap_or_else(|| self.base_delay * 2u32.saturating_pow(attempt.min(10)));
                    let backoff = backoff.min(Duration::from_secs(60));
                    let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..250));
                    tracing::debug!(
                        %url,
                        attempt = attempt + 1,
                        delay_ms = (backoff + jitter).as_millis() as u64,
                        reason = %error,
                        "retrying fetch"
                    );
                    tokio::time::sleep(backoff + jitter).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn fetch_once(&self, url: &Url) -> std::result::Result<FetchedPage, FetchError> {
        let response = self
            .client
            .get(url.clone())
            .header(ACCEPT, "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5")
            .header(ACCEPT_LANGUAGE, self.accept_language.as_str())
            .send()
            .await
            .map_err(|e| FetchError::transport(describe_reqwest_error(&e)))?;

        let status = response.status();
        let final_url = response.url().clone();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok());
            return Err(FetchError::status(status.as_u16(), retry_after));
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !is_html_content_type(&content_type) {
            return Err(FetchError::unsupported(&content_type));
        }
        if let Some(length) = response.content_length() {
            if length > self.max_bytes {
                return Err(FetchError::too_large(self.max_bytes));
            }
        }

        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| FetchError::transport(describe_reqwest_error(&e)))?;
            if buffer.len() + chunk.len() > self.max_bytes as usize {
                return Err(FetchError::too_large(self.max_bytes));
            }
            buffer.extend_from_slice(&chunk);
        }

        let html = decode_body(&buffer, &content_type);
        Ok(FetchedPage {
            url: final_url,
            status: status.as_u16(),
            bytes: buffer.len(),
            html,
            content_type,
        })
    }
}

/// Accept only HTML/XHTML payloads.
pub fn is_html_content_type(content_type: &str) -> bool {
    let lowered = content_type.trim().to_ascii_lowercase();
    lowered.starts_with("text/html") || lowered.starts_with("application/xhtml+xml")
}

/// Decode a body into UTF-8 using the declared charset, `<meta charset>` or a lossy fallback.
pub fn decode_body(bytes: &[u8], content_type: &str) -> String {
    let mut label: Option<String> = None;
    if let Some(index) = content_type.to_ascii_lowercase().find("charset=") {
        let rest = &content_type[index + "charset=".len()..];
        let candidate = rest
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .split(|c: char| c == ';' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string();
        if !candidate.is_empty() {
            label = Some(candidate);
        }
    }
    if label.is_none() {
        let head_len = bytes.len().min(4096);
        let head = String::from_utf8_lossy(&bytes[..head_len]);
        if let Some(captures) = META_CHARSET.captures(&head) {
            label = captures.get(1).map(|m| m.as_str().to_string());
        }
    }
    match label
        .as_deref()
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
    {
        Some(encoding) => {
            let (text, _, _) = encoding.decode(bytes);
            text.into_owned()
        }
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn describe_reqwest_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        format!("timeout: {error}")
    } else if error.is_connect() {
        format!("connect error: {error}")
    } else if error.is_decode() {
        format!("decode error: {error}")
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_content_types_are_accepted() {
        assert!(is_html_content_type("text/html; charset=utf-8"));
        assert!(is_html_content_type("TEXT/HTML"));
        assert!(is_html_content_type("application/xhtml+xml"));
        assert!(!is_html_content_type("application/pdf"));
        assert!(!is_html_content_type("application/json"));
        assert!(!is_html_content_type("image/png"));
        assert!(!is_html_content_type(""));
    }

    #[test]
    fn utf8_bodies_pass_through() {
        let html = "<p>Merhaba dünya — rust</p>";
        assert_eq!(
            decode_body(html.as_bytes(), "text/html; charset=utf-8"),
            html
        );
    }

    #[test]
    fn declared_charset_is_honoured() {
        // "İstanbul" in ISO-8859-9 (latin5): 0xDD for İ.
        let bytes = b"\xDDstanbul";
        let decoded = decode_body(bytes, "text/html; charset=iso-8859-9");
        assert_eq!(decoded, "İstanbul");
    }

    #[test]
    fn meta_charset_is_sniffed_when_header_is_silent() {
        let mut body = b"<html><head><meta charset=\"windows-1252\"></head><body>".to_vec();
        body.extend_from_slice(&[0x93, b'q', 0x94]);
        let decoded = decode_body(&body, "text/html");
        assert!(decoded.contains('\u{201c}'), "decoded: {decoded}");
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let decoded = decode_body(&[0xff, 0xfe, 0x00, 0x41], "text/html; charset=utf-8");
        assert!(!decoded.is_empty() || decoded.is_empty());
    }

    #[test]
    fn retry_classification_matches_policy() {
        assert!(FetchError::transport("reset").retryable());
        assert!(FetchError::status(429, Some(3)).retryable());
        assert!(FetchError::status(503, None).retryable());
        assert!(!FetchError::status(403, None).retryable());
        assert!(!FetchError::status(404, None).retryable());
        assert!(!FetchError::status(410, None).retryable());
        assert!(!FetchError::too_large(10).retryable());
        assert_eq!(
            FetchError::status(429, Some(9)).retry_after(),
            Some(Duration::from_secs(9))
        );
    }
}
