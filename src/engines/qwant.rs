//! Qwant web search via the `api.qwant.com` JSON API — keyless but OPT-IN.
//!
//! Disabled by default (like `startpage`): Qwant's API is keyless but rejects
//! generic clients, so it needs a browser-like `User-Agent` and is more
//! bot-block-prone than the always-on keyless engines. No credential is
//! required; enable it explicitly in config. [`parse`] is pure (fixture-tested).
//!
//! The API is undocumented but stable; the response nests results under
//! `data.result.items` (either a flat array or a `mainline` block list).

use serde_json::Value;

use super::{body_error, strip_html, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let count = ctx.max_results.clamp(1, 10);
    let offset = (ctx.offset()).to_string();
    let count_s = count.to_string();
    // Qwant locales look like `en_US`. We only emit a region when the request
    // carries one (e.g. `en-us`); otherwise fall back to a safe default.
    let locale = ctx
        .lang
        .split_once('-')
        .map(|(l, r)| format!("{}_{}", l.to_ascii_lowercase(), r.to_ascii_uppercase()))
        .unwrap_or_else(|| "en_US".to_string());
    let safe = ctx.safe_search.min(2).to_string();

    let resp = ctx
        .client
        .get("https://api.qwant.com/v3/search/web")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("q", ctx.query),
            ("count", count_s.as_str()),
            ("offset", offset.as_str()),
            ("locale", locale.as_str()),
            ("safesearch", safe.as_str()),
            ("device", "desktop"),
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

/// Parse a Qwant `v3/search/web` response. Pure for fixture testing.
///
/// The web results live under `data.result.items`, which is either a flat array
/// of result objects or an object with a `mainline` array of typed blocks (each
/// block holding its own `items`). We handle both and keep only `web` entries.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = &body["data"]["result"]["items"];
    let mut results = Vec::new();

    if let Some(arr) = items.as_array() {
        for item in arr {
            push_item(item, &mut results);
        }
    } else if let Some(blocks) = items["mainline"].as_array() {
        for block in blocks {
            // Skip ad/promotional blocks; keep organic web results.
            if block["type"].as_str() == Some("ads") {
                continue;
            }
            if let Some(inner) = block["items"].as_array() {
                for item in inner {
                    push_item(item, &mut results);
                }
            } else {
                push_item(block, &mut results);
            }
        }
    }
    results
}

/// Append a single Qwant item as an `EngineResult` when it is a web result.
fn push_item(item: &Value, out: &mut Vec<EngineResult>) {
    // Only organic web results carry a usable url+title; skip other types.
    match item["type"].as_str() {
        Some("web") | None => {}
        Some(_) => return,
    }
    let url = item["url"].as_str().unwrap_or_default();
    let title = strip_html(item["title"].as_str().unwrap_or_default());
    if url.is_empty() || title.is_empty() {
        return;
    }
    let content = strip_html(item["desc"].as_str().unwrap_or_default());
    let mut r = EngineResult::new(url.to_string(), title, content);
    r.category = Some("general".into());
    out.push(r);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/qwant.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("reliable"));
        // Highlight markup is stripped from snippet + title.
        assert!(!results[0].content.contains('<'));
        // The ad block is skipped, leaving only the two web results.
        assert_eq!(results.len(), 2);
    }
}
