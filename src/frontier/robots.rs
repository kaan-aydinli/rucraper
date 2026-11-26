//! Asynchronous `robots.txt` fetching, parsing, TTL caching and sitemap discovery.
//!
//! Parsing follows the group/rules model of RFC 9309: the most specific
//! user-agent group wins, `Allow` wins ties against `Disallow` of the same
//! length, `*` and `$` wildcards are supported, and `Crawl-delay` is honoured.
//! Rules are cached in memory for the hot path and persisted in the embedded
//! database so a restart does not re-fetch every host's robots file.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use url::Url;

const MAX_ROBOTS_BYTES: usize = 512 * 1024;
const MEMORY_CACHE_LIMIT: usize = 20_000;

/// One `Allow`/`Disallow` line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Rule {
    allow: bool,
    pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay_ms: Option<u64>,
}

/// Parsed rules that apply to one crawler token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RobotsRules {
    rules: Vec<Rule>,
    crawl_delay_ms: Option<u64>,
    sitemaps: Vec<String>,
}

impl RobotsRules {
    /// Rules that allow everything (missing or unparsable robots.txt).
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// True when `path` (including its query string) may be fetched.
    pub fn allowed(&self, path: &str) -> bool {
        let target = if path.is_empty() { "/" } else { path };
        let mut best_len: Option<usize> = None;
        let mut best_allow = true;
        for rule in &self.rules {
            if rule.pattern.is_empty() {
                continue;
            }
            if !pattern_matches(&rule.pattern, target) {
                continue;
            }
            let len = rule.pattern.len();
            match best_len {
                Some(current) if len < current => {}
                Some(current) if len == current => {
                    if rule.allow {
                        best_allow = true;
                    }
                }
                _ => {
                    best_len = Some(len);
                    best_allow = rule.allow;
                }
            }
        }
        best_allow
    }

    /// `Crawl-delay` if the origin published one.
    pub fn crawl_delay(&self) -> Option<Duration> {
        self.crawl_delay_ms.map(Duration::from_millis)
    }

    /// Sitemap URLs discovered in the file.
    pub fn sitemaps(&self) -> &[String] {
        &self.sitemaps
    }

