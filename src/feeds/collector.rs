//! Feed collector with rate limiting and domain-based delays.
//!
//! Ported from orgos-core internal/collector/scheduler.go

use super::manager::{FeedManager, ManagedFeed};
use super::parser::parse_feed;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Rate limiter for domains
pub struct RateLimiter {
    /// Domain -> last request time
    last_request: RwLock<HashMap<String, Instant>>,
    /// Domain-specific delays (in milliseconds)
    domain_delays: HashMap<String, u64>,
    /// Default delay between requests to same domain
    default_delay_ms: u64,
}

impl RateLimiter {
    pub fn new() -> Self {
        let mut domain_delays = HashMap::new();
        // Platform-specific rate limits (from orgos-core)
        domain_delays.insert("blog.naver.com".into(), 60_000);    // 1/min
        domain_delays.insert("brunch.co.kr".into(), 60_000);      // 1/min
        domain_delays.insert("youtube.com".into(), 10_000);       // 6/min
        domain_delays.insert("www.youtube.com".into(), 10_000);
        domain_delays.insert("tistory.com".into(), 30_000);       // 2/min
        domain_delays.insert("medium.com".into(), 30_000);

        Self {
            last_request: RwLock::new(HashMap::new()),
            domain_delays,
            default_delay_ms: 200, // 5/sec for general RSS
        }
    }

    /// Check if we can make a request to this domain
    pub fn can_request(&self, domain: &str) -> bool {
        let delay = self.get_delay(domain);
        let last = self.last_request.read().unwrap();

        match last.get(domain) {
            Some(t) => t.elapsed() >= Duration::from_millis(delay),
            None => true,
        }
    }

    /// Wait until we can make a request
    pub async fn wait_for(&self, domain: &str) {
        let delay = Duration::from_millis(self.get_delay(domain));

        loop {
            let should_wait = {
                let last = self.last_request.read().unwrap();
                match last.get(domain) {
                    Some(t) => {
                        let elapsed = t.elapsed();
                        if elapsed < delay {
                            Some(delay - elapsed)
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            };

            match should_wait {
                Some(wait_time) => {
                    tokio::time::sleep(wait_time).await;
                }
                None => break,
            }
        }
    }

    /// Mark that we made a request
    pub fn mark_request(&self, domain: &str) {
        let mut last = self.last_request.write().unwrap();
        last.insert(domain.to_string(), Instant::now());
    }

    fn get_delay(&self, domain: &str) -> u64 {
        // Check for subdomain matches
        for (d, delay) in &self.domain_delays {
            if domain.ends_with(d) {
                return *delay;
            }
        }
        self.default_delay_ms
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Category-based poll intervals
pub struct PollScheduler {
    intervals: HashMap<String, Duration>,
}

impl PollScheduler {
    pub fn new() -> Self {
        let mut intervals = HashMap::new();
        intervals.insert("news".into(), Duration::from_secs(60 * 60));        // 1 hour
        intervals.insert("blog".into(), Duration::from_secs(6 * 60 * 60));    // 6 hours
        intervals.insert("youtube".into(), Duration::from_secs(12 * 60 * 60)); // 12 hours
        intervals.insert("sns".into(), Duration::from_secs(2 * 60 * 60));     // 2 hours

        Self { intervals }
    }

    pub fn interval_for(&self, category: &str) -> Duration {
        self.intervals
            .get(category)
            .copied()
            .unwrap_or(Duration::from_secs(4 * 60 * 60)) // 4 hours default
    }
}

impl Default for PollScheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Feed collector that fetches and parses feeds
pub struct Collector {
    client: reqwest::Client,
    rate_limiter: Arc<RateLimiter>,
    manager: Arc<FeedManager>,
    max_concurrent: usize,
}

impl Collector {
    pub fn new(manager: Arc<FeedManager>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("Metasearch/1.0 RSS Collector")
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            rate_limiter: Arc::new(RateLimiter::new()),
            manager,
            max_concurrent: 20,
        }
    }

    /// Collect due feeds
    pub async fn collect_batch(&self) -> CollectResult {
        let feeds = self.manager.get_due_feeds(self.max_concurrent);
        let mut result = CollectResult::default();

        for feed in feeds {
            match self.fetch_feed(&feed).await {
                Ok(items) => {
                    let count = items.len() as u32;
                    if let Err(e) = self.manager.mark_success(feed.id, count) {
                        eprintln!("Failed to mark success for {}: {}", feed.url, e);
                    }
                    result.success += 1;
                    result.items_added += count as usize;
                }
                Err(e) => {
                    if let Err(e2) = self.manager.mark_failure(feed.id) {
                        eprintln!("Failed to mark failure for {}: {}", feed.url, e2);
                    }
                    result.failed += 1;
                    result.errors.push((feed.url.clone(), e));
                }
            }
        }

        result
    }

    /// Fetch a single feed
    async fn fetch_feed(&self, feed: &ManagedFeed) -> Result<Vec<super::RssItem>, String> {
        let domain = &feed.domain;

        // Wait for rate limit
        self.rate_limiter.wait_for(domain).await;
        self.rate_limiter.mark_request(domain);

        // Fetch
        let resp = self.client
            .get(&feed.url)
            .send()
            .await
            .map_err(|e| format!("Fetch error: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }

        let body = resp.text().await.map_err(|e| format!("Read error: {}", e))?;

        // Parse
        let source_name = feed.title.clone().unwrap_or_else(|| feed.domain.clone());
        let mut items = parse_feed(&body, &source_name);

        // Set language/category from feed
        for item in &mut items {
            item.language = Some(feed.language.clone());
            item.category = Some(feed.category.clone());
        }

        Ok(items)
    }

    /// Run continuous collection loop
    pub async fn run(&self) {
        loop {
            let result = self.collect_batch().await;

            if result.success > 0 || result.failed > 0 {
                eprintln!(
                    "[Collector] batch: {} success, {} failed, {} items",
                    result.success, result.failed, result.items_added
                );
            }

            // Sleep before next batch
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

#[derive(Debug, Default)]
pub struct CollectResult {
    pub success: usize,
    pub failed: usize,
    pub items_added: usize,
    pub errors: Vec<(String, String)>,
}
