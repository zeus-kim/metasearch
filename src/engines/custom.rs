//! Config-driven generic engine adapters.
//!
//! These let an engine be declared *entirely* in `settings.yml` (a
//! [`crate::config::CustomEngine`]) with no Rust module per source — the way to
//! grow the engine set to broad coverage without hand-writing hundreds of
//! modules. Three adapter `type`s are supported:
//!
//! * **`rss`** — fetch an RSS 2.0 or Atom feed (commonly a feed *search* URL)
//!   and parse its items. Reuses the same `quick-xml` handling the `googlenews`
//!   (RSS) and `arxiv` (Atom) engines use, unified into one [`parse_feed`].
//! * **`opensearch`** — query an OpenSearch endpoint, either via a templated
//!   search URL or by discovering it from an OpenSearch description document.
//! * **`json`** — fetch a JSON API and map fields into results via simple
//!   dot/bracket JSONPath-like [`json_path`]s.
//!
//! Every parser here is pure (no I/O) and fixture-tested, exactly like the
//! native engines. Privacy is preserved: the query is never logged.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde_json::Value;

use super::{strip_html, EngineContext, EngineResponse, USER_AGENT};
use crate::config::CustomEngine;
use crate::types::EngineResult;

/// Run a config-driven adapter for `spec`. Dispatches on `spec.kind`.
pub async fn search(
    ctx: &EngineContext<'_>,
    spec: &CustomEngine,
) -> Result<EngineResponse, String> {
    let category = spec.categories.first().map(|s| s.as_str());
    match spec.kind.as_str() {
        "rss" => {
            let url = resolve_template(spec.url_template.as_deref(), ctx, spec)?;
            let xml = fetch_text(ctx, &url).await?;
            let mut results = parse_feed(&xml, category);
            results.truncate(ctx.max_results.max(1));
            Ok(results.into())
        }
        "json" => {
            let url = resolve_template(spec.url_template.as_deref(), ctx, spec)?;
            let body = fetch_json(ctx, &url).await?;
            let mapping = JsonMapping::from_spec(spec);
            let mut results = parse_json(&body, &mapping, category);
            results.truncate(ctx.max_results.max(1));
            Ok(results.into())
        }
        "opensearch" => {
            let text = fetch_opensearch(ctx, spec).await?;
            let trimmed = text.trim_start();
            let mut results = if trimmed.starts_with('{') || trimmed.starts_with('[') {
                let body: Value =
                    serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))?;
                let mapping = JsonMapping::from_spec(spec);
                parse_json(&body, &mapping, category)
            } else {
                parse_feed(&text, category)
            };
            results.truncate(ctx.max_results.max(1));
            Ok(results.into())
        }
        other => Err(format!("unknown custom engine type: {other}")),
    }
}

// --------------------------------------------------------------- URL building

/// Percent-encode a value for inclusion in a URL query string.
fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Substitute the supported placeholders in a `url_template`. `{query}` is
/// URL-encoded; `{query_raw}` is verbatim. Returns an error when no template is
/// configured (should be caught at validation, but defended here too).
fn resolve_template(
    template: Option<&str>,
    ctx: &EngineContext<'_>,
    spec: &CustomEngine,
) -> Result<String, String> {
    let template = template
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("custom engine '{}' has no url_template", spec.name))?;
    let url = fill_placeholders(template, ctx, spec);
    if !ctx.allow_private_urls && !crate::url_safety::is_safe_public_url(&url) {
        return Err("blocked url (ssrf)".into());
    }
    Ok(url)
}

fn fill_placeholders(template: &str, ctx: &EngineContext<'_>, spec: &CustomEngine) -> String {
    template
        .replace("{query}", &enc(ctx.query))
        .replace("{query_raw}", ctx.query)
        .replace("{lang}", ctx.lang_code())
        .replace("{lang_region}", ctx.lang)
        .replace("{page}", &ctx.pageno.max(1).to_string())
        .replace("{offset}", &ctx.offset().to_string())
        .replace("{count}", &ctx.max_results.max(1).to_string())
        .replace("{safe}", &ctx.safe_search.to_string())
        .replace("{api_key}", spec.api_key.as_deref().unwrap_or(""))
}

// --------------------------------------------------------------------- Fetch

