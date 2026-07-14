//! Yandex web search via the official Yandex Search API (XML) — OPT-IN and
//! key-based.
//!
//! Disabled by default. Yandex's Search API needs two credentials, supplied via
//! config (`api_key` + `extra`) or, preferably, the `YANDEX_API_KEY` and
//! `YANDEX_FOLDER_ID` environment variables (never hardcoded):
//!   * `api_key` — a Yandex Cloud API key authorized for the Search API
//!   * `extra`   — the Yandex Cloud folder id the key belongs to
//!
//! The synchronous GET endpoint (`https://yandex.com/search/xml`) returns an
//! XML document; [`parse`] is pure (fixture-tested). The key is never logged.
//! With no key the engine fails gracefully.
//!
//! NOTE: fixture-only / unverified-live — exercising the real endpoint requires
//! a funded Yandex Cloud folder + API key. The request shape and XML parser
//! follow Yandex's documented Search API response format.

use quick_xml::events::Event;
use quick_xml::Reader;

use super::{strip_html, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let key = ctx
        .api_key
        .filter(|k| !k.is_empty())
        .ok_or("yandex engine needs an API key (set YANDEX_API_KEY)")?;
    let folder = ctx
        .extra
        .filter(|k| !k.is_empty())
        .ok_or("yandex engine needs a folder id (set YANDEX_FOLDER_ID)")?;

    // Yandex pages are 0-based.
    let page = ctx.pageno.saturating_sub(1).to_string();
    let resp = ctx
        .client
        .get("https://yandex.com/search/xml")
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("folderid", folder),
            ("apikey", key),
            ("query", ctx.query),
            ("l10n", ctx.lang_code()),
            ("page", page.as_str()),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let xml = resp.text().await.map_err(|e| super::body_read_error(&e))?;
    Ok(parse(&xml))
}

/// Parse a Yandex Search API XML response (`<doc>` entries). Pure for fixture
/// testing.
pub(crate) fn parse(xml: &str) -> Vec<EngineResult> {
    // NB: do NOT trim text. `<hlword>` highlight tags split a title/passage into
    // several text nodes; trimming would drop the spaces between them (turning
    // "<hlword>Rust</hlword> Programming" into "RustProgramming"). `strip_html`
    // normalizes whitespace on the assembled string instead.
    let mut reader = Reader::from_str(xml);

    let mut results = Vec::new();
    let mut in_doc = false;
    // Which leaf field the current text belongs to. `hlword` highlight tags are
    // nested inside `title`/`passage`, so we keep the field across them.
    let mut field = Field::None;
    let mut url = String::new();
    let mut title = String::new();
    let mut headline = String::new();
    let mut passages = String::new();
    let mut modtime = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "doc" => {
                        in_doc = true;
                        url.clear();
                        title.clear();
                        headline.clear();
                        passages.clear();
                        modtime.clear();
                        field = Field::None;
                    }
                    "url" if in_doc => field = Field::Url,
                    "title" if in_doc => field = Field::Title,
                    "headline" if in_doc => field = Field::Headline,
                    "passage" if in_doc => field = Field::Passage,
                    "modtime" if in_doc => field = Field::Modtime,
                    // Keep the active field across highlight markup.
                    "hlword" => {}
                    _ if in_doc => field = Field::None,
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_doc {
                    let text = t.decode().unwrap_or_default();
                    match field {
                        Field::Url => url.push_str(&text),
                        Field::Title => title.push_str(&text),
                        Field::Headline => headline.push_str(&text),
                        Field::Passage => passages.push_str(&text),
                        Field::Modtime => modtime.push_str(&text),
                        Field::None => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "hlword" => {}
                    "doc" => {
                        in_doc = false;
                        let u = url.trim().to_string();
                        let t = strip_html(title.trim());
                        if !u.is_empty() && !t.is_empty() {
                            let content = if !passages.trim().is_empty() {
                                strip_html(passages.trim())
                            } else {
                                strip_html(headline.trim())
                            };
                            let mut r = EngineResult::new(u, t, content);
                            r.category = Some("general".into());
                            if !modtime.trim().is_empty() {
                                r.published_date = Some(modtime.trim().to_string());
                            }
                            results.push(r);
                        }
                        field = Field::None;
                    }
                    "url" | "title" | "headline" | "passage" | "modtime" if in_doc => {
                        field = Field::None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    results
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Url,
    Title,
    Headline,
    Passage,
    Modtime,
}

/// Strip any `ns:` prefix from an XML tag name.
fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let xml = include_str!("../../tests/fixtures/yandex.xml");
        let results = parse(xml);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("reliable"));
        // Highlight tags are stripped out of title + snippet.
        assert!(!results[0].title.contains('<'));
        assert!(!results[0].content.contains('<'));
        assert_eq!(
            results[0].published_date.as_deref(),
            Some("20240115T040000")
        );
        assert_eq!(results.len(), 2);
    }
}
