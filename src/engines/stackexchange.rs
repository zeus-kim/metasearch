//! Stack Exchange (Stack Overflow) search via api.stackexchange.com (keyless).
//!
//! The API *always* gzip-compresses responses, so we decode the body manually
//! with `flate2` (reqwest's `gzip` feature pulls an un-vendored dependency).

use std::io::Read;

use serde_json::Value;

use super::{strip_html, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search_site(ctx, "stackoverflow").await
}

/// Search a specific Stack Exchange `site` (e.g. `stackoverflow`, `askubuntu`,
/// `superuser`). The same `/2.3/search/advanced` endpoint and parser serve
/// every site, so sibling engines (Ask Ubuntu, …) are thin wrappers over this.
pub async fn search_site(ctx: &EngineContext<'_>, site: &str) -> Result<Vec<EngineResult>, String> {
    let pagesize = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://api.stackexchange.com/2.3/search/advanced")
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("order", "desc"),
            ("sort", "relevance"),
            ("q", ctx.query),
            ("site", site),
            ("pagesize", pagesize.as_str()),
            ("page", page.as_str()),
            ("filter", "default"),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let bytes = resp.bytes().await.map_err(|e| super::body_read_error(&e))?;
    let text = decode_body(&bytes);
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))?;
    Ok(parse(&body))
}

/// Try gzip decode, falling back to raw UTF-8.
fn decode_body(bytes: &[u8]) -> String {
    let mut out = String::new();
    if flate2::read::GzDecoder::new(bytes)
        .read_to_string(&mut out)
        .is_ok()
        && !out.is_empty()
    {
        return out;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Parse a Stack Exchange `search/advanced` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["items"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let link = item["link"].as_str().unwrap_or_default();
        let title = strip_html(item["title"].as_str().unwrap_or_default());
        if link.is_empty() || title.is_empty() {
            continue;
        }
        let score = item["score"].as_i64().unwrap_or(0);
        let answers = item["answer_count"].as_i64().unwrap_or(0);
        let answered = item["is_answered"].as_bool().unwrap_or(false);
        let tags: Vec<String> = item["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut content = format!(
            "score {score} · {answers} answers{}",
            if answered { " · ✓ accepted" } else { "" }
        );
        if !tags.is_empty() {
            content.push_str(&format!(" · [{}]", tags.join(", ")));
        }
        let mut r = EngineResult::new(link, title, content);
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
            serde_json::from_str(include_str!("../../tests/fixtures/stackexchange.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("borrow checker"));
        assert!(results[0].content.contains("score"));
        // HTML entity decoded.
        assert!(!results[0].title.contains("&#"));
    }
}
