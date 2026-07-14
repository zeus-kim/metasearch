//! OpenAlex scholarly works engine (JSON, keyless). `science` category.

use serde_json::Value;

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let per_page = ctx.max_results.clamp(1, 50).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://api.openalex.org/works")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("search", ctx.query),
            ("per-page", per_page.as_str()),
            ("page", page.as_str()),
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

/// Parse an OpenAlex `/works` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["results"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let title = item["title"].as_str().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        // Prefer the DOI / landing page; fall back to the OpenAlex work URL.
        let url = item["doi"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| item["primary_location"]["landing_page_url"].as_str())
            .or_else(|| item["id"].as_str())
            .unwrap_or_default()
            .to_string();
        if url.is_empty() {
            continue;
        }
        let authors: Vec<String> = item["authorships"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["author"]["display_name"].as_str().map(String::from))
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();
        let year = item["publication_year"].as_i64();
        let mut content = String::new();
        if !authors.is_empty() {
            content.push_str(&authors.join(", "));
        }
        if let Some(y) = year {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(&y.to_string());
        }
        let citations = item["cited_by_count"].as_i64().unwrap_or(0);
        if citations > 0 {
            content.push_str(&format!(" · {citations} citations"));
        }
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("science".into());
        r.published_date = item["publication_date"].as_str().map(String::from);
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
            serde_json::from_str(include_str!("../../tests/fixtures/openalex.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("Attention Is All You Need"));
        assert!(results[0].url.contains("doi.org"));
        assert!(results[0].content.contains("Vaswani"));
        assert!(results[0].content.contains("2017"));
    }
}