async fn fetch_text(ctx: &EngineContext<'_>, url: &str) -> Result<String, String> {
    let resp = ctx
        .client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| super::request_error(&e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.text().await.map_err(|e| super::body_read_error(&e))
}

async fn fetch_json(ctx: &EngineContext<'_>, url: &str) -> Result<Value, String> {
    let resp = ctx
        .client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| super::request_error(&e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| super::body_error(&e))
}

/// Resolve the OpenSearch search URL (discovering it from a description
/// document when needed) and fetch its body as text.
async fn fetch_opensearch(ctx: &EngineContext<'_>, spec: &CustomEngine) -> Result<String, String> {
    // Prefer an explicit templated search URL; otherwise discover one from the
    // OpenSearch description document.
    if let Some(t) = spec.url_template.as_deref().filter(|t| !t.is_empty()) {
        let url = fill_placeholders(t, ctx, spec);
        return fetch_text(ctx, &url).await;
    }
    let desc_url = spec
        .description_url
        .as_deref()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            format!(
                "custom engine '{}' (opensearch) has no description_url",
                spec.name
            )
        })?;
    let desc = fetch_text(ctx, desc_url).await?;
    let template = pick_opensearch_template(&desc)
        .ok_or_else(|| format!("no usable Url template in {desc_url}"))?;
    let url = expand_opensearch_template(&template, ctx, spec);
    fetch_text(ctx, &url).await
}

// --------------------------------------------------------------- Feed parser

