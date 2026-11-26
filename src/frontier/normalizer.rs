//! RFC 3986 URL normalization tuned for corpus crawling.
//!
//! Rules implemented here:
//! * fragments are dropped entirely,
//! * tracking parameters (`utm_*`, `fbclid`, `gclid`, ...) are removed and the
//!   remaining ones are sorted so `?b=2&a=1` and `?a=1&b=2` collapse,
//! * relative references are resolved against the base document URL,
//! * non-HTTP schemes and binary/media extensions never enter the frontier,
//! * host names are lowercased and default ports are elided by `url`.

use once_cell::sync::Lazy;
use regex::Regex;
use url::Url;

/// Tracking parameters matched by exact name (case-insensitive).
const TRACKING_EXACT: &[&str] = &[
    "fbclid",
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "twclid",
    "ttclid",
    "li_fat_id",
    "mc_cid",
    "mc_eid",
    "mkt_tok",
    "igshid",
    "ref",
    "ref_src",
    "referrer",
    "source",
    "trk",
    "spm",
    "scid",
    "cmpid",
    "_ga",
    "_gl",
    "ncid",
    "s_kwcid",
    "ef_id",
    "rdt_cid",
    "vero_id",
    "oly_anon_id",
    "oly_enc_id",
    "__hssc",
    "__hstc",
    "__hsfp",
];

/// Tracking parameters matched by prefix (case-insensitive).
const TRACKING_PREFIXES: &[&str] = &["utm_", "pk_", "hsa_", "hsctatracking", "sc_cid", "wt_mc"];

/// Extensions that are never useful for a text corpus.
const BINARY_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "ico", "svg", "avif", "heic", "mp3",
    "mp4", "m4a", "m4v", "wav", "flac", "ogg", "oga", "ogv", "webm", "mov", "avi", "mkv", "wmv",
    "flv", "zip", "tar", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "iso", "dmg", "exe", "msi",
    "dll", "so", "dylib", "bin", "deb", "rpm", "apk", "jar", "css", "js", "mjs", "woff", "woff2",
    "ttf", "otf", "eot", "pdf", "ps", "eps", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt",
    "ods", "odp", "csv", "tsv", "json", "xml", "rss", "atom", "txt", "map", "wasm", "swf", "dat",
    "sql", "db", "sqlite", "parquet", "avro", "torrent",
];

static MULTI_SLASH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/{2,}").expect("static regex: multi slash"));

/// Returns true when a query parameter is a known tracking parameter.
pub fn is_tracking_param(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    if TRACKING_EXACT.contains(&lower.as_str()) {
        return true;
    }
    TRACKING_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// Resolve `raw` against `base` and normalize it. Returns `None` for anything
/// that must never enter the frontier.
pub fn normalize(base: &Url, raw: &str) -> Option<Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let joined = match base.join(trimmed) {
        Ok(url) => url,
        Err(_) => return None,
    };
    normalize_absolute(&joined)
}

/// Normalize an already absolute URL, dropping it when it is not crawlable.
pub fn normalize_absolute(url: &Url) -> Option<Url> {
    if !is_crawlable(url) {
        return None;
    }
    let mut url = url.clone();
    url.set_fragment(None);
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("");
        let _ = url.set_password(None);
    }
    if let Some(host) = url.host_str() {
        let lowered = host.to_ascii_lowercase();
        if lowered != host {
            let _ = url.set_host(Some(&lowered));
        }
    }
    let path = url.path().to_string();
    if path.is_empty() {
        url.set_path("/");
    } else if path.contains("//") {
        let collapsed = MULTI_SLASH.replace_all(&path, "/").into_owned();
        url.set_path(&collapsed);
    }
    sort_and_strip_query(&mut url);
    if !is_crawlable(&url) {
        return None;
    }
    Some(url)
}

fn sort_and_strip_query(url: &mut Url) {
    if url.query().is_none() {
        return;
    }
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !is_tracking_param(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.sort();
    if pairs.is_empty() {
        url.set_query(None);
        return;
    }
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in &pairs {
        serializer.append_pair(key, value);
    }
    let query = serializer.finish();
    url.set_query(Some(&query));
}

/// True when the URL is an HTTP(S) resource whose payload can plausibly be text.
pub fn is_crawlable(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    if url.host_str().map(|h| h.is_empty()).unwrap_or(true) {
        return false;
    }
    if let Some(port) = url.port() {
        if port == 0 {
            return false;
        }
    }
    let path = url.path();
    let last_segment = path.rsplit('/').next().unwrap_or("");
    if let Some((_, extension)) = last_segment.rsplit_once('.') {
        let extension = extension.to_ascii_lowercase();
        if BINARY_EXTENSIONS.contains(&extension.as_str()) {
            return false;
        }
    }
    // Reject obviously dynamic "download" style endpoints by file suffix in query.
    if let Some(query) = url.query() {
        let lower = query.to_ascii_lowercase();
        for marker in ["file=", "download=", "attachment="] {
            if lower.contains(marker) {
                return false;
            }
        }
    }
    true
}

/// Registrable domain (eTLD+1) of the URL, falling back to the host.
///
/// IP literals and single-label hosts are returned verbatim, which is what the
/// per-domain scheduler and the domain filters expect.
pub fn registrable_domain(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let host = host.trim_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Some(host);
    }
    // Single-label hosts (`localhost`, internal names) have no public suffix,
    // so they fall back to the host itself.
    psl::domain_str(&host).map(str::to_string).or(Some(host))
}

