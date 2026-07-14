//! Resolve Google News redirect URLs (`news.google.com/rss/articles/…`) to the
//! original publisher article URL. RSS items and cards carry the redirect link;
//! article fetch + rewrite need the real page.

use std::time::Duration;

use crate::url_safety::is_safe_public_url;

const GN_HOST: &str = "news.google.com";
const ARTICLE_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const DECODE_TIMEOUT: Duration = Duration::from_secs(4);

/// True when `url` is a Google News article redirect (RSS / cards).
pub fn is_google_news_article_url(url: &str) -> bool {
    url::Url::parse(url.trim())
        .ok()
        .is_some_and(|u| u.host_str() == Some(GN_HOST) && u.path().contains("/articles/"))
}

/// Extract the base64 article id from a Google News article URL.
pub fn extract_article_id(url: &str) -> Option<String> {
    let path = url::Url::parse(url.trim()).ok()?.path().to_string();
    let parts: Vec<&str> = path.split('/').collect();
    let idx = parts.iter().position(|&p| p == "articles")?;
    parts
        .get(idx + 1)
        .map(|s| s.split('?').next().unwrap_or(s).to_string())
}

/// Best-effort resolve: offline protobuf decode for legacy ids, otherwise fetch
/// the interstitial page + call Google's `garturlreq` batchexecute RPC.
pub async fn resolve_publisher_url(google_url: &str) -> Option<String> {
    if !is_google_news_article_url(google_url) {
        return None;
    }
    let id = extract_article_id(google_url)?;
    if let Some(url) = decode_offline(&id) {
        if is_safe_public_url(&url) {
            return Some(url);
        }
    }
    decode_via_batchexecute(google_url, &id).await
}

/// Legacy Google News ids embed the destination URL directly in the base64 blob.
pub fn decode_offline(article_id: &str) -> Option<String> {
    let bytes = base64_urlsafe_decode(article_id)?;
    if bytes.len() < 5 || bytes[0..3] != [0x08, 0x13, b'"'] {
        return None;
    }
    let mut s = &bytes[3..];
    if s.ends_with(&[0xd2, 0x01, 0x00]) {
        s = &s[..s.len().saturating_sub(3)];
    }
    if s.is_empty() {
        return None;
    }
    let len = s[0] as usize;
    let url_bytes = if len >= 0x80 {
        let n = len & 0x7f;
        if s.len() < 2 + n {
            return None;
        }
        &s[2..2 + n]
    } else if s.len() > len {
        &s[1..1 + len]
    } else {
        return None;
    };
    let url = String::from_utf8(url_bytes.to_vec()).ok()?;
    if url.starts_with("AU_") {
        return None;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url)
    } else {
        None
    }
}

async fn decode_via_batchexecute(gn_url: &str, article_id: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(DECODE_TIMEOUT)
        .build()
        .ok()?;

    let html = client
        .get(gn_url)
        .header(reqwest::header::USER_AGENT, ARTICLE_UA)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;

    let sg = extract_data_attr(&html, "data-n-a-sg")?;
    let ts = extract_data_attr(&html, "data-n-a-ts")?;

    let payload = build_garturl_payload(article_id, &ts, &sg);
    let body = format!(
        "f.req={}",
        url::form_urlencoded::byte_serialize(payload.as_bytes()).collect::<String>()
    );

    let resp = client
        .post("https://news.google.com/_/DotsSplashUi/data/batchexecute")
        .header(
            "content-type",
            "application/x-www-form-urlencoded;charset=UTF-8",
        )
        .header(reqwest::header::USER_AGENT, ARTICLE_UA)
        .header(reqwest::header::REFERER, gn_url)
        .body(body)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let text = resp.text().await.ok()?;
    parse_garturl_response(&text).filter(|u| is_safe_public_url(u))
}

fn extract_data_attr(html: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    let val = rest[..end].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

fn build_garturl_payload(article_id: &str, ts: &str, sg: &str) -> String {
    let inner = format!(
        "[\"garturlreq\",[[\"X\",\"X\",[\"X\",\"X\"],null,null,1,1,\"US:en\",null,1,null,null,null,null,null,0,1],\"X\",\"X\",1,[1,1,1],1,1,null,0,0,null,0],\"{article_id}\",{ts},\"{sg}\"]"
    );
    serde_json::json!([[["Fbv4je", inner, serde_json::Value::Null, "generic"]]]).to_string()
}

fn parse_garturl_response(body: &str) -> Option<String> {
    for chunk in body.split("\n\n") {
        if !chunk.starts_with("[[") {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(chunk).ok()?;
        let inner = val.get(0)?.get(2)?.as_str()?;
        let parsed: serde_json::Value = serde_json::from_str(inner).ok()?;
        if let Some(url) = parsed.get(1).and_then(|v| v.as_str()) {
            if url.starts_with("http://") || url.starts_with("https://") {
                return Some(url.to_string());
            }
        }
    }
    None
}

fn base64_urlsafe_decode(input: &str) -> Option<Vec<u8>> {
    let mut s = input.replace('-', "+").replace('_', "/");
    while !s.is_empty() && s.len() % 4 != 0 {
        s.push('=');
    }
    decode_base64(&s)
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const TABLE: [i8; 128] = {
        let mut t = [-1i8; 128];
        let mut i = 0u8;
        while i < 26 {
            t[(b'A' + i) as usize] = i as i8;
            i += 1;
        }
        i = 0;
        while i < 26 {
            t[(b'a' + i) as usize] = 26 + i as i8;
            i += 1;
        }
        i = 0;
        while i < 10 {
            t[(b'0' + i) as usize] = 52 + i as i8;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };

    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in bytes {
        if b == b'=' {
            break;
        }
        let v = if b < 128 { TABLE[b as usize] } else { -1 };
        if v < 0 {
            continue;
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_google_news_urls() {
        assert!(is_google_news_article_url(
            "https://news.google.com/rss/articles/CBMiabc?oc=5"
        ));
        assert!(!is_google_news_article_url("https://example.com/story"));
    }

    #[test]
    fn extracts_article_id_from_path() {
        assert_eq!(
            extract_article_id("https://news.google.com/rss/articles/CBMiabc123?oc=5").as_deref(),
            Some("CBMiabc123")
        );
    }

    #[test]
    fn offline_decode_legacy_embedded_url() {
        // Known legacy vector: base64 blob embeds https://www.pokernews.com/…
        let id = "CBMiUGh0dHBzOi8vd3d3LnBva2VybmV3cy5jb20vc3RyYXRlZ3kvd3Nzb3AtbWFpbi1ldmVudC10aXBzLW5pbmUtY2hhbXBpb25zLTMxMjg3Lmh0bA";
        let url = decode_offline(id).expect("legacy decode");
        assert!(url.starts_with("https://www.pokernews.com/"));
    }

    #[test]
    fn new_style_ids_need_batchexecute() {
        let id = "CBMisgFBVV95cUxPdGlqLU1DQWRqakpxa3BXdElJZUMxLXNkYmhxYVlBVmJwRkduNTdkMG9YZGdMelFpSTR2WjY0azFLVFMzZHRUdWw1cGxHVkxxelltODhkbHNaZEV5dWhuQ183ZUJPUmJpc1lCUUhMU2JHNDhrSlZOTnZjU0Q3OWdzVldZYkRBY2ptWDFXWXJRTmdtNXV6QmJiX2VrX2FYRmhya01lUkVfLVl1QWdnazRodVR3";
        assert!(decode_offline(id).is_none());
    }
}
