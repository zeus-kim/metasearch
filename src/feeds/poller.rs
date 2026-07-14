//! Background RSS feed poller with caching and retention

use super::RssItem;
use super::parser::parse_feed;
use super::store::ArticleStore;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default retention period in days
const DEFAULT_RETENTION_DAYS: u32 = 7;
/// Default poll interval in minutes
const DEFAULT_POLL_INTERVAL_MINS: u64 = 15;
/// Maximum items per feed to keep
const MAX_ITEMS_PER_FEED: usize = 100;

/// Feed cache entry
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CacheEntry {
    items: Vec<RssItem>,
    fetched_at: Instant,
    last_error: Option<String>,
}

/// Feed poller configuration
#[derive(Debug, Clone)]
pub struct PollerConfig {
    /// Days to retain articles (default: 7)
    pub retention_days: u32,
    /// Poll interval in minutes (default: 15)
    pub poll_interval_mins: u64,
    /// HTTP timeout in seconds
    pub timeout_secs: u64,
    /// User agent for requests
    pub user_agent: String,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            poll_interval_mins: DEFAULT_POLL_INTERVAL_MINS,
            timeout_secs: 30,
            user_agent: "Metasearch/1.0 RSS Reader".to_string(),
        }
    }
}

/// Feed entry from registry
#[derive(Debug, Clone, serde::Deserialize)]
struct RegistryFeed {
    lang: String,
    url: String,
    #[serde(rename = "type", default)]
    feed_type: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    tier: u8,
    #[serde(default)]
    category: String,
}

/// Loaded registry
static FEED_REGISTRY: std::sync::OnceLock<Vec<RegistryFeed>> = std::sync::OnceLock::new();

fn load_registry() -> &'static Vec<RegistryFeed> {
    FEED_REGISTRY.get_or_init(|| {
        let mut feeds = Vec::new();

        // Load tier 1 major news feeds FIRST (highest priority)
        let major_data = include_str!("../../static/major_news_feeds.jsonl");
        let mut major_count = 0;
        for line in major_data.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Ok(feed) = serde_json::from_str::<RegistryFeed>(line) {
                if feed.tier == 1 {
                    major_count += 1;
                }
                feeds.push(feed);
            }
        }

        // Load from feed_pool.json (has category info)
        let pool_data = include_str!("../../static/feed_pool.json");
        if let Ok(pool) = serde_json::from_str::<serde_json::Value>(pool_data) {
            if let Some(pool_feeds) = pool.get("feeds").and_then(|f| f.as_array()) {
                for feed in pool_feeds {
                    if let Ok(rf) = serde_json::from_value::<RegistryFeed>(feed.clone()) {
                        feeds.push(rf);
                    }
                }
            }
        }
        let pool_count = feeds.len() - major_count;

        // Then load from feeds_registry.jsonl
        let registry_data = include_str!("../../static/feeds_registry.jsonl");
        for line in registry_data.lines() {
            if let Ok(feed) = serde_json::from_str::<RegistryFeed>(line) {
                feeds.push(feed);
            }
        }

        // Deduplicate by URL - first occurrence wins (major feeds have priority)
        let mut seen = std::collections::HashSet::new();
        feeds.retain(|f| seen.insert(f.url.clone()));

        eprintln!("[Poller] Loaded {} feeds (tier1: {}, pool: {}, registry: rest)", feeds.len(), major_count, pool_count);
        feeds
    })
}

/// Aggregated feed cache with SQLite persistence
pub struct FeedCache {
    /// Per-feed cache: url -> CacheEntry (hot cache)
    cache: RwLock<HashMap<String, CacheEntry>>,
    /// Poller settings
    settings: PollerConfig,
    /// HTTP client
    client: reqwest::Client,
    /// SQLite store for persistence
    store: Option<ArticleStore>,
}

impl FeedCache {
    /// Create new feed cache with SQLite persistence
    pub fn new(settings: PollerConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(settings.timeout_secs))
            .user_agent(&settings.user_agent)
            .build()?;

        // Open SQLite database for persistent storage
        let db_path = std::env::var("METASEARCH_DATA_DIR")
            .unwrap_or_else(|_| "data".to_string());
        std::fs::create_dir_all(&db_path).ok();
        let store = ArticleStore::open(
            format!("{}/articles.db", db_path),
            settings.retention_days
        ).ok();

