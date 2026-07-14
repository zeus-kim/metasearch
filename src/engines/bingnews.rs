//! Bing News via the Bing News Search API v7 — OPT-IN and key-based.
//!
//! Disabled by default. Reuses the same subscription key as the `bing` web
//! engine, supplied via config (`api_key`) or, preferably, the `BING_API_KEY`
//! environment variable (never hardcoded). The key is sent in the
//! `Ocp-Apim-Subscription-Key` header and is never logged. Mapped to the `news`
//! category, complementing the keyless news engines (Google News, GDELT,
//! Wikinews). With no key the engine fails gracefully.

use serde_json::Value;

use super::EngineContext;
use crate::thumbnail::is_usable_thumbnail_url;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let key = ctx
        .api_key
        .filter(|k| !k.is_empty())
        .ok_or("bingnews engine needs an API key (set BING_API_KEY)")?;

    let count = ctx.max_results.clamp(1, 100).to_string();
    let offset = ctx.offset().to_string();
    let safe = match ctx.safe_search {
        2 => "Strict",
        1 => "Moderate",
        _ => "Off",
    };
    let mut query = vec![
        ("q", ctx.query),
        ("count", count.as_str()),
        ("offset", offset.as_str()),
        ("safeSearch", safe),
        ("setLang", ctx.lang_code()),
    ];
    // Bing News exposes a coarse recency filter (Day/Week/Month).
    if let Some(freshness) = match ctx.time_range {
        Some("day") => Some("Day"),
        Some("week") => Some("Week"),
        Some("month") => Some("Month"),
        _ => None,
    } {
        query.push(("freshness", freshness));
    }

    let resp = ctx
        .client
        .get("https://api.bing.microsoft.com/v7.0/news/search")
        .header("Ocp-Apim-Subscription-Key", key)
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

/// Parse a Bing News Search `value` array. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["value"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let url = item["url"].as_str().unwrap_or_default();
        let title = item["name"].as_str().unwrap_or_default();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        // Prefix the publisher when present (mirrors the other news engines).
        let provider = item["provider"][0]["name"].as_str().unwrap_or("");
        let desc = item["description"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let content = if provider.is_empty() {
            desc
        } else if desc.is_empty() {
            provider.to_string()
        } else {
            format!("{provider} · {desc}")
        };
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("news".into());
        r.published_date = item["datePublished"].as_str().map(String::from);
        if let Some(thumb) = item["image"]["thumbnail"]["contentUrl"].as_str() {
            if !thumb.is_empty() && is_usable_thumbnail_url(thumb) {
                r.thumbnail = Some(thumb.to_string());
                r.img_src = Some(thumb.to_string());
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
            serde_json::from_str(include_str!("../../tests/fixtures/bingnews.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].url, "https://news.example.com/rust-1-0");
        assert!(results[0].title.contains("Rust"));
        assert_eq!(results[0].category.as_deref(), Some("news"));
        assert!(results[0].content.contains("Example News"));
        assert!(results[0].published_date.is_some());
        assert!(results[0].thumbnail.is_some());
    }
}
