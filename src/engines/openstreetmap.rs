//! OpenStreetMap place search via Nominatim (JSON, keyless). `map` category.
//!
//! Nominatim asks for a descriptive User-Agent and is rate-limited to ~1 req/s;
//! the orchestrator's politeness limiter keeps us well within that.

use serde_json::Value;

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let limit = ctx.max_results.clamp(1, 40).to_string();

    let resp = ctx
        .client
        .get("https://nominatim.openstreetmap.org/search")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .header("Accept-Language", ctx.lang)
        .query(&[
            ("q", ctx.query),
            ("format", "jsonv2"),
            ("limit", limit.as_str()),
            ("addressdetails", "0"),
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

/// Parse a Nominatim `jsonv2` response (a bare JSON array). Pure for testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body.as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let display = item["display_name"].as_str().unwrap_or_default();
        if display.is_empty() {
            continue;
        }
        let osm_type = item["osm_type"].as_str().unwrap_or_default();
        let osm_id = item["osm_id"].as_i64().unwrap_or(0);
        // Stable permalink into the OSM website (first letter of the OSM type).
        let url = match (osm_type.chars().next(), osm_id) {
            (Some(c), id) if id != 0 => {
                format!("https://www.openstreetmap.org/{}/{id}", type_path(c))
            }
            _ => {
                let lat = item["lat"].as_str().unwrap_or("0");
                let lon = item["lon"].as_str().unwrap_or("0");
                format!("https://www.openstreetmap.org/#map=16/{lat}/{lon}")
            }
        };
        let name = item["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(display);
        let kind = item["type"].as_str().unwrap_or_default();
        let category = item["category"].as_str().unwrap_or_default();
        let mut content = String::new();
        if !category.is_empty() || !kind.is_empty() {
            content.push_str(&format!("{category} / {kind} · "));
        }
        content.push_str(display);
        let mut r = EngineResult::new(url, name, content);
        r.template = Some("map.html".into());
        r.category = Some("map".into());
        results.push(r);
    }
    results
}

/// Map an OSM element type letter to its website path segment.
fn type_path(c: char) -> &'static str {
    match c {
        'n' | 'N' => "node",
        'w' | 'W' => "way",
        'r' | 'R' => "relation",
        _ => "node",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/openstreetmap.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Tour Eiffel");
        assert!(results[0].url.contains("openstreetmap.org/way/5013364"));
        assert_eq!(results[0].template.as_deref(), Some("map.html"));
        assert!(results[0].content.contains("man_made"));
    }
}
