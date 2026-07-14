//! Article classification - multi-tier rule-based system
//!
//! Classification priority (highest to lowest):
//! 1. Feed category (from feed_pool.json)
//! 2. URL pattern matching (sports.*, /tech/, etc.)
//! 3. Domain rules (specific domains → categories)
//! 4. Country-specific categories (북한, 부동산, etc.)
//! 5. Keyword matching (fallback)

use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Classification config loaded from locales/classification.json
#[derive(Debug, Clone, Deserialize)]
pub struct ClassificationConfig {
    pub url_patterns: HashMap<String, Vec<String>>,
    pub domain_rules: HashMap<String, Vec<String>>,
    pub country_categories: HashMap<String, HashMap<String, CountryCategory>>,
    pub category_aliases: HashMap<String, String>,
    pub discovery_defaults: DiscoveryDefaults,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CountryCategory {
    pub name: HashMap<String, String>,
    pub patterns: Vec<String>,
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscoveryDefaults {
    pub categories_per_page: usize,
    pub articles_per_category: usize,
    pub default_categories: Vec<String>,
}

/// Category keywords loaded from locales/category_keywords.json
static CATEGORY_KEYWORDS: OnceLock<HashMap<String, HashMap<String, Vec<String>>>> = OnceLock::new();
static CLASSIFICATION_CONFIG: OnceLock<ClassificationConfig> = OnceLock::new();
static URL_PATTERN_CACHE: OnceLock<HashMap<String, Vec<Regex>>> = OnceLock::new();

fn load_keywords() -> HashMap<String, HashMap<String, Vec<String>>> {
    let path = "locales/category_keywords.json";
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn load_classification_config() -> ClassificationConfig {
    let path = "locales/classification.json";
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| default_config()),
        Err(_) => default_config(),
    }
}

fn default_config() -> ClassificationConfig {
    ClassificationConfig {
        url_patterns: HashMap::new(),
        domain_rules: HashMap::new(),
        country_categories: HashMap::new(),
        category_aliases: HashMap::new(),
        discovery_defaults: DiscoveryDefaults {
            categories_per_page: 6,
            articles_per_category: 8,
            default_categories: vec!["news".into(), "tech".into(), "business".into()],
        },
    }
}

fn get_keywords() -> &'static HashMap<String, HashMap<String, Vec<String>>> {
    CATEGORY_KEYWORDS.get_or_init(load_keywords)
}

pub fn get_config() -> &'static ClassificationConfig {
    CLASSIFICATION_CONFIG.get_or_init(load_classification_config)
}

fn get_url_patterns() -> &'static HashMap<String, Vec<Regex>> {
    URL_PATTERN_CACHE.get_or_init(|| {
        let config = get_config();
        config
            .url_patterns
            .iter()
            .map(|(cat, patterns)| {
                let regexes: Vec<Regex> = patterns
                    .iter()
                    .filter_map(|p| Regex::new(&format!("(?i){}", p)).ok())
                    .collect();
                (cat.clone(), regexes)
            })
            .collect()
    })
}

/// Main classification function with full context
pub struct ClassifyContext<'a> {
    pub title: &'a str,
    pub content: &'a str,
    pub feed_url: &'a str,
    pub article_url: &'a str,
    pub feed_category: Option<&'a str>,
    pub language: Option<&'a str>,
    pub country: Option<&'a str>,
}

