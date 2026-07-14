//! GitHub repository search via the public REST API (JSON, keyless).
//!
//! Unauthenticated requests are rate-limited (≈10 req/min for search); the
//! orchestrator's politeness limiter and 429 backoff keep us inside that.

use serde_json::Value;

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let per_page = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://api.github.com/search/repositories")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
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

/// Parse a GitHub repository search response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["items"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let url = item["html_url"].as_str().unwrap_or_default();
        let name = item["full_name"].as_str().unwrap_or_default();
        if url.is_empty() || name.is_empty() {
            continue;
        }
        let desc = item["description"].as_str().unwrap_or_default();
        let stars = item["stargazers_count"].as_i64().unwrap_or(0);
        let lang = item["language"].as_str().unwrap_or("");
        let mut meta = format!("★ {stars}");
        if !lang.is_empty() {
            meta.push_str(&format!(" · {lang}"));
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
            serde_json::from_str(include_str!("../../tests/fixtures/github.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "BurntSushi/ripgrep");
        assert!(results[0].content.contains("★"));
        assert!(results[0].url.contains("github.com/BurntSushi/ripgrep"));
    }
}
