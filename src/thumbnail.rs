//! Heuristics for ranking and filtering news/article thumbnail URLs.
//!
//! Rejects favicons and other tiny assets so Discover/News cards prefer real
//! article imagery over publisher icons.

use serde::{Deserialize, Serialize};

/// Whether a thumbnail URL is suitable for large card display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThumbnailQuality {
    Large,
    Small,
    Unknown,
}

/// Minimum pixel dimension we consider acceptable for a card thumbnail.
/// Anything provably smaller is treated as a favicon/icon and rejected so we
/// never upscale a tiny asset into a large hero/grid slot.
const MIN_CARD_DIM: u32 = 200;
/// Hard floor: provably below this is always rejected (favicon territory).
const TINY_DIM: u32 = 100;

/// Smallest longest-edge (in px) an image-search result may have before we
/// treat it as an icon/thumbnail not worth a grid cell. Looser than
/// [`MIN_CARD_DIM`] because image search legitimately surfaces medium assets;
/// we only want to drop provably icon-sized images.
pub const MIN_SEARCH_IMAGE_DIM: u32 = 100;

/// True when a *known* width/height pair is provably icon-sized (longest edge
/// below [`MIN_SEARCH_IMAGE_DIM`]). Zero/unknown dimensions return `false` so
/// engines never drop an image just because the upstream omitted its size.
pub fn is_tiny_dimension(width: u32, height: u32) -> bool {
    let max_dim = width.max(height);
    max_dim > 0 && max_dim < MIN_SEARCH_IMAGE_DIM
}

/// Classify a thumbnail URL without network I/O.
pub fn thumbnail_quality(url: &str) -> ThumbnailQuality {
    let u = url.trim();
    if u.is_empty() {
        return ThumbnailQuality::Unknown;
    }
    let lower = u.to_ascii_lowercase();
    if is_tiny_or_icon_url(&lower) {
        return ThumbnailQuality::Small;
    }
    // Provable dimensions win over heuristics: a known-small image must never
    // be upscaled into a large slot, and a known-large one is always fine.
    if let Some((w, h)) = dimensions_hint(&lower) {
        let max_dim = w.max(h);
        if max_dim < TINY_DIM {
            return ThumbnailQuality::Small;
        }
        if max_dim >= MIN_CARD_DIM {
            return ThumbnailQuality::Large;
        }
        // Between TINY_DIM and MIN_CARD_DIM: borderline, treat as small to avoid
        // blurry upscales in hero/grid slots.
        return ThumbnailQuality::Small;
    }
    let score = thumbnail_score(url);
    if score == 0 {
        return ThumbnailQuality::Small;
    }
    if let Some(w) = width_hint(&lower) {
        if w >= MIN_CARD_DIM {
            return ThumbnailQuality::Large;
        }
        if w < TINY_DIM {
            return ThumbnailQuality::Small;
        }
    }
    if score >= 50 {
        return ThumbnailQuality::Large;
    }
    if score >= 30 {
        return ThumbnailQuality::Unknown;
    }
    ThumbnailQuality::Small
}

/// Optional HEAD probe: Content-Length under 8 KiB → [`ThumbnailQuality::Small`].
pub async fn thumbnail_quality_with_head(client: &reqwest::Client, url: &str) -> ThumbnailQuality {
    let base = thumbnail_quality(url);
    if base != ThumbnailQuality::Unknown {
        return base;
    }
    let resp = match client
        .head(url.trim())
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return ThumbnailQuality::Unknown,
    };
    if let Some(len) = resp.content_length() {
        if len < 8 * 1024 {
            return ThumbnailQuality::Small;
        }
        if len >= 32 * 1024 {
            return ThumbnailQuality::Large;
        }
    }
    ThumbnailQuality::Unknown
}

/// True when quality is [`ThumbnailQuality::Large`].
pub fn is_large_thumbnail(url: &str) -> bool {
    thumbnail_quality(url) == ThumbnailQuality::Large
}

/// Google News default/branding images — shared RSS placeholder or GN chrome, not article art.
pub fn is_google_news_branding_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if lower.contains("news.google.com")
        || lower.contains("google.com/images/branding")
        || lower.contains("google.com/logos")
        || lower.contains("gstatic.com/images/branding")
    {
        return true;
    }
    if !lower.contains("googleusercontent.com") {
        return false;
    }
    lower.contains("j6_cofbogxhri9im864nl_ligxvsqp2aupskei7z0cnnfdvgumwuy20nuuhkreqyrp")
}