    /// Number of active rules (for diagnostics).
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// Parse a `robots.txt` body for the given crawler token (e.g. `pretrainscraperbot`).
pub fn parse_robots(content: &str, crawler_token: &str) -> RobotsRules {
    let token = crawler_token.trim().to_ascii_lowercase();
    let mut groups: Vec<Group> = Vec::new();
    let mut sitemaps: Vec<String> = Vec::new();
    let mut current: Option<Group> = None;
    let mut last_was_agent = false;

    for raw_line in content.lines() {
        let line = match raw_line.split_once('#') {
            Some((head, _)) => head,
            None => raw_line,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        match key.as_str() {
            "user-agent" => {
                let agent = value.to_ascii_lowercase();
                if agent.is_empty() {
                    continue;
                }
                if last_was_agent {
                    if let Some(group) = current.as_mut() {
                        group.agents.push(agent);
                    }
                } else {
                    if let Some(group) = current.take() {
                        if !group.agents.is_empty() {
                            groups.push(group);
                        }
                    }
                    current = Some(Group {
                        agents: vec![agent],
                        rules: Vec::new(),
                        crawl_delay_ms: None,
                    });
                }
                last_was_agent = true;
                continue;
            }
            "allow" | "disallow" => {
                last_was_agent = false;
                let group = current.get_or_insert_with(|| Group {
                    agents: vec!["*".to_string()],
                    rules: Vec::new(),
                    crawl_delay_ms: None,
                });
                let pattern = value.to_string();
                if pattern.is_empty() {
                    // `Disallow:` with no value allows everything; skip it.
                    continue;
                }
                group.rules.push(Rule {
                    allow: key == "allow",
                    pattern,
                });
            }
            "crawl-delay" => {
                last_was_agent = false;
                if let Ok(seconds) = value.parse::<f64>() {
                    if seconds.is_finite() && seconds >= 0.0 {
                        let millis = (seconds * 1000.0).round() as u64;
                        let group = current.get_or_insert_with(|| Group {
                            agents: vec!["*".to_string()],
                            rules: Vec::new(),
                            crawl_delay_ms: None,
                        });
                        group.crawl_delay_ms = Some(millis);
                    }
                }
            }
            "sitemap" => {
                last_was_agent = false;
                if !value.is_empty() {
                    sitemaps.push(value.to_string());
                }
            }
            _ => {
                last_was_agent = false;
            }
        }
    }
    if let Some(group) = current {
        if !group.agents.is_empty() {
            groups.push(group);
        }
    }

    // Pick the most specific matching group set, falling back to `*`.
    let mut best_score = 0usize;
    for group in &groups {
        for agent in &group.agents {
            if agent == "*" {
                continue;
            }
            if token.starts_with(agent.as_str()) || agent.starts_with(token.as_str()) {
                best_score = best_score.max(agent.len());
            }
        }
    }
    let mut rules = Vec::new();
    let mut crawl_delay_ms: Option<u64> = None;
    for group in &groups {
        let matches = if best_score > 0 {
            group.agents.iter().any(|agent| {
                agent != "*"
                    && (token.starts_with(agent.as_str()) || agent.starts_with(token.as_str()))
            })
        } else {
            group.agents.iter().any(|agent| agent == "*")
        };
        if !matches {
            continue;
        }
        rules.extend(group.rules.iter().cloned());
        if let Some(delay) = group.crawl_delay_ms {
            crawl_delay_ms = Some(crawl_delay_ms.map_or(delay, |current| current.max(delay)));
        }
    }

    RobotsRules {
        rules,
        crawl_delay_ms,
        sitemaps,
    }
}

/// `robots.txt` pattern matching with `*` wildcards and an optional `$` anchor.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(stripped) => (stripped, true),
        None => (pattern, false),
    };
    if pattern.is_empty() {
        return path.is_empty() && anchored;
    }
    if !pattern.contains('*') {
        return if anchored {
            path == pattern
        } else {
            path.starts_with(pattern)
        };
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut cursor = 0usize;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            if !path[cursor..].starts_with(part) {
                return false;
            }
            cursor += part.len();
        } else {
            match path[cursor..].find(part) {
                Some(found) => cursor += found + part.len(),
                None => return false,
            }
        }
    }
    if anchored {
        cursor == path.len()
    } else {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRobots {
    rules: RobotsRules,
    fetched_at: u64,
}

impl CachedRobots {
    fn is_fresh(&self, ttl_secs: u64) -> bool {
        now_unix().saturating_sub(self.fetched_at) <= ttl_secs
    }
}

/// Memory + database backed robots.txt cache with a configurable TTL.
#[derive(Debug)]
pub struct RobotsCache {
    client: reqwest::Client,
    token: String,
    ttl_secs: u64,
    memory: DashMap<String, CachedRobots>,
    tree: sled::Tree,
}

impl RobotsCache {
    /// Build a cache around the given HTTP client and persistent tree.
    pub fn new(client: reqwest::Client, user_agent: &str, ttl_secs: u64, tree: sled::Tree) -> Self {
        let token = user_agent
            .split_whitespace()
            .next()
            .unwrap_or("bot")
            .split('/')
            .next()
            .unwrap_or("bot")
            .to_ascii_lowercase();
        Self {
            client,
            token,
            ttl_secs,
            memory: DashMap::new(),
            tree,
        }
    }

    /// Rules for the origin of `url`; allows everything when unavailable.
    pub async fn rules_for_url(&self, url: &Url) -> Arc<RobotsRules> {
        let domain = match url.host_str() {
            Some(host) => host.to_ascii_lowercase(),
            None => return Arc::new(RobotsRules::allow_all()),
        };
        let origin = url.origin().ascii_serialization();
        self.rules_for(&origin, &domain).await
    }

    /// Rules for an explicit origin and domain pair.
    pub async fn rules_for(&self, origin: &str, domain: &str) -> Arc<RobotsRules> {
        if let Some(entry) = self.memory.get(domain) {
            if entry.is_fresh(self.ttl_secs) {
                return Arc::new(entry.rules.clone());
            }
        }
        if let Ok(Some(bytes)) = self.tree.get(domain.as_bytes()) {
            if let Ok(cached) = serde_json::from_slice::<CachedRobots>(&bytes) {
                if cached.is_fresh(self.ttl_secs) {
                    let rules = Arc::new(cached.rules.clone());
                    self.memory.insert(domain.to_string(), cached);
                    return rules;
                }
            }
        }

        let rules = match self.download(origin).await {
            Ok(text) => parse_robots(&text, &self.token),
            Err(reason) => {
                tracing::debug!(%origin, %reason, "robots.txt unavailable, allowing all");
                RobotsRules::allow_all()
            }
        };
        let cached = CachedRobots {
            rules: rules.clone(),
            fetched_at: now_unix(),
        };
        match serde_json::to_vec(&cached) {
            Ok(bytes) => {
                if let Err(error) = self.tree.insert(domain.as_bytes(), bytes) {
                    tracing::warn!(%domain, %error, "cannot persist robots.txt rules");
                }
            }
            Err(error) => tracing::warn!(%domain, %error, "cannot encode robots.txt rules"),
        }
        self.memory.insert(domain.to_string(), cached);
        if self.memory.len() > MEMORY_CACHE_LIMIT {
            let ttl = self.ttl_secs;
            self.memory.retain(|_, value| value.is_fresh(ttl));
        }
        Arc::new(rules)
    }

    async fn download(&self, origin: &str) -> std::result::Result<String, String> {
        let url = format!("{}/robots.txt", origin.trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return Ok(String::new());
        }
        if !status.is_success() {
            return Err(format!("status {status}"));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        let slice = if bytes.len() > MAX_ROBOTS_BYTES {
            &bytes[..MAX_ROBOTS_BYTES]
        } else {
            &bytes[..]
        };
        Ok(String::from_utf8_lossy(slice).into_owned())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# global group
User-agent: *
Disallow: /private/
Disallow: /tmp
Allow: /private/public/
Crawl-delay: 2.5

User-agent: PretrainScraperBot
Disallow: /no-bot
Crawl-delay: 7

Sitemap: https://example.com/sitemap.xml
"#;

    #[test]
    fn specific_group_overrides_star_group() {
        let rules = parse_robots(SAMPLE, "pretrainscraperbot");
        assert!(!rules.allowed("/no-bot/page"));
        assert!(rules.allowed("/private/page"), "star rules must not apply");
        assert_eq!(rules.crawl_delay(), Some(Duration::from_millis(7_000)));
        assert_eq!(rules.sitemaps(), ["https://example.com/sitemap.xml"]);
    }

    #[test]
    fn star_group_applies_to_other_crawlers() {
        let rules = parse_robots(SAMPLE, "otherbot");
        assert!(!rules.allowed("/private/page"));
        assert!(!rules.allowed("/tmp/file"));
        assert!(rules.allowed("/private/public/page"), "allow must win");
        assert!(rules.allowed("/blog/post"));
        assert_eq!(rules.crawl_delay(), Some(Duration::from_millis(2_500)));
    }

    #[test]
    fn wildcards_and_end_anchors_are_supported() {
        let rules = parse_robots(
            "User-agent: *\nDisallow: /*.pdf$\nDisallow: /search*\nAllow: /search/public$\n",
            "bot",
        );
        assert!(!rules.allowed("/files/report.pdf"));
        assert!(rules.allowed("/files/report.pdf.html"));
        assert!(!rules.allowed("/search?q=1"));
        assert!(rules.allowed("/search/public"));
    }

    #[test]
    fn missing_file_allows_everything() {
        let rules = parse_robots("", "bot");
        assert!(rules.allowed("/anything"));
        assert!(rules.crawl_delay().is_none());
        assert_eq!(rules.rule_count(), 0);
    }

    #[test]
    fn empty_disallow_means_allow_all() {
        let rules = parse_robots("User-agent: *\nDisallow:\n", "bot");
        assert!(rules.allowed("/page"));
        assert_eq!(rules.rule_count(), 0);
    }

    #[test]
    fn crawl_delay_is_parsed_as_fractional_seconds() {
        let rules = parse_robots("User-agent: *\nCrawl-delay: 0.25\n", "bot");
        assert_eq!(rules.crawl_delay(), Some(Duration::from_millis(250)));
    }
}
