//! Wikipedia engine via the MediaWiki search API (JSON, keyless).

use std::time::Duration;

use serde_json::Value;

use super::{strip_html, EngineContext, EngineResponse, WIKIMEDIA_USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<EngineResponse, String> {
    let lang = ctx.lang_code();
    let url = format!("https://{lang}.wikipedia.org/w/api.php");
    let offset = ctx.offset().to_string();
    let limit = ctx.max_results.to_string();

    let resp = ctx
        .client
        .get(&url)
        .header("User-Agent", WIKIMEDIA_USER_AGENT)
        .query(&[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", ctx.query),
            ("srlimit", &limit),
            ("sroffset", &offset),
            ("format", "json"),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| super::body_error(&e))?;
    let mut out = EngineResponse::from(parse(&body, lang));
    // MediaWiki's "did you mean" suggestion becomes a query correction.
    if let Some(s) = body["query"]["searchinfo"]["suggestion"].as_str() {
        if !s.is_empty() {
            out.corrections.push(s.to_string());
        }
    }
    Ok(out)
}

/// Parse a MediaWiki `list=search` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value, lang: &str) -> Vec<EngineResult> {
    let hits = match body["query"]["search"].as_array() {
        Some(h) => h,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for hit in hits {
        let title = hit["title"].as_str().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let snippet = strip_html(hit["snippet"].as_str().unwrap_or_default());
        let page = title.replace(' ', "_");
        let url = format!("https://{lang}.wikipedia.org/wiki/{page}");
        let mut r = EngineResult::new(url, title, snippet);
        r.template = Some("default.html".into());
        r.category = Some("general".into());
        results.push(r);
    }
    results
}

/// OpenSearch-style autocomplete from MediaWiki.
pub async fn autocomplete(
    client: &reqwest::Client,
    query: &str,
    lang: &str,
    timeout: Duration,
) -> Vec<String> {
    let lang = lang
        .split('-')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("en");
    let url = format!("https://{lang}.wikipedia.org/w/api.php");
    let resp = client
        .get(&url)
        .header("User-Agent", WIKIMEDIA_USER_AGENT)
        .query(&[
            ("action", "opensearch"),
            ("search", query),
            ("limit", "10"),
            ("format", "json"),
        ])
        .timeout(timeout)
        .send()
        .await;
    let Ok(resp) = resp else { return Vec::new() };
    let Ok(body) = resp.json::<Value>().await else {
        return Vec::new();
    };
    // OpenSearch shape: [query, [suggestions], [descriptions], [urls]]
    body[1]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let fixture = include_str!("../../tests/fixtures/wikipedia.json");
        let body: Value = serde_json::from_str(fixture).unwrap();
        let results = parse(&body, "en");
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Albert Einstein");
        assert!(results[0]
            .url
            .contains("en.wikipedia.org/wiki/Albert_Einstein"));
        // HTML stripped from snippet.
        assert!(!results[0].content.contains('<'));
    }
}
