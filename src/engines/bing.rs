//! Bing web search via the Bing Web Search API v7 — OPT-IN and key-based.
//!
//! Disabled by default. Requires a subscription key supplied via config
//! (`api_key`) or, preferably, the `BING_API_KEY` environment variable (never
//! hardcoded). The key is sent in the `Ocp-Apim-Subscription-Key` header and is
//! never logged. With no key the engine fails gracefully.

use serde_json::Value;

use super::EngineContext;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let key = ctx
        .api_key
        .filter(|k| !k.is_empty())
        .ok_or("bing engine needs an API key (set BING_API_KEY)")?;

    let count = ctx.max_results.clamp(1, 50).to_string();
    let offset = ctx.offset().to_string();
    let safe = match ctx.safe_search {
        2 => "Strict",
        1 => "Moderate",
        _ => "Off",
    };
    let query = vec![
        ("q", ctx.query),
        ("count", count.as_str()),
        ("offset", offset.as_str()),
        ("safeSearch", safe),
        ("setLang", ctx.lang_code()),
    ];

    let resp = ctx
        .client
        .get("https://api.bing.microsoft.com/v7.0/search")
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

/// Parse a Bing Web Search `webPages.value` array. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["webPages"]["value"].as_array() {
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
        let content = item["snippet"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("general".into());
        r.published_date = item["datePublished"].as_str().map(String::from);
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
            serde_json::from_str(include_str!("../../tests/fixtures/bing.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("reliable"));
    }
}
