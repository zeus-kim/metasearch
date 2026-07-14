//! Query parsing: standard `!bang` engine/category shortcuts and `:lang`
//! selectors.
//!
//! Examples:
//! * `!w einstein`         → search only Wikipedia
//! * `!gh ripgrep`         → search only GitHub
//! * `!images cats`        → restrict to the images category
//! * `:de bundestag`       → force German results
//! * `!w :de berlin`       → Wikipedia, German
//!
//! Bangs and language tokens are stripped from the text actually sent upstream.

/// The outcome of parsing a raw user query.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedQuery {
    /// The cleaned query text (bangs/lang tokens removed).
    pub query: String,
    /// Explicit engine selection from engine bangs (`None` = use categories).
    pub engines: Option<Vec<String>>,
    /// Categories requested via category bangs.
    pub categories: Vec<String>,
    /// Language override from a `:xx` token.
    pub language: Option<String>,
}

/// Map a bang token (without the leading `!`) to an engine name.
fn engine_bang(tok: &str) -> Option<&'static str> {
    Some(match tok {
        "w" | "wp" | "wiki" | "wikipedia" => "wikipedia",
        "wd" | "wikidata" => "wikidata",
        "ddg" | "d" | "duckduckgo" => "duckduckgo",
        "ddl" | "lite" => "duckduckgo_lite",
        "br" | "brave" => "brave",
        "bapi" | "braveapi" => "brave_api",
        "mjk" | "mojeek" => "mojeek",
        "gh" | "github" => "github",
        "se" | "so" | "stack" | "stackexchange" | "stackoverflow" => "stackexchange",
        "arx" | "arxiv" => "arxiv",
        "hn" | "hackernews" => "hackernews",
        "img" | "image" | "images" | "commons" | "wikicommons" => "wikicommons",
        "ddgi" | "ddgimg" | "duckduckgo_images" => "duckduckgo_images",
        "brimg" | "brave_images" => "brave_images",
        "ddv" | "ddgvid" | "duckduckgo_videos" => "duckduckgo_videos",
        "bingimg" | "bing_images" => "bing_images",
        "ov" | "openverse" => "openverse",
        "gl" | "gitlab" => "gitlab",
        "cb" | "codeberg" => "codeberg",
        "cr" | "crate" | "crates" | "cratesio" => "crates_io",
        "npm" => "npm",
        "composer" | "packagist" | "php" => "packagist",
        "gem" | "gems" | "rubygems" => "rubygems",
        "docker" | "dockerhub" => "dockerhub",
        "au" | "askubuntu" | "ubuntu" => "askubuntu",
        "wt" | "wikt" | "wiktionary" | "define" | "def" => "wiktionary",
        "wb" | "wikibooks" => "wikibooks",
        "wq" | "wikiquote" | "quote" => "wikiquote",
        "ws" | "wikisource" => "wikisource",
        "ol" | "openlibrary" | "book" | "books" => "openlibrary",
        "ia" | "archive" | "internetarchive" => "internetarchive",
        "oa" | "openalex" => "openalex",
        "xref" | "crossref" => "crossref",
        "pmc" | "epmc" | "pubmed" | "europepmc" => "europepmc",
        "ss" | "s2" | "semanticscholar" | "scholar" => "semanticscholar",
        "doaj" => "doaj",
        "qw" | "qwant" => "qwant",
        "yx" | "yandex" => "yandex",
        "gnews" | "googlenews" => "googlenews",
        "gdelt" => "gdelt",
        "bn" | "bnews" | "bingnews" => "bingnews",
        "wn" | "wikinews" => "wikinews",
        "lemmy" | "lm" => "lemmy",
        "am" | "music" | "audio" | "bandcamp" => "archive_music",
        "pt" | "peertube" | "vid" | "video" | "videos" => "peertube",
        "osm" | "openstreetmap" | "map" => "openstreetmap",
        _ => return None,
    })
}

/// Map a bang token to a category name.
fn category_bang(tok: &str) -> Option<&'static str> {
    Some(match tok {
        "general" | "web" => "general",
        "images" | "image" | "img" => "images",
        "news" => "news",
        "science" | "sci" => "science",
        "it" | "code" | "dev" => "it",
        "social" | "social_media" => "social",
        "videos" | "video" => "videos",
        "map" | "maps" => "map",
        "music" | "audio" => "music",
        "files" | "file" => "files",
        _ => return None,
    })
}

/// The natural category an engine bang implies (so the results UI renders the
/// matching template/tab). `None` means "no special category".
fn implied_category(engine: &str) -> Option<&'static str> {
    Some(match engine {
        "wikicommons" | "duckduckgo_images" | "bing_images" | "openverse" | "brave_images" => {
            "images"
        }
        "duckduckgo_videos" | "peertube" => "videos",
        "openstreetmap" => "map",
        "archive_music" => "music",
        _ => return None,
    })
}

