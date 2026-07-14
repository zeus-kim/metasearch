//! GitLab project search via the public REST API v4 (JSON, keyless). `it`.

use serde_json::Value;

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let per_page = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://gitlab.com/api/v4/projects")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("search", ctx.query),
            ("per_page", per_page.as_str()),
            ("page", page.as_str()),
            ("order_by", "star_count"),
            ("sort", "desc"),
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

/// Parse a GitLab `/projects` search response (a bare JSON array). Pure for
/// fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body.as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let url = item["web_url"].as_str().unwrap_or_default();
        let name = item["name_with_namespace"]
            .as_str()
            .or_else(|| item["name"].as_str())
            .unwrap_or_default();
        if url.is_empty() || name.is_empty() {
            continue;
        }
        let desc = item["description"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let stars = item["star_count"].as_i64().unwrap_or(0);
        let forks = item["forks_count"].as_i64().unwrap_or(0);
        let meta = format!("★ {stars} · ⑂ {forks}");
        let content = if desc.is_empty() {
            meta
        } else {
            format!("{desc} — {meta}")
        };
        let mut r = EngineResult::new(url, name, content);
        r.category = Some("it".into());
        r.published_date = item["last_activity_at"].as_str().map(String::from);
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
            serde_json::from_str(include_str!("../../tests/fixtures/gitlab.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "GitLab.org / gitlab-runner");
        assert!(results[0].url.contains("gitlab.com"));
        assert!(results[0].content.contains('★'));
    }
}
