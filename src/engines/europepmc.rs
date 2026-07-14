//! Europe PMC life-sciences literature engine (JSON, keyless). `science`.

use serde_json::Value;

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let page_size = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://www.ebi.ac.uk/europepmc/webservices/rest/search")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("query", ctx.query),
            ("format", "json"),
            ("pageSize", page_size.as_str()),
            ("page", page.as_str()),
            ("resultType", "lite"),
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

/// Build a stable URL for a Europe PMC record from its source + id / DOI.
fn record_url(item: &Value) -> String {
    if let Some(doi) = item["doi"].as_str().filter(|s| !s.is_empty()) {
        return format!("https://doi.org/{doi}");
    }
    let source = item["source"].as_str().unwrap_or("MED");
    let id = item["id"].as_str().unwrap_or_default();
    if id.is_empty() {
        String::new()
    } else {
        format!("https://europepmc.org/article/{source}/{id}")
    }
}

/// Parse a Europe PMC search response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["resultList"]["result"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let title = item["title"].as_str().unwrap_or_default();
        let url = record_url(item);
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let mut content = String::new();
        if let Some(authors) = item["authorString"].as_str().filter(|s| !s.is_empty()) {
            content.push_str(authors);
        }
        if let Some(journal) = item["journalTitle"].as_str().filter(|s| !s.is_empty()) {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(journal);
        }
        if let Some(year) = item["pubYear"].as_str().filter(|s| !s.is_empty()) {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(year);
        }
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("science".into());
        r.published_date = item["firstPublicationDate"].as_str().map(String::from);
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
            serde_json::from_str(include_str!("../../tests/fixtures/europepmc.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("CRISPR"));
        assert!(results[0].url.starts_with("https://"));
        assert!(results[0].content.contains("Front Microbiol"));
    }
}
