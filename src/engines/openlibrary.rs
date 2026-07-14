//! Open Library book search via the public JSON API (keyless). `general`.
//!
//! `https://openlibrary.org/search.json?q=…` searches the Internet Archive's
//! open bibliographic catalog. Keyless. The `parse` function is pure
//! (fixture-tested).

use serde_json::Value;

use super::{body_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let limit = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://openlibrary.org/search.json")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("q", ctx.query),
            ("limit", limit.as_str()),
            ("page", page.as_str()),
            (
                "fields",
                "key,title,author_name,first_publish_year,cover_i,edition_count",
            ),
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

/// Parse an Open Library `search.json` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["docs"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let key = item["key"].as_str().unwrap_or_default();
        let title = item["title"].as_str().unwrap_or_default();
        if key.is_empty() || title.is_empty() {
            continue;
        }
        let url = format!("https://openlibrary.org{key}");
        let authors: Vec<String> = item["author_name"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();
        let mut content = String::new();
        if !authors.is_empty() {
            content.push_str(&authors.join(", "));
        }
        if let Some(year) = item["first_publish_year"].as_i64() {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(&year.to_string());
        }
        if let Some(editions) = item["edition_count"].as_i64() {
            if editions > 0 {
                content.push_str(&format!(" · {editions} editions"));
            }
        }
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("general".into());
        if let Some(cover) = item["cover_i"].as_i64() {
            r.thumbnail = Some(format!("https://covers.openlibrary.org/b/id/{cover}-M.jpg"));
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
            serde_json::from_str(include_str!("../../tests/fixtures/openlibrary.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "The Rust Programming Language");
        assert!(results[0].url.contains("openlibrary.org/works/"));
        assert!(results[0].content.contains("Steve Klabnik"));
        assert!(results[0]
            .thumbnail
            .as_deref()
            .unwrap()
            .contains("covers.openlibrary.org"));
    }
}
