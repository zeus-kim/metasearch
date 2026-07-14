//! Internet Archive search via the `advancedsearch` JSON API (keyless).
//! `general` category.
//!
//! `https://archive.org/advancedsearch.php?q=…&output=json` queries the
//! archive.org catalog (texts, audio, video, software, …). Keyless. The
//! `parse` function is pure (fixture-tested).

use serde_json::Value;

use super::{body_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let rows = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://archive.org/advancedsearch.php")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("q", ctx.query),
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

/// Parse an archive.org `advancedsearch` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["response"]["docs"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let id = item["identifier"].as_str().unwrap_or_default();
        let title = item["title"].as_str().unwrap_or_default();
        if id.is_empty() || title.is_empty() {
            continue;
        }
        let url = format!("https://archive.org/details/{id}");
        let mediatype = item["mediatype"].as_str().unwrap_or("");
        // `description` may be a string or an array of strings.
        let desc = match &item["description"] {
            Value::String(s) => s.clone(),
            Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str())
                .next()
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        };
        let desc = super::strip_html(&desc);
        let desc: String = desc.chars().take(180).collect();
        let mut content = String::new();
        if !mediatype.is_empty() {
            content.push_str(mediatype);
        }
        if !desc.trim().is_empty() {
            if !content.is_empty() {
                content.push_str(" — ");
            }
            content.push_str(desc.trim());
        }
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("general".into());
        // `year` may arrive as a number or a string.
        r.published_date = item["year"]
            .as_str()
            .map(String::from)
            .or_else(|| item["year"].as_i64().map(|y| y.to_string()));
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
            serde_json::from_str(include_str!("../../tests/fixtures/internetarchive.json"))
                .unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].url.contains("archive.org/details/"));
        assert_eq!(results[0].category.as_deref(), Some("general"));
        assert!(!results[0].content.is_empty());
    }
}
