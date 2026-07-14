//! Google News engine via the public RSS search feed (XML, keyless). `news`.
//!
//! `https://news.google.com/rss/search?q=…&hl=<lang>&gl=<region>` returns an
//! RSS 2.0 feed of recent news items, with full language/region support. No API
//! key required. The `parse` function is pure (fixture-tested); note that the
//! item `<link>` is a Google News redirect URL (resolved by the client on
//! click), which is expected.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::{extract_img_src_from_html, strip_html, EngineContext, USER_AGENT};
use crate::thumbnail::{best_thumbnail_url, is_usable_thumbnail_url};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let lang = ctx.lang_code();
    // Region: use an explicit region suffix if present (e.g. `en-gb` → `GB`),
    // otherwise mirror the language (`en` → `EN`). `ceid` ties them together.
    let region = ctx
        .lang
        .split('-')
        .nth(1)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_else(|| lang.to_ascii_uppercase());
    let ceid = format!("{region}:{lang}");

    // Google News understands a `when:` recency operator inside the query.
    let mut q = ctx.query.to_string();
    match ctx.time_range {
        Some("day") => q.push_str(" when:1d"),
        Some("week") => q.push_str(" when:7d"),
        Some("year") => q.push_str(" when:1y"),
        _ => {}
    }

    let resp = ctx
        .client
        .get("https://news.google.com/rss/search")
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("q", q.as_str()),
            ("hl", lang),
            ("gl", region.as_str()),
            ("ceid", ceid.as_str()),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let xml = resp.text().await.map_err(|e| super::body_read_error(&e))?;
    let mut results = parse(&xml);
    results.truncate(ctx.max_results.max(1));
    Ok(results)
}

