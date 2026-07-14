//! RSS/Atom feed parser

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use crate::thumbnail::is_usable_thumbnail_url;

/// Parsed RSS item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssItem {
    pub title: String,
    pub url: String,
    pub description: String,
    pub published: Option<i64>,
    pub source: String,
    pub thumbnail: Option<String>,
    pub category: Option<String>,
    pub language: Option<String>,
    pub feed_type: Option<String>,
    pub country: Option<String>,
    pub normalized_category: Option<String>,
    #[serde(default = "default_tier")]
    pub tier: u8,
}

fn default_tier() -> u8 { 2 }

impl RssItem {
    /// Convert to search result format for frontend
    pub fn to_search_result(&self, source_name: &str) -> serde_json::Value {
        serde_json::json!({
            "title": self.title,
            "url": self.url,
            "content": self.description,
            "engine": "rss",
            "source": source_name,
            "thumbnail": self.thumbnail,
            "publishedDate": self.published.map(|ts| {
                chrono::DateTime::from_timestamp(ts, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            }),
            "category": self.category,
            "language": self.language,
        })
    }
}

/// Parse RSS 2.0 or Atom feed
pub fn parse_feed(xml: &str, source_name: &str) -> Vec<RssItem> {
    // Try RSS 2.0 first, then Atom
    let items = parse_rss2(xml, source_name);
    if !items.is_empty() {
        return items;
    }
    parse_atom(xml, source_name)
}

/// Parse RSS 2.0 format
fn parse_rss2(xml: &str, source_name: &str) -> Vec<RssItem> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut items = Vec::new();
    let mut buf = Vec::new();
    let mut in_item = false;
    let mut in_channel = false;
    let mut current_tag = String::new();

