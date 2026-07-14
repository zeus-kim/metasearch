//! Reddit search via the public `search.json` endpoint — OPT-IN and
//! FEATURE-GATED (`--features reddit`).
//!
//! Reddit was previously dropped because it bot-blocks unauthenticated traffic
//! aggressively. It is re-attempted here behind a cargo feature so it never
//! ships enabled by accident, and it fails gracefully when blocked. The pure
//! `parse` function (and its fixture test) compile regardless of the feature.

use serde_json::Value;

use super::EngineContext;
use crate::types::EngineResult;

#[cfg(feature = "reddit")]
pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    use super::USER_AGENT;
    let limit = ctx.max_results.clamp(1, 25).to_string();
    let mut query = vec![
        ("q", ctx.query),
        ("limit", limit.as_str()),
        ("sort", "relevance"),
        ("raw_json", "1"),
    ];
    if let Some(tr) = ctx.time_range {
        query.push(("t", tr));
    }

    let resp = ctx
        .client
        .get("https://www.reddit.com/search.json")
        .header("User-Agent", USER_AGENT)
        .query(&query)
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| super::body_error(&e))?;
    let results = parse(&body);
    if results.is_empty() {
        return Err("no results (possible bot-block)".into());
    }
    Ok(results)
}

#[cfg(not(feature = "reddit"))]
pub async fn search(_ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    Err("reddit engine disabled at build time (rebuild with --features reddit)".into())
}

/// Parse a Reddit `search.json` listing. Pure for fixture testing.
#[cfg_attr(not(feature = "reddit"), allow(dead_code))]
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let children = match body["data"]["children"].as_array() {
        Some(c) => c,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for child in children {
        let d = &child["data"];
        let title = d["title"].as_str().unwrap_or_default();
        let permalink = d["permalink"].as_str().unwrap_or_default();
        if title.is_empty() || permalink.is_empty() {
            continue;
        }
        let url = format!("https://www.reddit.com{permalink}");
        let subreddit = d["subreddit_name_prefixed"]
            .as_str()
            .or_else(|| d["subreddit"].as_str())
            .unwrap_or_default();
        let score = d["score"].as_i64().unwrap_or(0);
        let comments = d["num_comments"].as_i64().unwrap_or(0);
        let body_text = d["selftext"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let snippet: String = body_text.chars().take(300).collect();
        let meta = format!("{subreddit} · ▲{score} · {comments} comments");
        let content = if snippet.is_empty() {
            meta
        } else {
            format!("{meta} — {snippet}")
        };
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("social".into());
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
            serde_json::from_str(include_str!("../../tests/fixtures/reddit.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Why I love Rust");
        assert!(results[0].url.contains("reddit.com/r/rust/comments/"));
        assert!(results[0].content.contains("r/rust"));
    }
}
