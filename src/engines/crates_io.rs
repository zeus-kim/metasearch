//! crates.io engine via the public registry API (JSON, keyless). `it` category.

use serde_json::Value;

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let per_page = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://crates.io/api/v1/crates")
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

    let body: Value = resp.json().await.map_err(|e| super::body_error(&e))?;
    Ok(parse(&body))
}

/// Parse a crates.io `/crates` search response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["crates"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let name = item["name"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let url = format!("https://crates.io/crates/{name}");
        let desc = item["description"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let downloads = item["downloads"].as_i64().unwrap_or(0);
        let version = item["newest_version"]
            .as_str()
            .or_else(|| item["max_version"].as_str())
            .or_else(|| item["default_version"].as_str())
            .unwrap_or("");
        let mut meta = format!("↓ {downloads}");
        if !version.is_empty() {
            meta.push_str(&format!(" · v{version}"));
        }
        let content = if desc.is_empty() {
            meta
        } else {
            format!("{desc} — {meta}")
        };
        let mut r = EngineResult::new(url, name, content);
        r.category = Some("it".into());
        r.published_date = item["updated_at"].as_str().map(String::from);
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
            serde_json::from_str(include_str!("../../tests/fixtures/crates_io.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "ripgrep");
        assert!(results[0].url.contains("crates.io/crates/ripgrep"));
        assert!(results[0].content.contains("v15.1.0"));
        assert!(results[0].content.contains('↓'));
    }
}
