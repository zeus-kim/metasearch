//! Google web search via the Programmable Search Engine (Custom Search) JSON
//! API — OPT-IN and key-based.
//!
//! Disabled by default. Requires two credentials, supplied via config
//! (`api_key` + `extra`) or, preferably, the `GOOGLE_API_KEY` and
//! `GOOGLE_CSE_ID` environment variables (never hardcoded):
//!   * `api_key` — a Google API key with the Custom Search API enabled
//!   * `extra`   — the Programmable Search Engine id (`cx`)
//!
//! The key is never logged. With no key the engine fails gracefully (a
//! recoverable error that marks it unresponsive without breaking the search).

use serde_json::Value;

use super::EngineContext;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let key = ctx
        .api_key
        .filter(|k| !k.is_empty())
        .ok_or("google engine needs an API key (set GOOGLE_API_KEY)")?;
    let cx = ctx
        .extra
        .filter(|k| !k.is_empty())
        .ok_or("google engine needs a search-engine id (set GOOGLE_CSE_ID)")?;

    // Custom Search returns max 10 per page; `start` is 1-based.
    let num = ctx.max_results.clamp(1, 10).to_string();
    let start = (ctx.offset() + 1).to_string();
    // `lr=lang_<code>` restricts to documents in the query's language; `gl` sets
    // the geo region when the locale carries one (e.g. `ko-KR` → `KR`).
    let lr = format!("lang_{}", ctx.lang_code());
    let gl = ctx
        .lang
        .split('-')
        .nth(1)
        .map(str::to_ascii_uppercase)
        .filter(|c| c.len() == 2);
    let mut query = vec![
        ("key", key),
        ("cx", cx),
        ("q", ctx.query),
        ("num", num.as_str()),
        ("start", start.as_str()),
        ("hl", ctx.lang_code()),
        ("lr", lr.as_str()),
    ];
    if let Some(gl) = gl.as_deref() {
        query.push(("gl", gl));
    }
    if ctx.safe_search >= 1 {
        query.push(("safe", "active"));
    }

    let resp = ctx
        .client
        .get("https://www.googleapis.com/customsearch/v1")
        .query(&query)
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

/// Parse a Custom Search `items` array. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["items"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let url = item["link"].as_str().unwrap_or_default();
        let title = item["title"].as_str().unwrap_or_default();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let content = item["snippet"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("general".into());
        // Pull a thumbnail from the page's pagemap when present.
        if let Some(img) = item["pagemap"]["cse_thumbnail"][0]["src"].as_str() {
            r.thumbnail = Some(img.to_string());
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
            serde_json::from_str(include_str!("../../tests/fixtures/google.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("memory-safe"));
    }
}
