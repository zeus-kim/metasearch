//! Packagist (PHP/Composer package registry) search via the public JSON API
//! (keyless). `it` category.
//!
//! `https://packagist.org/search.json?q=…`. Keyless. The `parse` function is
//! pure (fixture-tested).

use serde_json::Value;

use super::{body_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let per_page = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://packagist.org/search.json")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("q", ctx.query),
            ("per_page", per_page.as_str()),
            ("page", page.as_str()),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| body_error(&e))?;
    Ok(parse(&body))
}

/// Parse a Packagist `search.json` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["results"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let name = item["name"].as_str().unwrap_or_default();
        let url = item["url"].as_str().unwrap_or_default();
        if name.is_empty() || url.is_empty() {
            continue;
        }
        let desc = item["description"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let downloads = item["downloads"].as_i64().unwrap_or(0);
        let stars = item["favers"].as_i64().unwrap_or(0);
        let mut meta = format!("↓ {downloads}");
        if stars > 0 {
            meta.push_str(&format!(" · ★ {stars}"));
        }
        let content = if desc.is_empty() {
            meta
        } else {
            format!("{desc} — {meta}")
        };
        let mut r = EngineResult::new(url, name, content);
        r.category = Some("it".into());
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
            serde_json::from_str(include_str!("../../tests/fixtures/packagist.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "monolog/monolog");
        assert!(results[0].url.contains("packagist.org"));
        assert!(results[0].content.contains("logs"));
        assert!(results[0].content.contains("↓"));
    }
}
