//! Semantic Scholar engine via the public Graph API (JSON, keyless). `science`.
//!
//! `https://api.semanticscholar.org/graph/v1/paper/search` searches ~200M
//! papers. Keyless (an optional API key only raises rate limits). The `parse`
//! function is pure (fixture-tested). Note: the keyless tier is aggressively
//! rate-limited, so live calls may intermittently return HTTP 429.

use serde_json::Value;

use super::{body_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let limit = ctx.max_results.clamp(1, 100).to_string();
    let offset = ctx.offset().to_string();

    let resp = ctx
        .client
        .get("https://api.semanticscholar.org/graph/v1/paper/search")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("query", ctx.query),
            ("limit", limit.as_str()),
            ("offset", offset.as_str()),
            (
                "fields",
                "title,abstract,url,year,authors,externalIds,citationCount",
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

/// Parse a Semantic Scholar `paper/search` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["data"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let title = item["title"].as_str().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        // Prefer a DOI landing page, then the S2 page, then the paper id.
        let url = item["externalIds"]["DOI"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|doi| format!("https://doi.org/{doi}"))
            .or_else(|| item["url"].as_str().map(String::from))
            .or_else(|| {
                item["paperId"]
                    .as_str()
                    .map(|id| format!("https://www.semanticscholar.org/paper/{id}"))
            })
            .unwrap_or_default();
        if url.is_empty() {
            continue;
        }
        let authors: Vec<String> = item["authors"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["name"].as_str().map(String::from))
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();
        let mut content = String::new();
        if !authors.is_empty() {
            content.push_str(&authors.join(", "));
        }
        if let Some(year) = item["year"].as_i64() {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(&year.to_string());
        }
        let citations = item["citationCount"].as_i64().unwrap_or(0);
        if citations > 0 {
            content.push_str(&format!(" · {citations} citations"));
        }
        if let Some(abs) = item["abstract"].as_str().filter(|s| !s.is_empty()) {
            let snippet: String = abs.chars().take(160).collect();
            content.push_str(" — ");
            content.push_str(snippet.trim());
        }
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("science".into());
        r.published_date = item["year"].as_i64().map(|y| y.to_string());
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
            serde_json::from_str(include_str!("../../tests/fixtures/semanticscholar.json"))
                .unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("Attention Is All You Need"));
        assert!(results[0].url.contains("doi.org"));
        assert!(results[0].content.contains("Vaswani"));
        assert!(results[0].content.contains("citations"));
    }
}
