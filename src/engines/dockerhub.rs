//! Docker Hub image search via the public v2 search API (JSON, keyless). `it`.
//!
//! `https://hub.docker.com/v2/search/repositories/?query=…`. Keyless. The
//! `parse` function is pure (fixture-tested).

use serde_json::Value;

use super::{body_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let page_size = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://hub.docker.com/v2/search/repositories/")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("query", ctx.query),
            ("page_size", page_size.as_str()),
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

/// Parse a Docker Hub `search/repositories` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["results"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let name = item["repo_name"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let is_official = item["is_official"].as_bool().unwrap_or(false);
        // Official images live under `/_/`, others under `/r/`.
        let url = if is_official && !name.contains('/') {
            format!("https://hub.docker.com/_/{name}")
        } else {
            format!("https://hub.docker.com/r/{name}")
        };
        let desc = item["short_description"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let stars = item["star_count"].as_i64().unwrap_or(0);
        let pulls = item["pull_count"].as_i64().unwrap_or(0);
        let mut meta = format!("★ {stars} · ↓ {pulls}");
        if is_official {
            meta.push_str(" · official");
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
            serde_json::from_str(include_str!("../../tests/fixtures/dockerhub.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "nginx");
        assert!(results[0].url.contains("hub.docker.com/_/nginx"));
        assert!(results[0].content.contains("official"));
        assert!(results[0].content.contains("★"));
    }
}