        if store.is_some() {
            eprintln!("[RSS] SQLite store opened: {}/articles.db", db_path);
            if let Some(ref s) = store {
                if let Ok(stats) = s.stats() {
                    eprintln!("[RSS] Existing articles: {}", stats.total_articles);
                }
            }
        }

        Ok(Self {
            cache: RwLock::new(HashMap::new()),
            settings,
            client,
            store,
        })
    }

    /// Get feeds from registry for a language
    fn get_feeds_for_lang(&self, lang: &str, limit: usize) -> Vec<&'static RegistryFeed> {
        let registry = load_registry();
        registry.iter()
            .filter(|f| f.lang == lang)
            .take(limit)
            .collect()
    }

    /// Get all feeds from registry (no language filter)
    fn get_all_feeds(&self, limit: usize) -> Vec<&'static RegistryFeed> {
        let registry = load_registry();
        registry.iter().take(limit).collect()
    }

    /// Get feeds from registry with offset and limit
    fn get_feeds_range(&self, offset: usize, limit: usize) -> Vec<&'static RegistryFeed> {
        let registry = load_registry();
        registry.iter().skip(offset).take(limit).collect()
    }

    /// Bulk fetch feeds in a range - saves to DB immediately
    pub async fn bulk_fetch_range(&self, offset: usize, limit: usize, workers: usize, cycle: u64) {
        let feeds = self.get_feeds_range(offset, limit);
        if feeds.is_empty() {
            return;
        }
        self.bulk_fetch_impl(&feeds, workers, cycle).await;
    }

    /// Bulk fetch all feeds (core-style) - saves to DB immediately per feed
    pub async fn bulk_fetch(&self, limit: usize, workers: usize) {
        let feeds = self.get_all_feeds(limit);
        if feeds.is_empty() {
            return;
        }
        self.bulk_fetch_impl(&feeds, workers, 0).await;
    }

    /// Internal implementation for bulk fetching with quality tracking
    async fn bulk_fetch_impl(&self, feeds: &[&'static RegistryFeed], workers: usize, cycle: u64) {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(workers));
        let client = self.client.clone();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let success = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let skipped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let db_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Share store reference for immediate DB writes
        let store_ref: Option<&ArticleStore> = self.store.as_ref();

        // Filter feeds based on quality
        let active_feeds: Vec<_> = feeds.iter().filter(|f| {
            if let Some(store) = store_ref {
                store.should_poll_feed(&f.url, cycle)
            } else {
                true
            }
        }).collect();

        let total = feeds.len();
        let active = active_feeds.len();
        let skip_count = total - active;

        if skip_count > 0 {
            eprintln!("[RSS] Bulk fetching {} feeds ({} skipped low-quality) with {} workers...", active, skip_count, workers);
        } else {
            eprintln!("[RSS] Bulk fetching {} feeds with {} workers...", active, workers);
        }

        let futures: Vec<_> = active_feeds.iter().map(|source| {
            let sem = semaphore.clone();
            let client = client.clone();
            let url = source.url.clone();
            let source_lang = source.lang.clone();
            let source_type = if source.feed_type.is_empty() { "news".to_string() } else { source.feed_type.clone() };
            let source_country = source.country.clone();
            let source_tier = source.tier;
            let counter = counter.clone();
            let success_counter = success.clone();
            async move {
                let _permit = sem.acquire().await.ok();
                let source_name = url.split('/').nth(2).unwrap_or("RSS").to_string();
                let result = client.get(&url).send().await;

                let (entry, is_success) = match result {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            (CacheEntry {
                                items: vec![],
                                fetched_at: Instant::now(),
                                last_error: Some(format!("HTTP {}", resp.status())),
                            }, false)
                        } else {
                            match resp.text().await {
                                Ok(body) => {
                                    let mut items = parse_feed(&body, &source_name);
                                    for item in &mut items {
                                        // Multi-tier classification with full context
                                        let ctx = crate::feeds::classify::ClassifyContext {
                                            title: &item.title,
                                            content: &item.description,
                                            feed_url: &url,
                                            article_url: &item.url,
                                            feed_category: if source.category.is_empty() { None } else { Some(&source.category) },
                                            language: Some(&source_lang),
                                            country: if source_country.is_empty() { None } else { Some(&source_country) },
                                        };
                                        let cat = crate::feeds::classify::classify(&ctx);
                                        item.category = Some(cat.clone());
                                        item.language = Some(source_lang.clone());
                                        item.feed_type = Some(source_type.clone());
                                        item.country = if source_country.is_empty() { None } else { Some(source_country.clone()) };
                                        item.tier = source_tier;
                                        // normalized_category: feed_type:category (e.g., "blog:tech", "youtube:science")
                                        item.normalized_category = Some(format!("{}:{}", source_type, cat));
                                    }
                                    items.truncate(MAX_ITEMS_PER_FEED);
                                    let has_items = !items.is_empty();
                                    if has_items {
                                        success_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                    (CacheEntry {
                                        items,
                                        fetched_at: Instant::now(),
                                        last_error: None,
                                    }, has_items)
                                }
                                Err(e) => (CacheEntry {
                                    items: vec![],
                                    fetched_at: Instant::now(),
                                    last_error: Some(format!("Parse: {}", e)),
                                }, false),
                            }
                        }
                    }
                    Err(e) => (CacheEntry {
                        items: vec![],
                        fetched_at: Instant::now(),
                        last_error: Some(format!("Fetch: {}", e)),
                    }, false),
                };

                let count = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if count % 500 == 0 {
                    let ok = success_counter.load(std::sync::atomic::Ordering::Relaxed);
                    eprintln!("[RSS] Progress: {}/{} feeds ({} success)", count, active, ok);
                }

                (url, entry, is_success)
            }
        }).collect();

        // Process all futures in parallel
        let results = futures_util::future::join_all(futures).await;

        // Save to DB, update cache, and record quality
        for (url, entry, is_success) in results {
            // Record feed quality
            if let Some(store) = store_ref {
                if is_success {
                    let _ = store.record_feed_success(&url, entry.items.len());
                } else if let Some(ref err) = entry.last_error {
                    let _ = store.record_feed_failure(&url, err);
                }
            }

            // Save articles
            if !entry.items.is_empty() {
                if let Some(store) = store_ref {
                    if let Ok(inserted) = store.insert_articles(&entry.items) {
                        db_counter.fetch_add(inserted, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }

            if let Ok(mut cache) = self.cache.write() {
                cache.insert(url, entry);
            }
        }

        let ok = success.load(std::sync::atomic::Ordering::Relaxed);
        let db_total = self.store.as_ref()
            .and_then(|s| s.stats().ok())
            .map(|s| s.total_articles)
            .unwrap_or(0);
        let db_new = db_counter.load(std::sync::atomic::Ordering::Relaxed);

        // Log quality stats periodically
        if cycle % 10 == 0 {
            if let Some(store) = store_ref {
                if let Ok(qstats) = store.feed_quality_stats() {
                    eprintln!("[RSS] Quality: {} active, {} degraded, {} disabled, avg={:.1}%",
                        qstats.active, qstats.degraded, qstats.disabled, qstats.avg_quality);
                }
            }
        }

        eprintln!("[RSS] Bulk fetch done: {}/{} feeds, +{} new articles, DB total: {}", ok, active, db_new, db_total);
    }

    /// Get items - prioritize SQLite, fallback to memory cache
    pub async fn get_items(&self, lang: &str, category: Option<&str>) -> Vec<RssItem> {
        let lang_code = lang.split('-').next().unwrap_or(lang);

        // Try SQLite first (persistent storage)
        if let Some(ref store) = self.store {
            if let Ok(items) = store.recent(Some(lang_code), category, 500) {
                if !items.is_empty() {
                    return items;
                }
            }
        }

        // Fallback to memory cache
        let mut all_items = Vec::new();
        let retention_secs = (self.settings.retention_days as i64) * 24 * 60 * 60;
        let now_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if let Ok(cache) = self.cache.read() {
            for entry in cache.values() {
                for item in &entry.items {
                    // Filter by language (compare 2-letter code)
                    if let Some(item_lang) = &item.language {
                        if item_lang != lang_code {
                            continue;
                        }
                    }
                    // Filter by retention period
                    if let Some(pub_ts) = item.published {
                        if now_ts - pub_ts > retention_secs {
                            continue;
                        }
                    }
                    // Filter by category if specified
                    if let Some(cat) = category {
                        if !cat.is_empty() && cat != "general" && cat != "news" {
                            let item_cat = item.category.as_deref().unwrap_or("general");
                            if item_cat != cat {
                                continue;
                            }
                        }
                    }
                    all_items.push(item.clone());
                }
            }
        }

        // Sort by publish date (newest first)
        all_items.sort_by(|a, b| {
            b.published.unwrap_or(0).cmp(&a.published.unwrap_or(0))
        });

        // Deduplicate by URL
        let mut seen = std::collections::HashSet::new();
        all_items.retain(|item| seen.insert(item.url.clone()));

        all_items
    }

    /// Get items directly from DB (bypasses cache, uses language+category filter)
    /// Used for Latin-script languages where title-based filtering doesn't work
    pub async fn get_items_from_db(&self, lang: &str, category: Option<&str>, limit: usize) -> Vec<RssItem> {
        self.get_items_from_db_with_country(lang, category, None, limit).await
    }

    /// Get items with optional country filter (ISO 3166-1 alpha-2 code)
    pub async fn get_items_from_db_with_country(&self, lang: &str, category: Option<&str>, country: Option<&str>, limit: usize) -> Vec<RssItem> {
        if let Some(store) = &self.store {
            // Map UI category to DB category
            let db_category = category.and_then(|c| match c {
                "all" | "news" | "" => None,
                "economy" => Some("business"),
                "art" => Some("culture"),
                other => Some(other),
            });

            match store.recent_with_country(Some(lang), db_category, country, limit) {
                Ok(items) => items,
                Err(e) => {
                    eprintln!("[FeedCache] DB query error: {}", e);
                    vec![]
                }
            }
        } else {
            vec![]
        }
    }

    /// Fetch a single feed from registry entry and update cache
    #[allow(dead_code)]
    async fn fetch_registry_feed(&self, source: &RegistryFeed) {
        let source_name = source.url.split('/').nth(2).unwrap_or("RSS").to_string();

        let result = self.client.get(&source.url).send().await;

        let entry = match result {
            Ok(resp) => {
                match resp.text().await {
                    Ok(body) => {
                        let mut items = parse_feed(&body, &source_name);
                        for item in &mut items {
                            item.category = Some("news".into());
                            item.language = Some(source.lang.clone());
                        }
                        items.truncate(MAX_ITEMS_PER_FEED);

                        // Note: og:image fetching moved to background task for speed
                        // Images are enriched via news_digest enrich_with_og_images

                        CacheEntry {
                            items,
                            fetched_at: Instant::now(),
                            last_error: None,
                        }
                    }
                    Err(e) => CacheEntry {
                        items: vec![],
                        fetched_at: Instant::now(),
                        last_error: Some(format!("Parse error: {}", e)),
                    },
                }
            }
            Err(e) => {
                let old_items = self.cache.read().ok()
                    .and_then(|c| c.get(&source.url).map(|e| e.items.clone()))
                    .unwrap_or_default();
                CacheEntry {
                    items: old_items,
                    fetched_at: Instant::now(),
                    last_error: Some(format!("Fetch error: {}", e)),
                }
            }
        };

        if let Ok(mut cache) = self.cache.write() {
            cache.insert(source.url.clone(), entry);
        }
    }

    /// Clean up old articles beyond retention period
    pub fn cleanup_old_articles(&self) {
        let retention_secs = (self.settings.retention_days as i64) * 24 * 60 * 60;
        let now_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if let Ok(mut cache) = self.cache.write() {
            for entry in cache.values_mut() {
                entry.items.retain(|item| {
                    item.published
                        .map(|ts| now_ts - ts <= retention_secs)
                        .unwrap_or(true)
                });
            }
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> FeedCacheStats {
        let cache = self.cache.read().unwrap();
        let total_feeds = cache.len();
        let total_items: usize = cache.values().map(|e| e.items.len()).sum();
        let errors: Vec<_> = cache.iter()
            .filter_map(|(url, e)| e.last_error.as_ref().map(|err| (url.clone(), err.clone())))
            .collect();

        FeedCacheStats {
            total_feeds,
            total_items,
            errors,
            languages: self.languages().len(),
        }
    }

    /// Get available languages from registry
    pub fn languages(&self) -> Vec<&'static str> {
        let registry = load_registry();
        let mut langs: Vec<&str> = registry.iter()
            .map(|f| f.lang.as_str())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        langs.sort();
        langs
    }

    /// Search items by query - uses FTS5 when available
    pub async fn search(&self, query: &str, lang: &str, limit: usize) -> Vec<RssItem> {
        let lang_code = lang.split('-').next().unwrap_or(lang);

        // Try SQLite FTS5 first (much faster)
        if let Some(ref store) = self.store {
            if let Ok(items) = store.search(query, Some(lang_code), limit) {
                if !items.is_empty() {
                    return items;
                }
            }
        }

        // Fallback to memory search
        let items = self.get_items(lang, None).await;
        let query_lower = query.to_lowercase();
        let words: Vec<&str> = query_lower.split_whitespace().collect();

        items.into_iter()
            .filter(|item| {
                if words.is_empty() {
                    return true;
                }
                let title_lower = item.title.to_lowercase();
                let content_lower = item.description.to_lowercase();
                words.iter().any(|w| title_lower.contains(w) || content_lower.contains(w))
            })
            .take(limit)
            .collect()
    }

    /// Prefetch feeds for a language (background warmup) - parallel fetch
    pub async fn prefetch(&self, lang: &str) {
        let sources = self.get_feeds_for_lang(lang, 100);
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(20)); // 20 concurrent

        let futures: Vec<_> = sources.iter().map(|source| {
            let sem = semaphore.clone();
            let client = self.client.clone();
            let url = source.url.clone();
            let source_lang = source.lang.clone();
            let source_cat = source.category.clone();
            let source_country = source.country.clone();
            let source_tier = source.tier;

            async move {
                let _permit = sem.acquire().await.ok();
                let source_name = url.split('/').nth(2).unwrap_or("RSS").to_string();
                let result = client.get(&url).send().await;

                let entry = match result {
                    Ok(resp) => {
                        match resp.text().await {
                            Ok(body) => {
                                let mut items = parse_feed(&body, &source_name);
                                for item in &mut items {
                                    let ctx = crate::feeds::classify::ClassifyContext {
                                        title: &item.title,
                                        content: &item.description,
                                        feed_url: &url,
                                        article_url: &item.url,
                                        feed_category: if source_cat.is_empty() { None } else { Some(&source_cat) },
                                        language: Some(&source_lang),
                                        country: if source_country.is_empty() { None } else { Some(&source_country) },
                                    };
                                    let cat = crate::feeds::classify::classify(&ctx);
                                    item.category = Some(cat);
                                    item.language = Some(source_lang.clone());
                                    item.tier = source_tier;
                                }
                                items.truncate(MAX_ITEMS_PER_FEED);
                                CacheEntry {
                                    items,
                                    fetched_at: Instant::now(),
                                    last_error: None,
                                }
                            }
                            Err(e) => CacheEntry {
                                items: vec![],
                                fetched_at: Instant::now(),
                                last_error: Some(format!("Parse: {}", e)),
                            },
                        }
                    }
                    Err(e) => CacheEntry {
                        items: vec![],
                        fetched_at: Instant::now(),
                        last_error: Some(format!("Fetch: {}", e)),
                    },
                };
                (url, entry)
            }
        }).collect();

        let results = futures_util::future::join_all(futures).await;

        if let Ok(mut cache) = self.cache.write() {
            for (url, entry) in results {
                cache.insert(url, entry);
            }
        }
    }

    /// Fast prefetch with limited concurrent fetches (for initial warmup)
    pub async fn prefetch_fast(&self, lang: &str, limit: usize) {
        let sources = self.get_feeds_for_lang(lang, limit);
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(10));

        let futures: Vec<_> = sources.iter().take(limit).map(|source| {
            let sem = semaphore.clone();
            let client = self.client.clone();
            let url = source.url.clone();
            let source_lang = source.lang.clone();
            let source_cat = source.category.clone();
            let source_country = source.country.clone();
            let source_tier = source.tier;

            async move {
                let _permit = sem.acquire().await.ok();
                let source_name = url.split('/').nth(2).unwrap_or("RSS").to_string();
                let result = client.get(&url).send().await;

                let entry = match result {
                    Ok(resp) => {
                        match resp.text().await {
                            Ok(body) => {
                                let mut items = parse_feed(&body, &source_name);
                                for item in &mut items {
                                    let ctx = crate::feeds::classify::ClassifyContext {
                                        title: &item.title,
                                        content: &item.description,
                                        feed_url: &url,
                                        article_url: &item.url,
                                        feed_category: if source_cat.is_empty() { None } else { Some(&source_cat) },
                                        language: Some(&source_lang),
                                        country: if source_country.is_empty() { None } else { Some(&source_country) },
                                    };
                                    let cat = crate::feeds::classify::classify(&ctx);
                                    item.category = Some(cat);
                                    item.language = Some(source_lang.clone());
                                    item.tier = source_tier;
                                }
                                items.truncate(MAX_ITEMS_PER_FEED);
                                CacheEntry {
                                    items,
                                    fetched_at: Instant::now(),
                                    last_error: None,
                                }
                            }
                            Err(_) => CacheEntry {
                                items: vec![],
                                fetched_at: Instant::now(),
                                last_error: Some("parse".into()),
                            },
                        }
                    }
                    Err(_) => CacheEntry {
                        items: vec![],
                        fetched_at: Instant::now(),
                        last_error: Some("fetch".into()),
                    },
                };
                (url, entry)
            }
        }).collect();

        let results = futures_util::future::join_all(futures).await;

        if let Ok(mut cache) = self.cache.write() {
            for (url, entry) in results {
                cache.insert(url, entry);
            }
        }
    }

    /// Fetch og:image from article page
    #[allow(dead_code)]
    async fn fetch_og_image(&self, url: &str) -> Result<String, ()> {
        use crate::feeds::parser::extract_og_image;

        let resp = self.client
            .get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| ())?;

        if !resp.status().is_success() {
            return Err(());
        }

        // Only read first 50KB to find og:image quickly
        let body = resp.text().await.map_err(|_| ())?;
        let head = if body.len() > 50000 {
            // Find a valid UTF-8 boundary
            let mut end = 50000;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            &body[..end]
        } else {
            &body
        };

        extract_og_image(head).ok_or(())
    }
}