/// Score a thumbnail URL; `0` means unusable for card display.
pub fn thumbnail_score(url: &str) -> i32 {
    let u = url.trim();
    if u.is_empty() || is_google_news_branding_url(u) {
        return 0;
    }
    let lower = u.to_ascii_lowercase();

    if is_tiny_or_icon_url(&lower) {
        return 0;
    }

    // Provably small dimensions are unusable for a card slot.
    if let Some((w, h)) = dimensions_hint(&lower) {
        if w.max(h) < TINY_DIM {
            return 0;
        }
    }

    let mut score = 50;

    if lower.contains("upload.wikimedia.org") || lower.contains("wikinews/en/thumb") {
        score += 40;
    }
    if lower.contains("bing.net") || lower.contains("th.bing.com") {
        score += 25;
    }
    if lower.contains("wp.com") || lower.contains("wordpress.com") {
        score += 20;
    }
    if lower.contains("pictrs/image")
        || lower.contains("i.redd.it/")
        || lower.contains("i.imgur.com/")
    {
        score += 15;
    }

    if let Some(w) = width_hint(&lower) {
        if w >= 400 {
            score += 40;
        } else if w >= 200 {
            score += 25;
        } else if w >= 100 {
            score += 10;
        } else if w < 64 {
            return 0;
        }
    }

    score
}

/// True when the URL is suitable for a news card hero/grid thumbnail.
pub fn is_usable_thumbnail_url(url: &str) -> bool {
    !is_google_news_branding_url(url)
        && matches!(
            thumbnail_quality(url),
            ThumbnailQuality::Large | ThumbnailQuality::Unknown
        )
        && thumbnail_score(url) >= 30
}

/// Pick the highest-scoring candidate, ignoring unusable URLs.
pub fn best_thumbnail_url<'a, I>(candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut best: Option<(i32, String)> = None;
    for url in candidates {
        let url = url.trim();
        if url.is_empty() {
            continue;
        }
        if !is_usable_thumbnail_url(url) {
            continue;
        }
        let score = thumbnail_score(url);
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, url.to_string()));
        }
    }
    best.map(|(_, u)| u)
}

/// Keep `existing` unless `candidate` scores higher (and is usable).
pub fn prefer_thumbnail(existing: &str, candidate: &str) -> String {
    if candidate.trim().is_empty() {
        return existing.to_string();
    }
    if existing.trim().is_empty() {
        return if is_usable_thumbnail_url(candidate) {
            candidate.to_string()
        } else {
            String::new()
        };
    }
    let existing_score = thumbnail_score(existing);
    let candidate_score = thumbnail_score(candidate);
    if candidate_score > existing_score {
        candidate.to_string()
    } else {
        existing.to_string()
    }
}

fn is_tiny_or_icon_url(lower: &str) -> bool {
    // Reject YouTube video URLs (not images)
    if lower.contains("youtube.com/v/")
        || lower.contains("youtube.com/watch")
        || lower.contains("youtube.com/embed")
        || lower.contains("youtu.be/")
    {
        return true;
    }
    if lower.contains("icons.duckduckgo.com/ip3/")
        || lower.ends_with(".ico")
        || lower.contains("favicon")
        || lower.contains("apple-touch-icon")
        || lower.contains("/icon/")
        || lower.contains("/icons/")
        || lower.contains("/logo")
        || lower.contains("default-logo")
        || lower.contains("default_logo")
        || lower.contains("placeholder-logo")
        || lower.contains("sprite")
        || lower.contains("spacer")
        || lower.contains("/avatar")
        || lower.contains("gravatar.com")
        || lower.contains("google.com/s2/favicons")
        || lower.contains("placeholder")
        || lower.contains("no-image")
        || lower.contains("noimage")
        || lower.contains("imagena.")
        || lower.contains("blank.gif")
        || lower.contains("blank.png")
        || lower.contains("transparent.")
        || lower.contains("pixel.gif")
        || lower.contains("1x1")
        || lower.contains("16x16")
        || lower.contains("24x24")
        || lower.contains("32x32")
        || lower.contains("48x48")
        || lower.contains("64x64")
        || lower.contains("96x96")
        || lower.contains("w=16")
        || lower.contains("h=16")
        || lower.contains("width=16")
        || lower.contains("height=16")
        || lower.contains("=w16")
        || lower.contains("=h16")
    {
        return true;
    }
    // Google user-content / Photos sizing param `=sNN` (square) — small when < TINY_DIM.
    if let Some(n) = sizing_param(lower, "=s") {
        if n < TINY_DIM {
            return true;
        }
    }
    false
}