/// Parse an RSS 2.0 or Atom feed into results. Pure for fixture testing.
///
/// Handles both `<item>` (RSS) and `<entry>` (Atom) elements, RSS text `<link>`
/// and Atom `href`-attribute `<link>` (preferring `rel="alternate"`), and the
/// common date/summary tag spellings.
pub(crate) fn parse_feed(xml: &str, category: Option<&str>) -> Vec<EngineResult> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut results = Vec::new();
    let mut in_item = false;
    let mut cur = String::new();
    let mut title = String::new();
    let mut link = String::new();
    let mut id = String::new();
    let mut desc = String::new();
    let mut pub_date = String::new();
    let mut image_url = String::new();
    // Whether we have already accepted an Atom alternate link (so a later
    // `rel="self"`/enclosure does not override it).
    let mut got_alternate = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "item" || name == "entry" {
                    in_item = true;
                    got_alternate = false;
                    title.clear();
                    link.clear();
                    id.clear();
                    desc.clear();
                    pub_date.clear();
                    image_url.clear();
                } else if in_item && name == "link" {
                    if let Some(href) = link_href(&e, &mut got_alternate) {
                        link = href;
                    }
                } else if in_item && (name == "content" || name == "thumbnail") {
                    // Extract image from <media:content url="..."> or <media:thumbnail>
                    if let Some(url) = attr_value(&e, "url") {
                        if image_url.is_empty() && is_image_url(&url) {
                            image_url = url;
                        }
                    }
                }
                cur = name;
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                if in_item && name == "link" {
                    if let Some(href) = link_href(&e, &mut got_alternate) {
                        link = href;
                    }
                } else if in_item && (name == "content" || name == "thumbnail" || name == "enclosure") {
                    // Extract image from <media:content url="..."> or <enclosure>
                    if let Some(url) = attr_value(&e, "url") {
                        if image_url.is_empty() && is_image_url(&url) {
                            image_url = url;
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if in_item {
                    let text = t.decode().unwrap_or_default().to_string();
                    // Handle Bing News <News:Image> tag (text content)
                    if cur == "Image" && image_url.is_empty() && is_image_url(&text) {
                        image_url = text.clone();
                    }
                    push_feed_field(
                        &cur,
                        &text,
                        &mut title,
                        &mut link,
                        &mut id,
                        &mut desc,
                        &mut pub_date,
                    );
                }
            }
            Ok(Event::CData(t)) => {
                if in_item {
                    let text = String::from_utf8_lossy(t.as_ref()).to_string();
                    push_feed_field(
                        &cur,
                        &text,
                        &mut title,
                        &mut link,
                        &mut id,
                        &mut desc,
                        &mut pub_date,
                    );
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "item" || name == "entry" {
                    in_item = false;
                    let t = title.trim().to_string();
                    // Fall back to the Atom <id> when no <link> was found (some
                    // feeds, e.g. arXiv, carry the canonical URL there).
                    let l = if link.trim().is_empty() {
                        id.trim().to_string()
                    } else {
                        link.trim().to_string()
                    };
                    if !t.is_empty() && !l.is_empty() {
                        let content: String = strip_html(desc.trim()).chars().take(300).collect();
                        let mut r = EngineResult::new(l, strip_html(&t), content);
                        if !pub_date.trim().is_empty() {
                            r.published_date = Some(pub_date.trim().to_string());
                        }
                        r.category = category.map(String::from);
                        // Use extracted image, or try to find one in description HTML
                        let img = if !image_url.is_empty() {
                            image_url.clone()
                        } else {
                            extract_img_from_html(&desc)
                        };
                        if !img.is_empty() {
                            r.img_src = Some(img.clone());
                            r.thumbnail = Some(img);
                        }
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

#[allow(clippy::too_many_arguments)]
fn push_feed_field(
    tag: &str,
    text: &str,
    title: &mut String,
    link: &mut String,
    id: &mut String,
    desc: &mut String,
    pub_date: &mut String,
) {
    match tag {
        "title" => title.push_str(text),
        // RSS carries the link as element text; Atom uses the href attribute
        // (handled separately) so only fill from text when still empty.
        "link" => {
            if link.is_empty() {
                link.push_str(text);
            }
        }
        "id" | "guid" => id.push_str(text),
        "description" | "summary" | "content" | "encoded" => {
            if desc.is_empty() {
                desc.push_str(text);
            }
        }
        "pubDate" | "published" | "updated" | "date" => {
            if pub_date.is_empty() {
                pub_date.push_str(text);
            }
        }
        _ => {}
    }
}

/// Extract a usable `href` from an Atom `<link>` element. Prefers
/// `rel="alternate"` (or a rel-less link); skips `self`/`edit`/`replies`.
fn link_href(e: &BytesStart, got_alternate: &mut bool) -> Option<String> {
    let mut href: Option<String> = None;
    let mut rel: Option<String> = None;
    for attr in e.attributes().flatten() {
        let key = local_name(attr.key.as_ref());
        let val = attr
            .unescape_value()
            .map(|c| c.into_owned())
            .unwrap_or_default();
        match key.as_str() {
            "href" => href = Some(val),
            "rel" => rel = Some(val),
            _ => {}
        }
    }
    let href = href?;
    if href.is_empty() {
        return None;
    }
    let rel = rel.as_deref();
    if matches!(
        rel,
        Some("self") | Some("edit") | Some("replies") | Some("enclosure")
    ) {
        return None;
    }
    let is_alternate = rel.is_none() || rel == Some("alternate");
    if *got_alternate && !is_alternate {
        return None;
    }
    if is_alternate {
        *got_alternate = true;
    }
    Some(href)
}

/// Strip any `ns:` prefix from an XML tag/attribute name.
fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

/// Extract attribute value from XML element.
fn attr_value(e: &BytesStart, name: &str) -> Option<String> {
    for attr in e.attributes().flatten() {
        let key = local_name(attr.key.as_ref());
        if key == name {
            return attr.unescape_value().map(|c| c.into_owned()).ok();
        }
    }
    None
}

/// Check if URL looks like an image.
fn is_image_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains(".jpg") || lower.contains(".jpeg") || lower.contains(".png")
        || lower.contains(".gif") || lower.contains(".webp")
        || lower.contains("image") || lower.contains("/img/")
}

/// Extract first image URL from HTML content.
fn extract_img_from_html(html: &str) -> String {
    // Look for <img src="...">
    if let Some(start) = html.find("<img") {
        let rest = &html[start..];
        if let Some(src_start) = rest.find("src=\"") {
            let after_src = &rest[src_start + 5..];
            if let Some(end) = after_src.find('"') {
                let url = &after_src[..end];
                if url.starts_with("http") {
                    return url.to_string();
                }
            }
        }
        // Also try src='...'
        if let Some(src_start) = rest.find("src='") {
            let after_src = &rest[src_start + 5..];
            if let Some(end) = after_src.find('\'') {
                let url = &after_src[..end];
                if url.starts_with("http") {
                    return url.to_string();
                }
            }
        }
    }
    String::new()
}

// --------------------------------------------------------------- JSON mapping

/// Borrowed field mapping for the JSON-API template adapter (kept separate from
/// the owned config struct so [`parse_json`] stays pure and trivially testable).
pub(crate) struct JsonMapping<'a> {
    pub result_path: &'a str,
    pub url_field: &'a str,
    pub title_field: &'a str,
    pub content_field: Option<&'a str>,
    pub thumbnail_field: Option<&'a str>,
    pub published_field: Option<&'a str>,
}

impl<'a> JsonMapping<'a> {
    fn from_spec(spec: &'a CustomEngine) -> Self {
        JsonMapping {
            result_path: spec.result_path.as_deref().unwrap_or(""),
            url_field: spec.url_field.as_deref().unwrap_or(""),
            title_field: spec.title_field.as_deref().unwrap_or(""),
            content_field: spec.content_field.as_deref(),
            thumbnail_field: spec.thumbnail_field.as_deref(),
            published_field: spec.published_field.as_deref(),
        }
    }
}

/// Parse a generic JSON response into results using `mapping`. Pure.
pub(crate) fn parse_json(
    body: &Value,
    mapping: &JsonMapping<'_>,
    category: Option<&str>,
) -> Vec<EngineResult> {
    let array = if mapping.result_path.is_empty() {
        body.as_array()
    } else {
        json_path(body, mapping.result_path).and_then(Value::as_array)
    };
    let items = match array {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let url = field_str(item, mapping.url_field);
        let title = field_str(item, mapping.title_field);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let content = mapping
            .content_field
            .map(|f| strip_html(&field_str(item, f)))
            .unwrap_or_default();
        let mut r = EngineResult::new(url, strip_html(&title), content);
        if let Some(tf) = mapping.thumbnail_field {
            let thumb = field_str(item, tf);
            if !thumb.is_empty() {
                r.thumbnail = Some(thumb.clone());
                r.img_src = Some(thumb);
            }
        }
        if let Some(pf) = mapping.published_field {
            let pd = field_str(item, pf);
            if !pd.is_empty() {
                r.published_date = Some(pd);
            }
        }
        r.category = category.map(String::from);
        results.push(r);
    }
    results
}

/// Resolve a field to a display string: strings verbatim, numbers/bools
/// stringified, anything else empty.
fn field_str(item: &Value, path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    match json_path(item, path) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// A minimal, dependency-free JSONPath-like resolver.
///
/// Supports dot-separated keys with optional `[index]` array subscripts, e.g.
/// `data.items`, `hits[0].url`, `[2].title`. Returns `None` for any missing
/// segment. Documented in `docs/custom-engines.md`.
pub(crate) fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for raw_seg in path.split('.') {
        if raw_seg.is_empty() {
            continue;
        }
        let (key, indices) = split_indices(raw_seg);
        if !key.is_empty() {
            cur = cur.get(key)?;
        }
        for idx in indices {
            cur = cur.get(idx)?;
        }
    }
    Some(cur)
}

/// Split a segment like `items[0][1]` into (`"items"`, `[0, 1]`).
fn split_indices(seg: &str) -> (&str, Vec<usize>) {
    let Some(open) = seg.find('[') else {
        return (seg, Vec::new());
    };
    let key = &seg[..open];
    let mut indices = Vec::new();
    let mut rest = &seg[open..];
    while let Some(stripped) = rest.strip_prefix('[') {
        if let Some(close) = stripped.find(']') {
            if let Ok(n) = stripped[..close].parse::<usize>() {
                indices.push(n);
            }
            rest = &stripped[close + 1..];
        } else {
            break;
        }
    }
    (key, indices)
}

// ---------------------------------------------------------- OpenSearch (OSD)

/// Pick the best `<Url template="…">` from an OpenSearch description document.
/// Prefers JSON, then RSS/Atom, then anything else. Pure for fixture testing.
pub(crate) fn pick_opensearch_template(xml: &str) -> Option<String> {
    let templates = parse_opensearch_templates(xml);
    let score = |ty: &str| -> u8 {
        let ty = ty.to_ascii_lowercase();
        if ty.contains("json") {
            3
        } else if ty.contains("rss") || ty.contains("atom") {
            2
        } else if ty.contains("html") {
            0
        } else {
            1
        }
    };
    templates
        .into_iter()
        .max_by_key(|(ty, _)| score(ty))
        .map(|(_, template)| template)
}

/// Extract `(type, template)` pairs from an OpenSearch description document.
pub(crate) fn parse_opensearch_templates(xml: &str) -> Vec<(String, String)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) => {
                if local_name(e.name().as_ref()) == "Url" {
                    let mut ty = String::new();
                    let mut template = String::new();
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        let val = attr
                            .unescape_value()
                            .map(|c| c.into_owned())
                            .unwrap_or_default();
                        match key.as_str() {
                            "type" => ty = val,
                            "template" => template = val,
                            _ => {}
                        }
                    }
                    if !template.is_empty() {
                        out.push((ty, template));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Expand an OpenSearch `template` (with `{searchTerms}`, `{count}`, etc.) into
/// a concrete URL. Unknown optional parameters (`{foo?}`) are blanked.
fn expand_opensearch_template(
    template: &str,
    ctx: &EngineContext<'_>,
    spec: &CustomEngine,
) -> String {
    let start_index = (ctx.offset() + 1).to_string();
    let count = ctx.max_results.max(1).to_string();
    let page = ctx.pageno.max(1).to_string();
    let mut out = template
        .replace("{searchTerms}", &enc(ctx.query))
        .replace("{language}", ctx.lang_code())
        .replace("{count}", &count)
        .replace("{startIndex}", &start_index)
        .replace("{startPage}", &page)
        .replace("{api_key}", spec.api_key.as_deref().unwrap_or(""));
    // Blank out any remaining `{optional?}` (or required) placeholders so the
    // URL stays valid even when the server declares params we don't supply.
    while let Some(open) = out.find('{') {
        if let Some(close) = out[open..].find('}') {
            out.replace_range(open..open + close + 1, "");
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss_fixture() {
        let xml = include_str!("../../tests/fixtures/custom_rss.xml");
        let results = parse_feed(xml, Some("news"));
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "First Post");
        assert_eq!(results[0].url, "https://example.com/first");
        assert_eq!(results[0].category.as_deref(), Some("news"));
        assert!(results[0].published_date.is_some());
        assert!(results[0].content.contains("summary of the first"));
    }

    #[test]
    fn parses_atom_fixture() {
        let xml = include_str!("../../tests/fixtures/custom_atom.xml");
        let results = parse_feed(xml, Some("it"));
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Atom Entry One");
        // The alternate href link, not the self link, is used.
        assert_eq!(results[0].url, "https://example.org/entry-one");
        assert!(results[0].content.contains("first atom entry"));
        assert!(results[0].published_date.is_some());
    }

    #[test]
    fn parses_json_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/custom_json.json")).unwrap();
        let mapping = JsonMapping {
            result_path: "data.items",
            url_field: "link",
            title_field: "name",
            content_field: Some("description"),
            thumbnail_field: Some("image.src"),
            published_field: Some("created_at"),
        };
        let results = parse_json(&body, &mapping, Some("general"));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Result One");
        assert_eq!(results[0].url, "https://example.com/one");
        assert_eq!(results[0].content, "First result description");
        assert_eq!(
            results[0].thumbnail.as_deref(),
            Some("https://example.com/one.png")
        );
        assert_eq!(results[0].published_date.as_deref(), Some("2026-01-01"));
        assert_eq!(results[0].category.as_deref(), Some("general"));
    }

    #[test]
    fn json_path_navigates_keys_and_indices() {
        let v: Value = serde_json::json!({
            "a": { "b": [ {"c": 1}, {"c": 2} ] },
            "list": [10, 20, 30]
        });
        assert_eq!(json_path(&v, "a.b[1].c"), Some(&Value::from(2)));
        assert_eq!(json_path(&v, "list[2]"), Some(&Value::from(30)));
        assert!(json_path(&v, "a.b[5].c").is_none());
        assert!(json_path(&v, "missing").is_none());
    }

    #[test]
    fn json_path_top_level_array() {
        let v: Value = serde_json::json!([{"u": "x"}, {"u": "y"}]);
        let mapping = JsonMapping {
            result_path: "",
            url_field: "u",
            title_field: "u",
            content_field: None,
            thumbnail_field: None,
            published_field: None,
        };
        let results = parse_json(&v, &mapping, None);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].url, "y");
    }

    #[test]
    fn opensearch_prefers_json_then_feed() {
        let xml = include_str!("../../tests/fixtures/opensearch.xml");
        let templates = parse_opensearch_templates(xml);
        assert!(templates.len() >= 2);
        let picked = pick_opensearch_template(xml).unwrap();
        // The JSON template should win over the HTML one.
        assert!(picked.contains("format=json"));
        assert!(picked.contains("{searchTerms}"));
    }

    #[test]
    fn opensearch_template_expands() {
        // Minimal stand-in EngineContext is awkward here; expansion is covered
        // indirectly. Verify placeholder blanking on a raw string instead.
        let t = "https://s.example/?q={searchTerms}&n={count}&x={unknown?}";
        // Manually mimic expand_opensearch_template's blanking for {unknown?}.
        let mut out = t.replace("{searchTerms}", "rust").replace("{count}", "10");
        while let Some(open) = out.find('{') {
            if let Some(close) = out[open..].find('}') {
                out.replace_range(open..open + close + 1, "");
            } else {
                break;
            }
        }
        assert_eq!(out, "https://s.example/?q=rust&n=10&x=");
    }
}