/// Feed cache statistics
#[derive(Debug, Clone)]
pub struct FeedCacheStats {
    pub total_feeds: usize,
    pub total_items: usize,
    pub errors: Vec<(String, String)>,
    pub languages: usize,
}

/// Background feed poller task
pub struct FeedPoller {
    cache: Arc<FeedCache>,
    #[allow(dead_code)]
    languages: Vec<String>,
    cycle: std::sync::atomic::AtomicU64,
}

impl FeedPoller {
    /// Create new poller with specific languages to monitor
    pub fn new(cache: Arc<FeedCache>, languages: Vec<String>) -> Self {
        Self {
            cache,
            languages,
            cycle: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Run background polling loop - core style continuous cycling
    pub async fn run(&self) {
        let cleanup_interval = Duration::from_secs(3600);
        let mut last_cleanup = Instant::now();

        let total_feeds = load_registry().len();
        let batch_size = 500;   // 배치당 500개 (CPU 부하 감소)
        let workers = 10;       // 동시 워커 10개
        let mut offset = 0;

        eprintln!("[RSS] Starting continuous feed polling: {} feeds, {}개씩 배치 (with quality filtering)", total_feeds, batch_size);

        // 30초마다 배치 처리 (서버 응답성 유지)
        let mut interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            interval.tick().await;

            let start = Instant::now();
            let cycle = self.cycle.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // 현재 offset부터 batch_size만큼 fetch (품질 필터링 적용)
            self.cache.bulk_fetch_range(offset, batch_size, workers, cycle).await;

            // 다음 offset 계산 (순환)
            offset += batch_size;
            if offset >= total_feeds {
                offset = 0;
                eprintln!("[RSS] Full cycle complete, restarting from beginning");
            }

            let db_total = self.cache.store.as_ref()
                .and_then(|s| s.stats().ok())
                .map(|s| s.total_articles)
                .unwrap_or(0);

            eprintln!("[RSS] Batch done: offset={}/{}, DB total={}, took {}ms",
                offset, total_feeds, db_total, start.elapsed().as_millis());

            // 정기 정리
            if last_cleanup.elapsed() > cleanup_interval {
                self.cache.cleanup_old_articles();
                last_cleanup = Instant::now();
                eprintln!("[RSS] Cleanup complete");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = PollerConfig::default();
        assert_eq!(config.retention_days, 7);
        assert_eq!(config.poll_interval_mins, 15);
    }
}