/// Parse an explicit `WIDTHxHEIGHT` pair from common CDN/WordPress URL shapes,
/// e.g. `...-150x150.jpg`, `.../800x600/...`. Returns `None` when absent.
fn dimensions_hint(lower: &str) -> Option<(u32, u32)> {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // Expect an 'x' separator then more digits.
            if i < bytes.len() && bytes[i] == b'x' {
                let w: u32 = lower[start..i].parse().ok()?;
                let hstart = i + 1;
                let mut j = hstart;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > hstart {
                    if let Ok(h) = lower[hstart..j].parse::<u32>() {
                        // Plausible pixel dimensions only (avoid matching ids).
                        if (16..=10000).contains(&w) && (16..=10000).contains(&h) {
                            return Some((w, h));
                        }
                    }
                }
                i = j;
                continue;
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Extract the integer following a sizing token like `=s`, `=w`, `=h`.
fn sizing_param(lower: &str, token: &str) -> Option<u32> {
    let idx = lower.find(token)?;
    let rest = &lower[idx + token.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok().filter(|n| *n > 0)
}

fn width_hint(lower: &str) -> Option<u32> {
    for token in ["width=", "w=", "=w", "=s", "imwidth="] {
        if let Some(w) = sizing_param(lower, token) {
            return Some(w);
        }
    }
    // Wikimedia thumb paths: .../330px-Filename.jpg
    if let Some(idx) = lower.rfind('/') {
        let rest = &lower[idx + 1..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(w) = digits.parse::<u32>() {
            if w >= 64 {
                return Some(w);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wikinews_fixture_thumb_is_large_enough() {
        let url = "https://upload.wikimedia.org/wikipedia/commons/thumb/1/1b/Battery_icon.svg/320px-Battery_icon.svg.png";
        assert_eq!(thumbnail_quality(url), ThumbnailQuality::Large);
        assert!(is_usable_thumbnail_url(url));
    }

    #[test]
    fn rejects_favicons_and_tiny_icons() {
        assert_eq!(
            thumbnail_quality("https://icons.duckduckgo.com/ip3/example.com.ico"),
            ThumbnailQuality::Small
        );
        assert_eq!(
            thumbnail_quality("https://example.com/favicon.ico"),
            ThumbnailQuality::Small
        );
        assert_eq!(
            thumbnail_quality("https://example.com/apple-touch-icon.png"),
            ThumbnailQuality::Small
        );
        assert!(!is_usable_thumbnail_url(
            "https://icons.duckduckgo.com/ip3/washingtonpost.com.ico"
        ));
    }

    #[test]
    fn wikimedia_thumb_path_does_not_misparsed_as_1px() {
        let url = "https://upload.wikimedia.org/wikipedia/commons/thumb/1/1b/Battery_icon.svg/320px-Battery_icon.svg.png";
        assert!(is_usable_thumbnail_url(url));
        assert_eq!(thumbnail_quality(url), ThumbnailQuality::Large);
    }

    #[test]
    fn thumbnail_quality_large_for_wide_assets() {
        let url = "https://upload.wikimedia.org/wikipedia/commons/thumb/a/ab/Example.jpg/640px-Example.jpg";
        assert_eq!(thumbnail_quality(url), ThumbnailQuality::Large);
        assert!(is_usable_thumbnail_url(url));
    }

    #[test]
    fn prefers_large_wikimedia_thumbs() {
        let url = "https://upload.wikimedia.org/wikipedia/commons/thumb/a/ab/Example.jpg/640px-Example.jpg";
        assert!(is_usable_thumbnail_url(url));
        assert!(thumbnail_score(url) > thumbnail_score("https://lemmy.world/pictrs/image/x.png"));
    }

    #[test]
    fn best_thumbnail_picks_highest_score() {
        let best = best_thumbnail_url([
            "https://icons.duckduckgo.com/ip3/foo.com.ico",
            "https://upload.wikimedia.org/wikipedia/commons/thumb/x/y/400px-y.jpg",
            "https://lemmy.world/pictrs/image/abc.png",
        ])
        .unwrap();
        assert!(best.contains("wikimedia.org"));
    }

    #[test]
    fn prefer_thumbnail_keeps_better_existing() {
        let existing = "https://upload.wikimedia.org/wikipedia/commons/thumb/x/y/500px-y.jpg";
        let worse = "https://lemmy.world/pictrs/image/abc.png";
        assert_eq!(prefer_thumbnail(existing, worse), existing);
    }

    #[test]
    fn rejects_explicit_small_wordpress_dimensions() {
        assert_eq!(
            thumbnail_quality("https://cdn.example.com/wp-content/photo-150x150.jpg"),
            ThumbnailQuality::Small
        );
        assert_eq!(
            thumbnail_quality("https://cdn.example.com/photo-90x90.png"),
            ThumbnailQuality::Small
        );
        assert!(!is_usable_thumbnail_url(
            "https://cdn.example.com/photo-64x64.png"
        ));
    }

    #[test]
    fn accepts_large_explicit_dimensions() {
        assert_eq!(
            thumbnail_quality("https://cdn.example.com/hero-1200x630.jpg"),
            ThumbnailQuality::Large
        );
        assert_eq!(
            thumbnail_quality("https://images.example.com/2024/05/photo-800x600.webp"),
            ThumbnailQuality::Large
        );
    }

    #[test]
    fn rejects_google_news_branding_placeholder() {
        let gn = "https://lh3.googleusercontent.com/J6_coFbogxhRI9iM864NL_liGXvsQp2AupsKei7z0cNNfDvGUmWUy20nuUhkREQyrpY4bEeIBuc=s0-w300-rw";
        assert!(is_google_news_branding_url(gn));
        assert!(!is_usable_thumbnail_url(gn));
        assert!(is_usable_thumbnail_url(
            "https://cdn.example.com/hero-1200x630.jpg"
        ));
    }

    #[test]
    fn rejects_small_google_sizing_param() {
        assert_eq!(
            thumbnail_quality("https://lh3.googleusercontent.com/abc=s64-c"),
            ThumbnailQuality::Small
        );
        assert_eq!(
            thumbnail_quality("https://lh3.googleusercontent.com/abc=s400-c"),
            ThumbnailQuality::Large
        );
    }

    #[test]
    fn rejects_avatar_and_sprite_assets() {
        assert!(!is_usable_thumbnail_url(
            "https://www.gravatar.com/avatar/abc"
        ));
        assert!(!is_usable_thumbnail_url(
            "https://example.com/assets/sprite.png"
        ));
    }

    #[test]
    fn is_tiny_dimension_only_drops_provably_small() {
        assert!(is_tiny_dimension(48, 48));
        assert!(is_tiny_dimension(99, 80));
        assert!(!is_tiny_dimension(100, 60));
        assert!(!is_tiny_dimension(1024, 768));
        // Unknown dimensions must never be treated as tiny.
        assert!(!is_tiny_dimension(0, 0));
        assert!(!is_tiny_dimension(0, 600));
    }

    #[test]
    fn rejects_placeholder_and_blank_assets() {
        assert!(!is_usable_thumbnail_url(
            "https://cdn.example.com/placeholder-image.png"
        ));
        assert!(!is_usable_thumbnail_url("https://example.com/blank.gif"));
        assert!(!is_usable_thumbnail_url(
            "https://example.com/assets/no-image.jpg"
        ));
        assert!(!is_usable_thumbnail_url(
            "https://example.com/spacer-transparent.png"
        ));
        // Wikinews "no image available" placeholder rendered at a large size.
        assert!(!is_usable_thumbnail_url(
            "https://upload.wikimedia.org/wikipedia/commons/thumb/c/c8/ImageNA.svg/960px-ImageNA.svg.png"
        ));
    }

    #[test]
    fn dimensions_hint_ignores_ids() {
        // 7-digit run is not a plausible dimension pair.
        assert_eq!(
            dimensions_hint("https://example.com/12345678/photo.jpg"),
            None
        );
    }
}
