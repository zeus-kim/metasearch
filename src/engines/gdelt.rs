//! GDELT Doc API engine (JSON, keyless). `news` category.
//!
//! `https://api.gdeltproject.org/api/v2/doc/doc?query=…&format=json&mode=artlist`
//! indexes worldwide online news in near-real time. Keyless. The `parse`
//! function is pure (fixture-tested).

use serde_json::Value;

use super::{body_error, EngineContext, USER_AGENT};
use crate::thumbnail::is_usable_thumbnail_url;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let max = ctx.max_results.clamp(1, 75).to_string();
    // No explicit `sort`: GDELT defaults to relevance for `artlist`, and the
    // documented sort tokens are case-sensitive (`HybridRel`, not the lowercase
    // form, which the endpoint silently mishandles). Keyless GDELT is also
    // aggressively rate-limited (≈1 request / 5s per IP), so under concurrent
    // fan-out it frequently returns HTTP 429 — fixture-tested, live-flaky.
    let mut query = vec![
        ("query", ctx.query.to_string()),
        ("format", "json".to_string()),
        ("mode", "artlist".to_string()),
        ("maxrecords", max),
    ];
    // Map our recency window to a GDELT `timespan` (min/h/d/w/m units).
    if let Some(ts) = match ctx.time_range {
        Some("day") => Some("1d"),
        Some("week") => Some("1w"),
        Some("month") => Some("1m"),
        Some("year") => Some("12m"),
        _ => None,
    } {
        query.push(("timespan", ts.to_string()));
    }

    let resp = ctx
        .client
        .get("https://api.gdeltproject.org/api/v2/doc/doc")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&query)
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

/// Parse a GDELT Doc `artlist` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["articles"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let url = item["url"].as_str().unwrap_or_default();
        let title = item["title"].as_str().unwrap_or_default();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let domain = item["domain"].as_str().unwrap_or("");
        let country = item["sourcecountry"].as_str().unwrap_or("");
        let mut content = String::new();
        if !domain.is_empty() {
            content.push_str(domain);
        }
        if !country.is_empty() {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(country);
        }
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("news".into());
        // GDELT seen-date is like `20260530T101500Z`.
        r.published_date = item["seendate"].as_str().map(String::from);
        let img = item["socialimage"].as_str().unwrap_or_default();
        if !img.is_empty() && is_usable_thumbnail_url(img) {
            r.thumbnail = Some(img.to_string());
            r.img_src = Some(img.to_string());
        }
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
            serde_json::from_str(include_str!("../../tests/fixtures/gdelt.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("Rust"));
        assert!(results[0].url.starts_with("http"));
        assert_eq!(results[0].category.as_deref(), Some("news"));
        assert!(results[0].content.contains("example.com"));
        assert!(results[0].published_date.is_some());
    }
}