/// Map a `!!`-prefixed token to an external search-engine URL template (with
/// `{}` where the encoded query goes). This mirrors the standard `!!` "redirect to
/// the external engine" behaviour — the metasearch server bounces the browser
/// straight to that engine's own results page instead of searching locally.
fn external_bang(tok: &str) -> Option<&'static str> {
    Some(match tok {
        "g" | "google" => "https://www.google.com/search?q={}",
        "ddg" | "duckduckgo" => "https://duckduckgo.com/?q={}",
        "b" | "bing" => "https://www.bing.com/search?q={}",
        "bra" | "brave" => "https://search.brave.com/search?q={}",
        "sp" | "startpage" => "https://www.startpage.com/sp/search?query={}",
        "w" | "wiki" | "wikipedia" => "https://en.wikipedia.org/w/index.php?search={}",
        "gh" | "github" => "https://github.com/search?q={}",
        "yt" | "youtube" => "https://www.youtube.com/results?search_query={}",
        "so" | "stackoverflow" => "https://stackoverflow.com/search?q={}",
        "a" | "amazon" => "https://www.amazon.com/s?k={}",
        "gh-code" | "ghcode" => "https://github.com/search?type=code&q={}",
        "mdn" => "https://developer.mozilla.org/en-US/search?q={}",
        "npm" => "https://www.npmjs.com/search?q={}",
        "crates" | "cratesio" => "https://crates.io/search?q={}",
        "yandex" => "https://yandex.com/search/?text={}",
        "maps" | "gmaps" => "https://www.google.com/maps/search/{}",
        "osm" => "https://www.openstreetmap.org/search?query={}",
        _ => return None,
    })
}

/// If `raw` begins with a `!!<bang>` external redirect, return the destination
/// URL (query terms percent-encoded). Returns `None` for ordinary queries.
///
/// Example: `!!g rustlang` → `https://www.google.com/search?q=rustlang`.
pub fn external_redirect(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let rest = raw.strip_prefix("!!")?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let bang = parts.next().unwrap_or("").to_ascii_lowercase();
    let terms = parts.next().unwrap_or("").trim();
    if bang.is_empty() || terms.is_empty() {
        return None;
    }
    let template = external_bang(&bang)?;
    let encoded: String = url::form_urlencoded::byte_serialize(terms.as_bytes()).collect();
    Some(template.replace("{}", &encoded))
}

/// Cheap check that a token looks like a BCP-47-ish language tag (`en`, `en-US`).
fn looks_like_lang(tok: &str) -> bool {
    let core = tok.split('-').next().unwrap_or(tok);
    (2..=3).contains(&core.len()) && core.chars().all(|c| c.is_ascii_alphabetic())
}