/// Classify article using multi-tier system
pub fn classify(ctx: &ClassifyContext) -> String {
    // 1. Use feed category if valid (not "general" or empty)
    if let Some(cat) = ctx.feed_category {
        let normalized = normalize_category(cat);
        if !normalized.is_empty() && normalized != "news" && normalized != "general" {
            return normalized;
        }
    }

    // 2. URL pattern matching (feed URL first, then article URL)
    if let Some(cat) = classify_by_url_pattern(ctx.feed_url) {
        return cat;
    }
    if let Some(cat) = classify_by_url_pattern(ctx.article_url) {
        return cat;
    }

    // 3. Domain rules
    if let Some(cat) = classify_by_domain_rules(ctx.feed_url) {
        return cat;
    }
    if let Some(cat) = classify_by_domain_rules(ctx.article_url) {
        return cat;
    }

    // 4. Country-specific categories
    if let Some(country) = ctx.country {
        if let Some(cat) = classify_country_specific(ctx, country) {
            return cat;
        }
    }

    // 5. Keyword matching (language-aware)
    let lang = ctx.language.unwrap_or("en");
    if let Some(cat) = classify_by_keywords(ctx.title, ctx.content, lang) {
        return cat;
    }

    // Default
    "news".to_string()
}

/// Simplified classification for backward compatibility
pub fn classify_article(title: &str, content: &str, source_url: &str) -> String {
    let ctx = ClassifyContext {
        title,
        content,
        feed_url: source_url,
        article_url: source_url,
        feed_category: None,
        language: None,
        country: None,
    };
    classify(&ctx)
}

/// Classify for a specific language
pub fn classify_article_for_lang(
    title: &str,
    content: &str,
    source_url: &str,
    lang: &str,
) -> String {
    let ctx = ClassifyContext {
        title,
        content,
        feed_url: source_url,
        article_url: source_url,
        feed_category: None,
        language: Some(lang),
        country: None,
    };
    classify(&ctx)
}

/// Normalize category using aliases
pub fn normalize_category(category: &str) -> String {
    let lower = category.to_lowercase();
    let config = get_config();

    config
        .category_aliases
        .get(&lower)
        .cloned()
        .unwrap_or_else(|| lower)
}

/// Classify by URL pattern
fn classify_by_url_pattern(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }

    let patterns = get_url_patterns();
    for (category, regexes) in patterns.iter() {
        for regex in regexes {
            if regex.is_match(url) {
                return Some(category.clone());
            }
        }
    }
    None
}

/// Classify by domain rules
fn classify_by_domain_rules(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }

    let url_lower = url.to_lowercase();
    let config = get_config();

    for (category, domains) in &config.domain_rules {
        for domain in domains {
            if url_lower.contains(domain) {
                return Some(category.clone());
            }
        }
    }
    None
}

/// Classify using country-specific rules
fn classify_country_specific(ctx: &ClassifyContext, country: &str) -> Option<String> {
    let config = get_config();
    let country_cats = config.country_categories.get(country)?;

    let text = format!("{} {} {} {}", ctx.title, ctx.content, ctx.feed_url, ctx.article_url);
    let text_lower = text.to_lowercase();

    for (cat_key, cat_config) in country_cats {
        // Check URL patterns
        for pattern in &cat_config.patterns {
            if text_lower.contains(pattern) {
                return Some(cat_key.clone());
            }
        }

        // Check keywords
        let keyword_matches = cat_config
            .keywords
            .iter()
            .filter(|kw| text.contains(*kw) || text_lower.contains(&kw.to_lowercase()))
            .count();

        if keyword_matches >= 2 {
            return Some(cat_key.clone());
        }
    }
    None
}

/// Classify by keyword matching
fn classify_by_keywords(title: &str, content: &str, lang: &str) -> Option<String> {
    let text = format!("{} {}", title, content).to_lowercase();
    let keywords = get_keywords();
    let lang_code = lang.split('-').next().unwrap_or(lang);

    let mut scores: Vec<(&str, i32)> = keywords
        .iter()
        .map(|(cat, lang_keywords)| {
            let mut score = 0i32;

            // Language-specific keywords (higher weight)
            if let Some(words) = lang_keywords.get(lang_code) {
                score += words
                    .iter()
                    .filter(|w| text.contains(&w.to_lowercase()))
                    .count() as i32
                    * 2;
            }

            // English keywords as fallback
            if lang_code != "en" {
                if let Some(words) = lang_keywords.get("en") {
                    score += words
                        .iter()
                        .filter(|w| text.contains(&w.to_lowercase()))
                        .count() as i32;
                }
            }

            (cat.as_str(), score)
        })
        .collect();

    scores.sort_by(|a, b| b.1.cmp(&a.1));

    if let Some((cat, score)) = scores.first() {
        if *score >= 3 {
            return Some(cat.to_string());
        }
    }
    None
}

