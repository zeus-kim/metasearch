//! Brave Search via the official Brave Search API — OPT-IN and key-based.
//!
//! Disabled by default. Requires a subscription token supplied via config
//! (`api_key`) or, preferably, the `BRAVE_API_KEY` environment variable (never
//! hardcoded). The token is sent in the `X-Subscription-Token` header and is
//! never logged. With no key the engine fails gracefully (a recoverable error
//! that marks it unresponsive without breaking the search).
//!
//! This is the JSON-API counterpart to the keyless `brave` HTML scraper: the
//! API is far more reliable (no bot-blocking) but needs a key, so it ships as a
//! separate opt-in engine rather than replacing the scraper.

use serde_json::Value;

use super::{strip_html, EngineContext};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let key = ctx
        .api_key
        .filter(|k| !k.is_empty())
        .ok_or("brave_api engine needs an API key (set BRAVE_API_KEY)")?;

    // Free tier caps `count` at 20 per page; `offset` is a 0-based page index
    // (max 9). We map our page number onto the offset directly.
    let count = ctx.max_results.clamp(1, 20).to_string();
    let offset = ctx.pageno.saturating_sub(1).clamp(0, 9).to_string();
    let safe = match ctx.safe_search {
        2 => "strict",
        1 => "moderate",
        _ => "off",
    };
    let mut query = vec![
        ("q", ctx.query),
        ("count", count.as_str()),
        ("offset", offset.as_str()),
        ("safesearch", safe),
        ("search_lang", ctx.lang_code()),
    ];
    // A region suffix (e.g. `en-us`) maps to Brave's 2-letter `country` param.
    let country = ctx
        .lang
        .split('-')
        .nth(1)
        .map(str::to_ascii_uppercase)
        .filter(|c| c.len() == 2);
    if let Some(c) = country.as_deref() {
        query.push(("country", c));
    }
    if let Some(freshness) = match ctx.time_range {
        Some("day") => Some("pd"),
        Some("week") => Some("pw"),
        Some("month") => Some("pm"),
        Some("year") => Some("py"),
        _ => None,
    } {
        query.push(("freshness", freshness));
    }

    let resp = ctx
        .client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("Accept-Encoding", "gzip")
        .header("X-Subscription-Token", key)
        .query(&query)
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| super::body_error(&e))?;
    Ok(parse(&body))
}

/// Parse a Brave Search API `web.results` array. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["web"]["results"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let url = item["url"].as_str().unwrap_or_default();
        let title = strip_html(item["title"].as_str().unwrap_or_default());
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let content = strip_html(item["description"].as_str().unwrap_or_default());
        let mut r = EngineResult::new(url.to_string(), title, content);
        r.category = Some("general".into());
        // `page_age` is the document's own date; `age` is a relative-ish label.
        r.published_date = item["page_age"]
            .as_str()
            .or_else(|| item["age"].as_str())
            .map(String::from);
        if let Some(thumb) = item["thumbnail"]["src"].as_str() {
            if !thumb.is_empty() {
                r.thumbnail = Some(thumb.to_string());
            }
        }
        results.push(r);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/brave_api.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("reliable"));
        // Highlight tags are stripped from the snippet.
        assert!(!results[0].content.contains('<'));
        assert!(results[0].thumbnail.is_some());
    }
}
