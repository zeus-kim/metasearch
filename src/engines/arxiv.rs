//! arXiv engine via the export Atom API (XML, keyless). Science category.

use quick_xml::events::Event;
use quick_xml::Reader;

use super::{strip_html, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let start = ctx.offset().to_string();
    let max = ctx.max_results.to_string();
    let search_query = format!("all:{}", ctx.query);

    let resp = ctx
        .client
        .get("https://export.arxiv.org/api/query")
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("search_query", search_query.as_str()),
            ("start", start.as_str()),
            ("max_results", max.as_str()),
            ("sortBy", "relevance"),
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

/// Parse an arXiv Atom feed into results. Pure for fixture testing.
pub(crate) fn parse(xml: &str) -> Vec<EngineResult> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut results = Vec::new();
    let mut depth = 0usize; // nesting depth of <entry>
    let mut in_entry = false;
    let mut cur_tag = String::new();
    let mut title = String::new();
    let mut id = String::new();
    let mut summary = String::new();
    let mut published = String::new();
    let mut authors: Vec<String> = Vec::new();
    let mut in_author = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "entry" => {
                        in_entry = true;
                        depth = 0;
                        title.clear();
                        id.clear();
                        summary.clear();
                        published.clear();
                        authors.clear();
                    }
                    "author" if in_entry => in_author = true,
                    _ => {}
                }
                if in_entry {
                    depth += 1;
                    cur_tag = name;
                }
            }
            Ok(Event::Text(t)) => {
                if in_entry {
                    let text = t.decode().unwrap_or_default().to_string();
                    match cur_tag.as_str() {
                        "title" => title.push_str(&text),
                        "id" => id.push_str(&text),
                        "summary" => summary.push_str(&text),
                        "published" => published.push_str(&text),
                        "name" if in_author => authors.push(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "author" {
                    in_author = false;
                }
                if name == "entry" {
                    in_entry = false;
                    if !title.is_empty() && !id.is_empty() {
                        let mut content = strip_html(summary.trim());
                        if !authors.is_empty() {
                            content = format!("{} — {}", authors.join(", "), content);
                        }
                        let mut r = EngineResult::new(
                            id.trim().to_string(),
                            strip_html(title.trim()),
                            content,
                        );
                        if !published.is_empty() {
                            r.published_date = Some(published.trim().to_string());
                        }
                        r.category = Some("science".into());
                        results.push(r);
                    }
                } else if in_entry {
                    depth = depth.saturating_sub(1);
                    cur_tag.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    results
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
        let xml = include_str!("../../tests/fixtures/arxiv.xml");
        let results = parse(xml);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("Attention Is All You Need"));
        assert!(results[0].url.contains("arxiv.org/abs/1706.03762"));
        assert!(results[0].content.contains("Vaswani"));
    }
}
