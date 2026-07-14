//! Wikidata engine via the `wbsearchentities` API (JSON, keyless).
//!
//! Returns structured entities (label + description) for the `general`
//! category, and — for the top hit — fetches the full entity with
//! `wbgetentities` to build a standard [`Infobox`] (knowledge panel):
//! label/description, a thumbnail (P18 image), an official-website link (P856),
//! and a handful of human-readable date claims (born/died/inception). The two
//! pure parsers (`parse`, `parse_infobox`) are fixture-tested; the second
//! network call only fires when the search returned at least one entity.

use serde_json::Value;

use super::{body_error, EngineContext, EngineResponse, WIKIMEDIA_USER_AGENT};
use crate::types::{EngineResult, Infobox, InfoboxAttribute, InfoboxUrl};

pub async fn search(ctx: &EngineContext<'_>) -> Result<EngineResponse, String> {
    let lang = ctx.lang_code();
    let limit = ctx.max_results.to_string();
    let offset = ctx.offset().to_string();

    let resp = ctx
        .client
        .get("https://www.wikidata.org/w/api.php")
        .header("User-Agent", WIKIMEDIA_USER_AGENT)
        .query(&[
            ("action", "wbsearchentities"),
            ("search", ctx.query),
            ("language", lang),
            ("uselang", lang),
            ("format", "json"),
            ("limit", &limit),
            ("continue", &offset),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| body_error(&e))?;
    let mut out = EngineResponse::from(parse(&body));

    // Build an infobox from the top entity (a second, best-effort call). A
    // failure here never fails the engine — we just skip the panel.
    if let Some(id) = body["search"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|e| e["id"].as_str())
        .filter(|s| !s.is_empty())
    {
        if let Ok(Some(ib)) = fetch_infobox(ctx, id, lang).await {
            out.infoboxes.push(ib);
        }
    }

    Ok(out)
}

/// Fetch a single entity via `wbgetentities` and turn it into an infobox.
async fn fetch_infobox(
    ctx: &EngineContext<'_>,
    id: &str,
    lang: &str,
) -> Result<Option<Infobox>, String> {
    let resp = ctx
        .client
        .get("https://www.wikidata.org/w/api.php")
        .header("User-Agent", WIKIMEDIA_USER_AGENT)
        .query(&[
            ("action", "wbgetentities"),
            ("ids", id),
            ("props", "labels|descriptions|claims"),
            ("languages", lang),
            ("format", "json"),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| body_error(&e))?;
    Ok(parse_infobox(&body, id, lang))
}

/// Parse a `wbsearchentities` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["search"].as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let label = item["label"].as_str().unwrap_or_default();
        let id = item["id"].as_str().unwrap_or_default();
        if label.is_empty() || id.is_empty() {
            continue;
        }
        let url = item["concepturi"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://www.wikidata.org/wiki/{id}"));
        let description = item["description"].as_str().unwrap_or_default();
        let mut r = EngineResult::new(url, label, description);
        r.category = Some("general".into());
        results.push(r);
    }
    results
}

/// Build an [`Infobox`] from a `wbgetentities` response. Pure for fixture
/// testing. Returns `None` if the entity has no usable label.
pub(crate) fn parse_infobox(body: &Value, id: &str, lang: &str) -> Option<Infobox> {
    let entity = &body["entities"][id];
    if entity.is_null() {
        return None;
    }

    let label = localized(&entity["labels"], lang)?;
    let description = localized(&entity["descriptions"], lang).unwrap_or_default();

    let mut ib = Infobox::new(label, description, "wikidata");
    ib.id = format!("https://www.wikidata.org/wiki/{id}");

    let claims = &entity["claims"];

    // Thumbnail from the P18 image (a Commons filename → FilePath URL).
    if let Some(file) = string_claim(claims, "P18") {
        let name = file.replace(' ', "_");
        let enc: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
        ib.img_src = format!("https://commons.wikimedia.org/wiki/Special:FilePath/{enc}?width=320");
    }

    // A few human-readable date attributes.
    for (prop, label) in [("P569", "Born"), ("P570", "Died"), ("P571", "Inception")] {
        if let Some(date) = time_claim(claims, prop) {
            ib.attributes.push(InfoboxAttribute {
                label: label.into(),
                value: date,
            });
        }
    }

    // Official website (P856) and the canonical Wikidata page.
    if let Some(site) = string_claim(claims, "P856") {
        ib.urls.push(InfoboxUrl {
            title: "Official website".into(),
            url: site,
        });
    }
    ib.urls.push(InfoboxUrl {
        title: "Wikidata".into(),
        url: format!("https://www.wikidata.org/wiki/{id}"),
    });

    Some(ib)
}

/// Pick the value for `lang` from a Wikidata `labels`/`descriptions` map,
/// falling back to English, then to any available language.
fn localized(map: &Value, lang: &str) -> Option<String> {
    let pick = |l: &str| map[l]["value"].as_str().map(String::from);
    pick(lang)
        .or_else(|| pick("en"))
        .or_else(|| {
            map.as_object()
                .and_then(|o| o.values().next())
                .and_then(|v| v["value"].as_str().map(String::from))
        })
        .filter(|s| !s.is_empty())
}

/// First string-valued snak for `prop` (e.g. P18 filename, P856 URL).
fn string_claim(claims: &Value, prop: &str) -> Option<String> {
    claims[prop][0]["mainsnak"]["datavalue"]["value"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// First time-valued snak for `prop`, normalised to `YYYY-MM-DD`.
fn time_claim(claims: &Value, prop: &str) -> Option<String> {
    let raw = claims[prop][0]["mainsnak"]["datavalue"]["value"]["time"].as_str()?;
    // Wikidata times look like "+1879-03-14T00:00:00Z"; keep the date part.
    let trimmed = raw.trim_start_matches('+');
    let date = trimmed.split('T').next().unwrap_or(trimmed);
    if date.is_empty() {
        None
    } else {
        Some(date.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let fixture = include_str!("../../tests/fixtures/wikidata.json");
        let body: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Albert Einstein");
        assert!(results[0].url.contains("Q937"));
        assert!(results[0].content.contains("physicist"));
    }

    #[test]
    fn parses_infobox_fixture() {
        let fixture = include_str!("../../tests/fixtures/wikidata_entity.json");
        let body: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let ib = parse_infobox(&body, "Q937", "en").expect("infobox");
        assert_eq!(ib.infobox, "Albert Einstein");
        assert!(ib.content.contains("physicist"));
        // P18 thumbnail resolved to a Commons FilePath URL.
        assert!(ib.img_src.contains("commons.wikimedia.org"));
        assert!(ib.img_src.contains("Special:FilePath"));
        // Date attributes extracted and normalised.
        let born = ib.attributes.iter().find(|a| a.label == "Born").unwrap();
        assert_eq!(born.value, "1879-03-14");
        assert!(ib.attributes.iter().any(|a| a.label == "Died"));
        // Official website + Wikidata link present.
        assert!(ib.urls.iter().any(|u| u.title == "Official website"));
        assert!(ib
            .urls
            .iter()
            .any(|u| u.url.contains("wikidata.org/wiki/Q937")));
    }
}