/// Check if article matches a category
pub fn matches_category(title: &str, content: &str, category: &str, lang: &str) -> bool {
    if category.is_empty() || category == "general" || category == "news" {
        return true;
    }

    let normalized = normalize_category(category);
    let keywords = get_keywords();
    let lang_code = lang.split('-').next().unwrap_or(lang);

    if let Some(lang_keywords) = keywords.get(&normalized) {
        let text = format!("{} {}", title, content).to_lowercase();

        // Check language-specific keywords
        if let Some(words) = lang_keywords.get(lang_code) {
            if words.iter().any(|w| text.contains(&w.to_lowercase())) {
                return true;
            }
        }

        // Fallback to English
        if let Some(words) = lang_keywords.get("en") {
            if words.iter().any(|w| text.contains(&w.to_lowercase())) {
                return true;
            }
        }
    }

    false
}

/// Get all available categories
pub fn get_categories() -> Vec<&'static str> {
    vec![
        "news",
        "politics",
        "business",
        "finance",
        "tech",
        "world",
        "sports",
        "entertainment",
        "health",
        "science",
        "culture",
        "opinion",
        "lifestyle",
        "society",
    ]
}

/// Get categories for a specific country (including country-specific ones)
pub fn get_categories_for_country(country: &str) -> Vec<String> {
    let mut cats: Vec<String> = get_categories().iter().map(|s| s.to_string()).collect();

    let config = get_config();
    if let Some(country_cats) = config.country_categories.get(country) {
        for cat_key in country_cats.keys() {
            if !cats.contains(cat_key) {
                cats.push(cat_key.clone());
            }
        }
    }

    cats
}

/// Get localized category name
pub fn get_category_name(category: &str, lang: &str) -> String {
    let config = get_config();
    let lang_code = lang.split('-').next().unwrap_or(lang);

    // Check country-specific categories first
    for country_cats in config.country_categories.values() {
        if let Some(cat_config) = country_cats.get(category) {
            if let Some(name) = cat_config.name.get(lang_code) {
                return name.clone();
            }
            if let Some(name) = cat_config.name.get("en") {
                return name.clone();
            }
        }
    }

    // Default: capitalize first letter
    let mut chars = category.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => category.to_string(),
    }
}

/// Get discovery defaults
pub fn get_discovery_defaults() -> &'static DiscoveryDefaults {
    &get_config().discovery_defaults
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_pattern_sports() {
        assert_eq!(
            classify_by_url_pattern("https://sports.chosun.com/news/123"),
            Some("sports".into())
        );
        assert_eq!(
            classify_by_url_pattern("https://example.com/sports/article"),
            Some("sports".into())
        );
    }

    #[test]
    fn test_normalize_category() {
        assert_eq!(normalize_category("TECHNOLOGY"), "tech");
        assert_eq!(normalize_category("economy"), "business");
        assert_eq!(normalize_category("nation"), "politics");
    }

    #[test]
    fn test_classify_with_feed_category() {
        let ctx = ClassifyContext {
            title: "Some article",
            content: "Content here",
            feed_url: "https://example.com/feed",
            article_url: "https://example.com/article",
            feed_category: Some("sports"),
            language: Some("en"),
            country: None,
        };
        assert_eq!(classify(&ctx), "sports");
    }

    #[test]
    fn test_korean_keywords() {
        let ctx = ClassifyContext {
            title: "국회 본회의 통과",
            content: "여야 합의로 법안 통과",
            feed_url: "",
            article_url: "",
            feed_category: None,
            language: Some("ko"),
            country: Some("ko"),
        };
        assert_eq!(classify(&ctx), "politics");
    }
}
