//! Local RSS feeds engine - searches articles from the built-in feed system.
//!
//! This engine queries the RSS feed cache populated by the feed poller,
//! returning news articles without external dependencies.

use crate::feeds::FeedCache;
use crate::types::EngineResult;
use super::EngineContext;
use std::sync::{Arc, OnceLock};

static FEED_CACHE: OnceLock<Arc<FeedCache>> = OnceLock::new();

/// Set the global feed cache (called from server initialization)
pub fn set_feed_cache(cache: Arc<FeedCache>) {
    let result = FEED_CACHE.set(cache);
    eprintln!("[local_feeds] set_feed_cache called, success={}", result.is_ok());
}

fn get_feed_cache() -> Option<Arc<FeedCache>> {
    FEED_CACHE.get().cloned()
}

/// Public getter for feed cache (for use by other modules like trending)
pub fn get_feed_cache_public() -> Option<Arc<FeedCache>> {
    FEED_CACHE.get().cloned()
}

/// Search local RSS feeds
pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let cache = match get_feed_cache() {
        Some(c) => c,
        None => {
            eprintln!("[local_feeds] ERROR: No cache available");
            return Ok(vec![]);
        }
    };
    let lang = ctx.lang_code();
    let query = ctx.query.trim();
    let query_lower = query.to_lowercase();
    let is_generic = is_generic_news_query(&query_lower) || query.is_empty();

    // Use DB directly with language + category + country filter for accurate results
    let is_social = ctx.category.map(|c| c == "social").unwrap_or(false);
    let category = ctx.category.filter(|c| !c.is_empty() && *c != "all" && *c != "news" && *c != "social");
    let country = ctx.country;
    eprintln!("[local_feeds] DB query for lang={}, category={:?}, country={:?}, query={}, is_social={}", lang, category, country, query, is_social);

    let filtered: Vec<_> = if is_generic || category.is_some() || is_social || country.is_some() {
        // Generic query or category/country browsing: get from DB with filters
        let items = cache.get_items_from_db_with_country(lang, category, country, ctx.max_results * 5).await;
        eprintln!("[local_feeds] get_items_from_db returned {} items", items.len());
        let items: Vec<_> = if is_social {
            // For social category, filter to blog sources only
            items.into_iter().filter(|item| {
                let source = item.source.to_lowercase();
                source.contains("blog") || source.contains("tistory")
                    || source.contains("brunch") || source.contains("medium")
                    || source.contains("substack") || source.contains("velog")
            }).collect()
        } else {
            items
        };
        items.into_iter().take(ctx.max_results * 3).collect()
    } else {
        // Specific query without category: use FTS search
        let items = cache.search(query, lang, ctx.max_results * 5).await;
        eprintln!("[local_feeds] FTS search returned {} items", items.len());
        items
    };

    Ok(filtered.into_iter().map(|item| {
        // Extract specific category from normalized_category (e.g., "news:culture" → "culture")
        // Fall back to item.category if it's specific (not "news" or "general")
        let category = item.normalized_category.as_ref()
            .and_then(|nc| nc.split(':').nth(1))
            .map(|s| s.to_string())
            .or_else(|| {
                item.category.clone().filter(|c| !c.is_empty() && c != "news" && c != "general")
            })
            .unwrap_or_else(|| "news".to_string());

        EngineResult {
            url: item.url,
            title: item.title,
            content: item.description,
            img_src: item.thumbnail.clone(),
            thumbnail: item.thumbnail,
            published_date: item.published.map(|ts| format_timestamp(ts)),
            template: Some("news.html".into()),
            category: Some(category),
            priority: None,
            publisher_url: None,
            language: item.language.clone(),
        }
    }).collect())
}

/// Check if title matches the expected language
fn matches_language(title: &str, lang: &str) -> bool {
    match lang {
        "ko" => title.chars().any(|c| ('\u{AC00}'..='\u{D7AF}').contains(&c)),
        "ja" => title.chars().any(|c|
            ('\u{3040}'..='\u{309F}').contains(&c) || // Hiragana
            ('\u{30A0}'..='\u{30FF}').contains(&c) || // Katakana
            ('\u{4E00}'..='\u{9FFF}').contains(&c)),  // CJK (kanji)
        "zh" => title.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
        "ar" => title.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
        "he" => title.chars().any(|c| ('\u{0590}'..='\u{05FF}').contains(&c)),
        "ru" | "uk" => title.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)),
        "th" => title.chars().any(|c| ('\u{0E00}'..='\u{0E7F}').contains(&c)),
        "hi" | "mr" => title.chars().any(|c| ('\u{0900}'..='\u{097F}').contains(&c)), // Devanagari (Hindi, Marathi)
        "bn" => title.chars().any(|c| ('\u{0980}'..='\u{09FF}').contains(&c)), // Bengali
        "te" => title.chars().any(|c| ('\u{0C00}'..='\u{0C7F}').contains(&c)), // Telugu
        "ta" => title.chars().any(|c| ('\u{0B80}'..='\u{0BFF}').contains(&c)), // Tamil
        "ml" => title.chars().any(|c| ('\u{0D00}'..='\u{0D7F}').contains(&c)), // Malayalam
        "kn" => title.chars().any(|c| ('\u{0C80}'..='\u{0CFF}').contains(&c)), // Kannada
        "gu" => title.chars().any(|c| ('\u{0A80}'..='\u{0AFF}').contains(&c)), // Gujarati
        "pa" => title.chars().any(|c| ('\u{0A00}'..='\u{0A7F}').contains(&c)), // Punjabi (Gurmukhi)
        "el" => title.chars().any(|c| ('\u{0370}'..='\u{03FF}').contains(&c)),
        // For Latin-script languages, accept any ASCII text (can't distinguish easily)
        _ => title.chars().any(|c| c.is_alphabetic()),
    }
}

/// Check if query is a generic news request (e.g., "news", "뉴스", "latest", "discover:*")
fn is_generic_news_query(query: &str) -> bool {
    let q = query.to_lowercase();
    // Discover queries are always generic (we show latest news for the category)
    if q.starts_with("discover:") {
        return true;
    }
    // Generic news terms in various languages - use DB browse with tier boost instead of FTS
    let generic_terms = [
        "news", "latest", "latest news", "breaking news", "top stories",
        "뉴스", "최신", "최신 뉴스", "속보", "뉴스 속보",
        "ニュース", "最新ニュース", "速報", "最新",
        "新闻", "最新新闻", "头条",
        "nachrichten", "actualités", "noticias", "nieuws", "nyheter", "actualidad",
        "notícias", "новости", "tin tức", "ข่าว", "berita",
    ];
    generic_terms.contains(&q.as_str())
}

/// Search local feeds for news category specifically
pub async fn search_news(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search(ctx).await
}

fn format_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let days = ts / 86400;
    let secs = ts % 86400;
    let (y, m, d) = civil_from_days(days);
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