/// Parse a raw query into structured search parameters.
pub fn parse(raw: &str) -> ParsedQuery {
    let mut engines: Vec<String> = Vec::new();
    let mut categories: Vec<String> = Vec::new();
    let mut language: Option<String> = None;
    let mut terms: Vec<&str> = Vec::new();

    for tok in raw.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('!') {
            let key = rest.to_ascii_lowercase();
            if let Some(eng) = engine_bang(&key) {
                if !engines.iter().any(|e| e == eng) {
                    engines.push(eng.to_string());
                }
                // Some engine bangs imply a category (so the UI picks the right
                // template/tab), e.g. `!img` → images, `!video` → videos.
                if let Some(cat) = implied_category(eng) {
                    if !categories.iter().any(|c| c == cat) {
                        categories.push(cat.into());
                    }
                }
                continue;
            }
            if let Some(cat) = category_bang(&key) {
                if !categories.iter().any(|c| c == cat) {
                    categories.push(cat.to_string());
                }
                continue;
            }
            // Unknown bang: keep it as a literal term.
            terms.push(tok);
        } else if let Some(rest) = tok.strip_prefix(':') {
            if looks_like_lang(rest) {
                language = Some(rest.to_ascii_lowercase());
            } else {
                terms.push(tok);
            }
        } else {
            terms.push(tok);
        }
    }

    ParsedQuery {
        query: terms.join(" "),
        engines: if engines.is_empty() {
            None
        } else {
            Some(engines)
        },
        categories,
        language,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_query_unchanged() {
        let p = parse("hello world");
        assert_eq!(p.query, "hello world");
        assert!(p.engines.is_none());
        assert!(p.categories.is_empty());
        assert!(p.language.is_none());
    }

    #[test]
    fn engine_bang_selects_engine() {
        let p = parse("!gh ripgrep");
        assert_eq!(p.query, "ripgrep");
        assert_eq!(p.engines, Some(vec!["github".to_string()]));
    }

    #[test]
    fn multiple_engine_bangs_accumulate() {
        let p = parse("!w !wd einstein");
        assert_eq!(p.query, "einstein");
        assert_eq!(
            p.engines,
            Some(vec!["wikipedia".to_string(), "wikidata".to_string()])
        );
    }

    #[test]
    fn category_bang_and_language() {
        let p = parse("!images :de katzen");
        assert_eq!(p.query, "katzen");
        assert_eq!(p.categories, vec!["images".to_string()]);
        assert_eq!(p.language.as_deref(), Some("de"));
    }

    #[test]
    fn image_engine_bang_implies_images_category() {
        let p = parse("!img sunset");
        assert_eq!(p.engines, Some(vec!["wikicommons".to_string()]));
        assert_eq!(p.categories, vec!["images".to_string()]);
    }

    #[test]
    fn region_language_tag() {
        let p = parse(":en-US weather");
        assert_eq!(p.query, "weather");
        assert_eq!(p.language.as_deref(), Some("en-us"));
    }

    #[test]
    fn unknown_bang_kept_as_term() {
        let p = parse("!notabang foo");
        assert_eq!(p.query, "!notabang foo");
        assert!(p.engines.is_none());
    }

    #[test]
    fn new_engine_bangs_resolve() {
        assert_eq!(parse("!cr serde").engines, Some(vec!["crates_io".into()]));
        assert_eq!(parse("!npm express").engines, Some(vec!["npm".into()]));
        assert_eq!(parse("!gl runner").engines, Some(vec!["gitlab".into()]));
        assert_eq!(
            parse("!oa attention").engines,
            Some(vec!["openalex".into()])
        );
        assert_eq!(parse("!lemmy privacy").engines, Some(vec!["lemmy".into()]));
    }

    #[test]
    fn added_engine_bangs_resolve() {
        assert_eq!(parse("!gem rails").engines, Some(vec!["rubygems".into()]));
        assert_eq!(
            parse("!docker nginx").engines,
            Some(vec!["dockerhub".into()])
        );
        assert_eq!(
            parse("!composer monolog").engines,
            Some(vec!["packagist".into()])
        );
        assert_eq!(
            parse("!book rust").engines,
            Some(vec!["openlibrary".into()])
        );
        assert_eq!(
            parse("!ia lecture").engines,
            Some(vec!["internetarchive".into()])
        );
        assert_eq!(
            parse("!scholar attention").engines,
            Some(vec!["semanticscholar".into()])
        );
        assert_eq!(
            parse("!gnews rust").engines,
            Some(vec!["googlenews".into()])
        );
        assert_eq!(parse("!gdelt rust").engines, Some(vec!["gdelt".into()]));
        assert_eq!(parse("!wn election").engines, Some(vec!["wikinews".into()]));
        assert_eq!(
            parse("!au broken package").engines,
            Some(vec!["askubuntu".into()])
        );
        assert_eq!(
            parse("!wq einstein").engines,
            Some(vec!["wikiquote".into()])
        );
        assert_eq!(parse("!doaj open").engines, Some(vec!["doaj".into()]));
    }

    #[test]
    fn major_web_engine_bangs_resolve() {
        assert_eq!(parse("!bapi rust").engines, Some(vec!["brave_api".into()]));
        assert_eq!(parse("!qw rust").engines, Some(vec!["qwant".into()]));
        assert_eq!(parse("!yx rust").engines, Some(vec!["yandex".into()]));
        assert_eq!(parse("!yandex rust").engines, Some(vec!["yandex".into()]));
        assert_eq!(
            parse("!bnews election").engines,
            Some(vec!["bingnews".into()])
        );
        // The keyless `brave` scraper bang is unaffected.
        assert_eq!(parse("!br rust").engines, Some(vec!["brave".into()]));
    }

    #[test]
    fn map_and_video_bangs_imply_category() {
        let p = parse("!map eiffel tower");
        assert_eq!(p.engines, Some(vec!["openstreetmap".to_string()]));
        assert_eq!(p.categories, vec!["map".to_string()]);
        let p = parse("!video linux");
        assert_eq!(p.engines, Some(vec!["peertube".to_string()]));
        assert_eq!(p.categories, vec!["videos".to_string()]);
    }

    #[test]
    fn new_video_and_music_bangs_resolve() {
        assert_eq!(
            parse("!ddv rust").engines,
            Some(vec!["duckduckgo_videos".into()])
        );
        assert_eq!(
            parse("!brimg cat").engines,
            Some(vec!["brave_images".into()])
        );
        assert_eq!(
            parse("!am ambient").engines,
            Some(vec!["archive_music".into()])
        );
    }

    #[test]
    fn files_and_music_category_bangs() {
        assert_eq!(parse("!files pdf").categories, vec!["files".to_string()]);
        assert_eq!(parse("!music jazz").categories, vec!["music".to_string()]);
    }

    #[test]
    fn external_redirect_builds_url() {
        assert_eq!(
            external_redirect("!!g rustlang").as_deref(),
            Some("https://www.google.com/search?q=rustlang")
        );
        assert_eq!(
            external_redirect("!!gh ripgrep").as_deref(),
            Some("https://github.com/search?q=ripgrep")
        );
        // Multi-word terms are percent-encoded.
        let u = external_redirect("!!ddg borrow checker").unwrap();
        assert!(u.starts_with("https://duckduckgo.com/?q="));
        assert!(u.contains("borrow") && u.contains("checker"));
    }

    #[test]
    fn external_redirect_ignores_plain_and_single_bang() {
        assert!(external_redirect("hello world").is_none());
        assert!(external_redirect("!g single bang is internal").is_none());
        assert!(external_redirect("!!notabang foo").is_none());
        assert!(external_redirect("!!g").is_none()); // no terms
    }
}
