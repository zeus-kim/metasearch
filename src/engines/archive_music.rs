//! Internet Archive audio search (keyless JSON API). `music` category.
//!
//! Wraps archive.org `advancedsearch.php` with a `mediatype:audio` filter so the
//! music tab has a dedicated engine beyond general archive results.

use serde_json::Value;

use super::{body_error, internetarchive, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let q = if ctx.query.trim().is_empty() {
        "mediatype:audio".to_string()
    } else {
        format!("({}) AND mediatype:audio", ctx.query)
    };
    let rows = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://archive.org/advancedsearch.php")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("q", q.as_str()),
            ("fl[]", "identifier"),
            ("fl[]", "title"),
            ("fl[]", "description"),
            ("fl[]", "mediatype"),
            ("fl[]", "year"),
            ("rows", rows.as_str()),
            ("page", page.as_str()),
            ("output", "json"),
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

/// Parse archive.org audio hits, tagging results as `music`. Pure for testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    internetarchive::parse(body)
        .into_iter()
        .map(|mut r| {
            r.category = Some("music".into());
            r
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture_as_music() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/internetarchive.json"))
                .unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].category.as_deref(), Some("music"));
    }
}
