//! Standalone RSS feed system for metasearch.
//! Provides built-in news feeds without requiring orgos-core.
//!
//! Architecture (ported from orgos-core):
//! - FeedManager: Feed lifecycle, quality scoring, polling tiers
//! - Collector: Rate-limited RSS fetching
//! - Trending: Entity extraction and rising detection
//! - ArticleStore: SQLite FTS5 storage

mod parser;
pub mod poller;
pub mod store;
pub mod manager;
pub mod collector;
pub mod trending;
pub mod classify;
pub mod embeddings;


pub use parser::RssItem;
pub use poller::{FeedPoller, FeedCache, PollerConfig};
pub use store::ArticleStore;
pub use manager::{FeedManager, FeedStatus, PollTier, ManagedFeed, QualityScore};
pub use collector::{Collector, RateLimiter, CollectResult};
pub use trending::{EntityExtractor, TrendingCalculator, TrendingItem, Entity, EntityType};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Feed entry from pool.json (orgos-core format)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PoolFeed {
    pub url: String,
    #[serde(rename = "type", default)]
    pub feed_type: String,
    #[serde(default)]
    pub lang: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub tier: u8,
    #[serde(default)]
    pub source: String,
}

/// Feed pool configuration (orgos-core format)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeedPool {
    pub version: String,
    pub count: usize,
    pub feeds: Vec<PoolFeed>,
}

impl FeedPool {
    /// Load feed pool from embedded JSON
    pub fn load_embedded() -> Result<Self, serde_json::Error> {
        let json = include_str!("../../static/feed_pool.json");
        serde_json::from_str(json)
    }

    /// Get feeds for a specific language
    pub fn get_by_lang(&self, lang: &str) -> Vec<&PoolFeed> {
        self.feeds.iter().filter(|f| f.lang == lang).collect()
    }

    /// Get feeds for a specific country
    pub fn get_by_country(&self, country: &str) -> Vec<&PoolFeed> {
        self.feeds.iter().filter(|f| f.country == country.to_lowercase()).collect()
    }

    /// Get all unique languages
    pub fn languages(&self) -> Vec<String> {
        let mut langs: Vec<_> = self.feeds.iter()
            .map(|f| f.lang.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        langs.sort();
        langs
    }

    /// Get tier 1-2 feeds (high quality) for a language
    pub fn get_priority_feeds(&self, lang: &str) -> Vec<&PoolFeed> {
        self.feeds.iter()
            .filter(|f| f.lang == lang && f.tier <= 2)
            .collect()
    }
}

/// Feed source configuration (legacy format)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeedSource {
    pub name: String,
    pub url: String,
    pub category: String,
}

/// Language feed configuration (legacy format)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LanguageFeeds {
    pub name: String,
    pub sources: Vec<FeedSource>,
}

/// Root feeds configuration (legacy format)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeedsConfig {
    pub meta: FeedsMeta,
    pub feeds: HashMap<String, LanguageFeeds>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeedsMeta {
    pub version: String,
    pub description: String,
    pub total_languages: u32,
}

impl FeedsConfig {
    /// Load feeds from embedded JSON
    pub fn load_embedded() -> Result<Self, serde_json::Error> {
        let json = include_str!("../../static/feeds.json");
        serde_json::from_str(json)
    }

    /// Get feeds for a specific language
    pub fn get_language(&self, lang: &str) -> Option<&LanguageFeeds> {
        self.feeds.get(lang)
    }

    /// Get all feed URLs for a language
    pub fn get_urls(&self, lang: &str) -> Vec<&str> {
        self.feeds.get(lang)
            .map(|lf| lf.sources.iter().map(|s| s.url.as_str()).collect())
            .unwrap_or_default()
    }
}
