//! npm registry engine via the public search API (JSON, keyless). `it` category.

use serde_json::Value;

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let size = ctx.max_results.clamp(1, 100).to_string();
    let from = ctx.offset().to_string();

    let resp = ctx
        .client
        .get("https://registry.npmjs.org/-/v1/search")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("text", ctx.query),
            ("size", size.as_str()),
            ("from", from.as_str()),
        ])
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

/// Parse an npm registry search response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["objects"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let pkg = &item["package"];
        let name = pkg["name"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let url = pkg["links"]["npm"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("https://www.npmjs.com/package/{name}"));
        let desc = pkg["description"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let version = pkg["version"].as_str().unwrap_or("");
        let content = if desc.is_empty() && version.is_empty() {
            String::new()
        } else if desc.is_empty() {
            format!("v{version}")
        } else if version.is_empty() {
            desc
        } else {
            format!("{desc} — v{version}")
        };
        let mut r = EngineResult::new(url, name, content);
        r.category = Some("it".into());
        r.published_date = pkg["date"].as_str().map(String::from);
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
            serde_json::from_str(include_str!("../../tests/fixtures/npm.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "express");
        assert!(results[0].url.contains("npmjs.com/package/express"));
        assert!(results[0].content.contains("v5.2.1"));
        assert!(results[0].content.contains("web framework"));
    }
}
