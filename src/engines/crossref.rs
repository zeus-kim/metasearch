//! Crossref scholarly metadata engine (JSON, keyless). `science` category.

use serde_json::Value;

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let rows = ctx.max_results.clamp(1, 50).to_string();
    let offset = ctx.offset().to_string();

    let resp = ctx
        .client
        .get("https://api.crossref.org/works")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("query", ctx.query),
            ("rows", rows.as_str()),
            ("offset", offset.as_str()),
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

/// Join Crossref `family`/`given` author objects into a short list.
fn authors(item: &Value) -> Vec<String> {
    item["author"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    let family = x["family"].as_str()?;
                    Some(match x["given"].as_str() {
                        Some(g) => format!("{g} {family}"),
                        None => family.to_string(),
                    })
                })
                .take(3)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a Crossref `/works` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["message"]["items"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let title = item["title"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        let url = item["URL"].as_str().unwrap_or_default();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let mut content = String::new();
        let authors = authors(item);
        if !authors.is_empty() {
            content.push_str(&authors.join(", "));
        }
        if let Some(container) = item["container-title"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|t| t.as_str())
        {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(container);
        }
        if let Some(year) = item["issued"]["date-parts"][0][0].as_i64() {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(&year.to_string());
        }
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("science".into());
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
            serde_json::from_str(include_str!("../../tests/fixtures/crossref.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("Attention"));
        assert!(results[0].url.contains("doi.org"));
        assert!(results[0].content.contains("Vaswani"));
        assert!(results[0].content.contains("2017"));
    }
}