/// Parse a Google News RSS feed into results. Pure for fixture testing.
pub(crate) fn parse(xml: &str) -> Vec<EngineResult> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut results = Vec::new();
    let mut in_item = false;
    let mut cur = String::new();
    let mut title = String::new();
    let mut link = String::new();
    let mut pub_date = String::new();
    let mut description = String::new();
    let mut source = String::new();
    let mut source_url = String::new();
    let mut media_candidates: Vec<MediaCandidate> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "item" {
                    in_item = true;
                    title.clear();
                    link.clear();
                    pub_date.clear();
                    description.clear();
                    source.clear();
                    source_url.clear();
                    media_candidates.clear();
                } else if in_item {
                    if name == "source" {
                        if let Some(u) = attr_value(&e, "url") {
                            source_url = u;
                        }
                    }
                    capture_media_candidate(&e, &name, &mut media_candidates);
                }
                cur = name;
            }
            Ok(Event::Empty(e)) => {
                if in_item {
                    let name = local_name(e.name().as_ref());
                    capture_media_candidate(&e, &name, &mut media_candidates);
                }
            }
            Ok(Event::Text(t)) => {
                if in_item {
                    let text = t.decode().unwrap_or_default().to_string();
                    push_field(
                        &cur,
                        &text,
                        &mut title,
                        &mut link,
                        &mut description,
                        &mut source,
                        &mut pub_date,
                    );
                }
            }
            Ok(Event::CData(t)) => {
                if in_item {
                    let text = String::from_utf8_lossy(t.as_ref()).to_string();
                    push_field(
                        &cur,
                        &text,
                        &mut title,
                        &mut link,
                        &mut description,
                        &mut source,
                        &mut pub_date,
                    );
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "item" {
                    in_item = false;
                    let t = title.trim().to_string();
                    let l = link.trim().to_string();
                    if !t.is_empty() && !l.is_empty() {
                        let content = if !source.trim().is_empty() {
                            source.trim().to_string()
                        } else {
                            let d = strip_html(&description);
                            d.chars().take(200).collect()
                        };
                        let mut r = EngineResult::new(l, strip_html(&t), content);
                        if !pub_date.trim().is_empty() {
                            r.published_date = Some(pub_date.trim().to_string());
                        }
                        if let Some(img) = resolve_item_thumbnail(&media_candidates, &description) {
                            r.thumbnail = Some(img.clone());
                            r.img_src = Some(img);
                        }
                        if !source_url.trim().is_empty() {
                            r.publisher_url = Some(source_url.trim().to_string());
                        }
                        r.category = Some("news".into());
                        results.push(r);
                    }
                }
                cur.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    results
}

fn resolve_item_thumbnail(
    media_candidates: &[MediaCandidate],
    description: &str,
) -> Option<String> {
    if let Some(best) = best_media_thumbnail(media_candidates) {
        return Some(best);
    }
    extract_img_src_from_html(description).filter(|img| is_usable_thumbnail_url(img))
}

fn best_media_thumbnail(media_candidates: &[MediaCandidate]) -> Option<String> {
    media_candidates
        .iter()
        .filter(|c| c.kind == MediaKind::Content && is_usable_thumbnail_url(&c.url))
        .max_by_key(|c| c.width)
        .map(|c| c.url.clone())
        .or_else(|| {
            media_candidates
                .iter()
                .filter(|c| c.kind == MediaKind::Thumbnail && c.width >= 200)
                .filter(|c| is_usable_thumbnail_url(&c.url))
                .max_by_key(|c| c.width)
                .map(|c| c.url.clone())
        })
        .or_else(|| {
            best_thumbnail_url(
                media_candidates
                    .iter()
                    .filter(|c| c.width >= 200)
                    .map(|c| c.url.as_str()),
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Content,
    Thumbnail,
    Enclosure,
}

#[derive(Debug, Clone)]
struct MediaCandidate {
    url: String,
    width: u32,
    kind: MediaKind,
}

fn capture_media_candidate(e: &BytesStart, name: &str, candidates: &mut Vec<MediaCandidate>) {
    if !matches!(name, "content" | "thumbnail" | "enclosure") {
        return;
    }
    let Some(url) = attr_value(e, "url") else {
        return;
    };
    let width = attr_value(e, "width")
        .and_then(|w| w.parse().ok())
        .or_else(|| attr_value(e, "height").and_then(|h| h.parse().ok()))
        .unwrap_or(0);
    let kind = match name {
        "content" => MediaKind::Content,
        "thumbnail" => MediaKind::Thumbnail,
        _ => MediaKind::Enclosure,
    };
    candidates.push(MediaCandidate { url, width, kind });
}

fn attr_value(e: &BytesStart, key: &str) -> Option<String> {
    for attr in e.attributes().flatten() {
        if local_name(attr.key.as_ref()) == key {
            let val = attr
                .unescape_value()
                .map(|c| c.into_owned())
                .unwrap_or_default();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn push_field(
    tag: &str,
    text: &str,
    title: &mut String,
    link: &mut String,
    description: &mut String,
    source: &mut String,
    pub_date: &mut String,
) {
    match tag {
        "title" => title.push_str(text),
        "link" => link.push_str(text),
        "description" => description.push_str(text),
        "source" => source.push_str(text),
        "pubDate" => pub_date.push_str(text),
        _ => {}
    }
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
        let xml = include_str!("../../tests/fixtures/googlenews.xml");
        let results = parse(xml);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("Rust"));
        assert!(results[0].url.starts_with("https://news.google.com/"));
        assert_eq!(results[0].category.as_deref(), Some("news"));
        assert!(results[0].published_date.is_some());
        assert!(results[0].content.contains("Example News"));
        // RSS items without media should not fall back to publisher favicons.
        assert!(results[0].img_src.is_none());
        assert!(results[0].thumbnail.is_none());
    }

    #[test]
    fn parses_media_thumbnail() {
        let xml = r#"<?xml version="1.0"?><rss xmlns:media="http://search.yahoo.com/mrss/"><channel>
            <item>
              <title>Photo story</title>
              <link>https://news.google.com/rss/articles/abc?oc=5</link>
              <media:thumbnail url="https://cdn.example.com/thumb.jpg" width="800"/>
              <source url="https://news.example.com">Example</source>
            </item>
          </channel></rss>"#;
        let results = parse(xml);
        assert_eq!(
            results[0].thumbnail.as_deref(),
            Some("https://cdn.example.com/thumb.jpg")
        );
        assert_eq!(
            results[0].publisher_url.as_deref(),
            Some("https://news.example.com")
        );
    }

    #[test]
    fn picks_widest_media_thumbnail() {
        let xml = r#"<?xml version="1.0"?><rss xmlns:media="http://search.yahoo.com/mrss/"><channel>
            <item>
              <title>Photo story</title>
              <link>https://news.google.com/rss/articles/abc?oc=5</link>
              <media:thumbnail url="https://cdn.example.com/small.jpg" width="120"/>
              <media:content url="https://cdn.example.com/large.jpg" width="800"/>
              <source url="https://news.example.com">Example</source>
            </item>
          </channel></rss>"#;
        let results = parse(xml);
        assert_eq!(
            results[0].img_src.as_deref(),
            Some("https://cdn.example.com/large.jpg")
        );
    }

    #[test]
    fn skips_small_media_thumbnail_without_content() {
        let xml = r#"<?xml version="1.0"?><rss xmlns:media="http://search.yahoo.com/mrss/"><channel>
            <item>
              <title>Small thumb only</title>
              <link>https://news.google.com/rss/articles/abc?oc=5</link>
              <media:thumbnail url="https://cdn.example.com/small.jpg" width="120"/>
              <source url="https://news.example.com">Example</source>
            </item>
          </channel></rss>"#;
        let results = parse(xml);
        assert!(results[0].img_src.is_none());
    }
}
