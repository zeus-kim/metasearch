//! RubyGems package registry search via the public API (JSON, keyless). `it`.
//!
//! `https://rubygems.org/api/v1/search.json?query=…` returns a top-level JSON
//! array of gems. Keyless. The `parse` function is pure (fixture-tested).

use serde_json::Value;

use super::{body_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://rubygems.org/api/v1/search.json")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[("query", ctx.query), ("page", page.as_str())])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| body_error(&e))?;
    let mut results = parse(&body);
    results.truncate(ctx.max_results.max(1));
    Ok(results)
}

/// Parse a RubyGems `search.json` response (a JSON array). Pure for fixture
/// testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body.as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let name = item["name"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let url = item["project_uri"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("https://rubygems.org/gems/{name}"));
        let info = item["info"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let version = item["version"].as_str().unwrap_or("");
        let downloads = item["downloads"].as_i64().unwrap_or(0);
        let mut meta = String::new();
        if !version.is_empty() {
            meta.push_str(&format!("v{version}"));
        }
        if downloads > 0 {
            if !meta.is_empty() {
                meta.push_str(" · ");
            }
            meta.push_str(&format!("↓ {downloads}"));
        }
        let content = if info.is_empty() {
            meta
        } else if meta.is_empty() {
            info
        } else {
            format!("{info} — {meta}")
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
            serde_json::from_str(include_str!("../../tests/fixtures/rubygems.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "rails");
        assert!(results[0].url.contains("rubygems.org"));
        assert!(results[0].content.contains("v7"));
    }
}