/// Apply the configured allow/deny domain filters.
pub fn domain_permitted(url: &Url, allowed: &[String], blocked: &[String]) -> bool {
    let host = match url.host_str() {
        Some(host) => host.to_ascii_lowercase(),
        None => return false,
    };
    let registrable = registrable_domain(url).unwrap_or_else(|| host.clone());
    let matches = |entry: &String| -> bool {
        let entry = entry.trim().trim_start_matches('.').to_ascii_lowercase();
        if entry.is_empty() {
            return false;
        }
        host == entry || host.ends_with(&format!(".{entry}")) || registrable == entry
    };
    if !allowed.is_empty() && !allowed.iter().any(matches) {
        return false;
    }
    !blocked.iter().any(matches)
}

/// Priority bucket for a link discovered at `depth`.
///
/// Shallow pages are fetched first, which keeps a crawl useful even when the
/// budget (`max_pages`) is exhausted early.
pub fn priority_for_depth(depth: u32) -> u8 {
    match depth {
        0 => 240,
        1 => 200,
        2 => 150,
        3 => 110,
        4 => 80,
        5 => 60,
        _ => 40,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://blog.example.com/posts/rust").unwrap()
    }

    #[test]
    fn strips_tracking_parameters_and_sorts_the_rest() {
        let url = normalize(
            &base(),
            "https://example.com/page?utm_source=x&b=2&fbclid=abc&a=1&gclid=z",
        )
        .expect("crawlable");
        assert_eq!(url.as_str(), "https://example.com/page?a=1&b=2");
    }

    #[test]
    fn removes_fragment_and_resolves_relative_paths() {
        let url = normalize(&base(), "../other/page#section-2").expect("crawlable");
        assert_eq!(url.as_str(), "https://blog.example.com/other/page");

        let url = normalize(&base(), "/root").expect("crawlable");
        assert_eq!(url.as_str(), "https://blog.example.com/root");

        assert!(normalize(&base(), "#anchor").is_none());
    }

    #[test]
    fn rejects_binary_and_non_http_schemes() {
        assert!(normalize(&base(), "https://example.com/photo.JPG").is_none());
        assert!(normalize(&base(), "https://example.com/app.js").is_none());
        assert!(normalize(&base(), "https://example.com/archive.tar.gz").is_none());
        assert!(normalize(&base(), "mailto:someone@example.com").is_none());
        assert!(normalize(&base(), "javascript:alert(1)").is_none());
        assert!(normalize(&base(), "tel:+15551234").is_none());
        assert!(normalize(&base(), "data:text/html,<h1>x</h1>").is_none());
        assert!(normalize(&base(), "ftp://example.com/file").is_none());
    }

    #[test]
    fn lowercases_hosts_and_drops_default_ports_and_credentials() {
        let url =
            normalize(&base(), "HTTPS://user:pass@Example.COM:443/A/B?q=1").expect("crawlable");
        assert_eq!(url.as_str(), "https://example.com/A/B?q=1");
    }

    #[test]
    fn extracts_registrable_domains() {
        let url = Url::parse("https://news.blog.example.co.uk/x").unwrap();
        assert_eq!(registrable_domain(&url).as_deref(), Some("example.co.uk"));
        let ip = Url::parse("http://127.0.0.1:8080/").unwrap();
        assert_eq!(registrable_domain(&ip).as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn domain_filters_respect_allow_and_deny_lists() {
        let url = Url::parse("https://sub.example.com/page").unwrap();
        assert!(domain_permitted(&url, &[], &[]));
        assert!(domain_permitted(&url, &["example.com".into()], &[]));
        assert!(!domain_permitted(&url, &["other.com".into()], &[]));
        assert!(!domain_permitted(&url, &[], &["example.com".into()]));
        assert!(domain_permitted(&url, &[], &["other.com".into()]));
    }

    #[test]
    fn depth_maps_to_decreasing_priority() {
        assert!(priority_for_depth(0) > priority_for_depth(1));
        assert!(priority_for_depth(2) > priority_for_depth(9));
    }

    #[test]
    fn empty_path_becomes_root() {
        let url = normalize(&base(), "https://example.com").expect("crawlable");
        assert_eq!(url.as_str(), "https://example.com/");
    }
}