    let mut title = String::new();
    let mut link = String::new();
    let mut description = String::new();
    let mut pub_date = String::new();
    let mut thumbnail = None;
    let mut category = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match tag.as_str() {
                    "channel" => in_channel = true,
                    "item" if in_channel => {
                        in_item = true;
                        title.clear();
                        link.clear();
                        description.clear();
                        pub_date.clear();
                        thumbnail = None;
                        category = None;
                    }
                    "media:thumbnail" | "media:content" | "enclosure" if in_item => {
                        let mut url = String::new();
                        let mut width = 0u32;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"url" => url = html_unescape(&String::from_utf8_lossy(&attr.value)),
                                b"width" => width = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                        if !url.is_empty() && is_usable_thumbnail_url(&url) {
                            // Prefer larger images
                            if thumbnail.is_none() || width > 0 {
                                thumbnail = Some(url);
                            }
                        }
                    }
                    _ => {}
                }
                current_tag = tag;
            }
            Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if in_item && (tag == "media:thumbnail" || tag == "media:content" || tag == "enclosure") {
                    let mut url = String::new();
                    let mut width = 0u32;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"url" => url = html_unescape(&String::from_utf8_lossy(&attr.value)),
                            b"width" => width = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                    if !url.is_empty() && is_usable_thumbnail_url(&url) {
                        // Prefer larger images (width >= 200), or take first usable
                        if thumbnail.is_none() || width >= 200 {
                            thumbnail = Some(url);
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if in_item {
                    let text = String::from_utf8_lossy(e.as_ref()).to_string();
                    match current_tag.as_str() {
                        "title" => title = html_unescape(&text),
                        "link" => link = text,
                        "description" | "content:encoded" => {
                            let decoded = html_unescape(&text);
                            if thumbnail.is_none() {
                                if let Some(img) = extract_img_src(&decoded) {
                                    if is_usable_thumbnail_url(&img) {
                                        thumbnail = Some(img);
                                    }
                                }
                            }
                            if description.is_empty() || decoded.len() > description.len() {
                                description = strip_html(&decoded);
                            }
                        }
                        "pubDate" | "dc:date" => pub_date = text,
                        "category" => category = Some(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if in_item {
                    let text = String::from_utf8_lossy(&e).to_string();
                    match current_tag.as_str() {
                        "title" => title = html_unescape(&text),
                        "description" | "content:encoded" => {
                            if thumbnail.is_none() {
                                if let Some(img) = extract_img_src(&text) {
                                    if is_usable_thumbnail_url(&img) {
                                        thumbnail = Some(img);
                                    }
                                }
                            }
                            if description.is_empty() || text.len() > description.len() {
                                description = strip_html(&text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "item" && in_item {
                    in_item = false;
                    if !title.is_empty() && !link.is_empty() {
                        items.push(RssItem {
                            title: title.clone(),
                            url: link.clone(),
                            description: truncate(&description, 500),
                            published: parse_date(&pub_date),
                            source: source_name.to_string(),
                            thumbnail: thumbnail.clone(),
                            category: category.clone(),
                            language: None,
                            feed_type: None,
                            country: None,
                            normalized_category: None,
                            tier: 2,
                        });
                    }
                }
                if tag == "channel" {
                    in_channel = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    items
}

/// Parse Atom format
fn parse_atom(xml: &str, source_name: &str) -> Vec<RssItem> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut items = Vec::new();
    let mut buf = Vec::new();
    let mut in_entry = false;
    let mut current_tag = String::new();

    let mut title = String::new();
    let mut link = String::new();
    let mut summary = String::new();
    let mut updated = String::new();
    let mut thumbnail = None::<String>;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match tag.as_str() {
                    "entry" => {
                        in_entry = true;
                        title.clear();
                        link.clear();
                        summary.clear();
                        updated.clear();
                        thumbnail = None;
                    }
                    "link" if in_entry => {
                        let mut href = String::new();
                        let mut rel = String::new();
                        let mut link_type = String::new();
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"href" => href = String::from_utf8_lossy(&attr.value).to_string(),
                                b"rel" => rel = String::from_utf8_lossy(&attr.value).to_string(),
                                b"type" => link_type = String::from_utf8_lossy(&attr.value).to_string(),
                                _ => {}
                            }
                        }
                        if rel == "enclosure" && link_type.starts_with("image/") {
                            thumbnail = Some(href.clone());
                        } else if rel.is_empty() || rel == "alternate" {
                            link = href;
                        }
                    }
                    "media:thumbnail" | "media:content" if in_entry => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"url" {
                                let url = String::from_utf8_lossy(&attr.value).to_string();
                                if thumbnail.is_none() && is_usable_thumbnail_url(&url) {
                                    thumbnail = Some(url);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                current_tag = tag;
            }
            Ok(Event::Text(e)) => {
                if in_entry {
                    let text = String::from_utf8_lossy(e.as_ref()).to_string();
                    match current_tag.as_str() {
                        "title" => title = html_unescape(&text),
                        "summary" | "content" => {
                            let decoded = html_unescape(&text);
                            if thumbnail.is_none() {
                                if let Some(img) = extract_img_src(&decoded) {
                                    if is_usable_thumbnail_url(&img) {
                                        thumbnail = Some(img);
                                    }
                                }
                            }
                            if summary.is_empty() || decoded.len() > summary.len() {
                                summary = strip_html(&decoded);
                            }
                        }
                        "updated" | "published" => updated = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "entry" && in_entry {
                    in_entry = false;
                    if !title.is_empty() && !link.is_empty() {
                        items.push(RssItem {
                            title: title.clone(),
                            url: link.clone(),
                            description: truncate(&summary, 500),
                            published: parse_iso_date(&updated),
                            source: source_name.to_string(),
                            thumbnail: thumbnail.clone(),
                            category: None,
                            language: None,
                            feed_type: None,
                            country: None,
                            normalized_category: None,
                            tier: 2,
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    items
}

/// Unescape HTML entities
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Strip HTML tags and CSS from text
fn strip_html(html: &str) -> String {
    // First, check if this looks like CSS (common patterns)
    if html.contains("background-color:") || html.contains("font-size:")
        || html.contains("border-") || html.contains("color: rgb(")
        || html.contains("text-size-adjust") || html.contains("line-height:")
    {
        return String::new();
    }

    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_style = false;
    let mut in_entity = false;
    let mut entity = String::new();

    let lower_chars: Vec<char> = html.to_lowercase().chars().collect();
    let chars: Vec<char> = html.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let lc = lower_chars.get(i).copied().unwrap_or(c);

        // Check for <style> tag start (character-level comparison)
        if !in_style && lc == '<' && i + 6 < lower_chars.len() {
            let tag: String = lower_chars[i..i+6].iter().collect();
            if tag == "<style" {
                in_style = true;
                in_tag = true;
            }
        }
        // Check for </style> end
        if in_style && lc == '<' && i + 8 <= lower_chars.len() {
            let tag: String = lower_chars[i..i+8].iter().collect();
            if tag == "</style>" {
                in_style = false;
                i += 8;
                continue;
            }
        }

        if in_style {
            i += 1;
            continue;
        }

        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            '&' => {
                in_entity = true;
                entity.clear();
            }
            ';' if in_entity => {
                in_entity = false;
                match entity.as_str() {
                    "amp" => result.push('&'),
                    "lt" => result.push('<'),
                    "gt" => result.push('>'),
                    "quot" => result.push('"'),
                    "apos" => result.push('\''),
                    "nbsp" => result.push(' '),
                    _ => {}
                }
            }
            _ if in_entity => entity.push(c),
            _ if !in_tag => result.push(c),
            _ => {}
        }
        i += 1;
    }

    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate text to max length
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Parse RFC 2822 date (RSS pubDate) - filters out future dates
fn parse_date(s: &str) -> Option<i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let max_future = now + 86400; // Allow 1 day future for timezone issues

    chrono::DateTime::parse_from_rfc2822(s)
        .map(|dt| dt.timestamp())
        .ok()
        .filter(|&ts| ts <= max_future && ts > 0)
}

/// Parse ISO 8601 date (Atom updated) - filters out future dates
fn parse_iso_date(s: &str) -> Option<i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let max_future = now + 86400;

    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .ok()
        .filter(|&ts| ts <= max_future && ts > 0)
}

/// Extract first img src from HTML content
fn extract_img_src(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    if let Some(img_start) = lower.find("<img") {
        let rest = &html[img_start..];
        if let Some(src_start) = rest.to_lowercase().find("src=") {
            let after_src = &rest[src_start + 4..];
            let quote = after_src.chars().next()?;
            if quote == '"' || quote == '\'' {
                let url_start = 1;
                if let Some(url_end) = after_src[url_start..].find(quote) {
                    let url = &after_src[url_start..url_start + url_end];
                    // Accept any http URL as potential image
                    if url.starts_with("http") {
                        return Some(url.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Extract og:image from HTML page content
#[allow(dead_code)]
pub fn extract_og_image(html: &str) -> Option<String> {
    let lower = html.to_lowercase();

    // Try og:image first
    if let Some(img) = extract_meta_content(&lower, html, "og:image") {
        if is_usable_thumbnail_url(&img) {
            return Some(img);
        }
    }

    // Fallback to twitter:image
    if let Some(img) = extract_meta_content(&lower, html, "twitter:image") {
        if is_usable_thumbnail_url(&img) {
            return Some(img);
        }
    }

    None
}

/// Safely get a substring ensuring UTF-8 char boundaries
#[allow(dead_code)]
fn safe_slice(s: &str, start: usize, end: usize) -> &str {
    let mut safe_start = start.min(s.len());
    let mut safe_end = end.min(s.len());

    while safe_start > 0 && !s.is_char_boundary(safe_start) {
        safe_start -= 1;
    }
    while safe_end < s.len() && !s.is_char_boundary(safe_end) {
        safe_end += 1;
    }

    &s[safe_start..safe_end]
}

/// Extract content attribute from meta tag with given property
#[allow(dead_code)]
fn extract_meta_content(_lower: &str, original: &str, property: &str) -> Option<String> {
    // Simple approach: search in original directly with case-insensitive matching
    let original_lower = original.to_lowercase();

    // Match <meta property="og:image" content="...">
    let pattern = format!("property=\"{}\"", property);
    if let Some(prop_idx) = original_lower.find(&pattern) {
        // Look for content=" after the property
        let search_region = &original_lower[prop_idx..];
        if let Some(content_idx) = search_region.find("content=\"") {
            let abs_content_start = prop_idx + content_idx + 9;
            if abs_content_start < original.len() {
                let rest = &original[abs_content_start..];
                if let Some(quote_end) = rest.find('"') {
                    let url = &rest[..quote_end];
                    if url.starts_with("http") {
                        return Some(url.to_string());
                    }
                }
            }
        }

        // Also check before property for content=
        if prop_idx > 0 {
            let before = &original_lower[..prop_idx];
            if let Some(content_idx) = before.rfind("content=\"") {
                let abs_content_start = content_idx + 9;
                if abs_content_start < prop_idx {
                    let url_region = &original[abs_content_start..prop_idx];
                    if let Some(quote_end) = url_region.find('"') {
                        let url = &url_region[..quote_end];
                        if url.starts_with("http") {
                            return Some(url.to_string());
                        }
                    }
                }
            }
        }
    }

    // Also try name="og:image" format
    let name_pattern = format!("name=\"{}\"", property);
    if let Some(name_idx) = original_lower.find(&name_pattern) {
        let search_region = &original_lower[name_idx..];
        if let Some(content_idx) = search_region.find("content=\"") {
            let abs_content_start = name_idx + content_idx + 9;
            if abs_content_start < original.len() {
                let rest = &original[abs_content_start..];
                if let Some(quote_end) = rest.find('"') {
                    let url = &rest[..quote_end];
                    if url.starts_with("http") {
                        return Some(url.to_string());
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_html() {
        assert_eq!(strip_html("<p>Hello <b>world</b></p>"), "Hello world");
        assert_eq!(strip_html("A &amp; B"), "A & B");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello wo...");
    }

    #[test]
    fn test_extract_og_image() {
        let html = r#"<html><head>
            <meta property="og:image" content="https://example.com/image-800x600.jpg">
        </head></html>"#;
        assert_eq!(
            extract_og_image(html),
            Some("https://example.com/image-800x600.jpg".to_string())
        );
    }
}
