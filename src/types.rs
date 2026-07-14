//! Result types.
//!
//! [`SearchResult`] is serialized to match the standard JSON result object exactly
//! (see a live search instance: `GET /search?q=...&format=json`), so existing
//! search clients can talk to this engine unchanged. [`Answer`] and [`Infobox`]
//! mirror the standard `answers`/`infoboxes` entries.

use serde::{Deserialize, Serialize};

/// A raw result produced by a single engine, before aggregation.
///
/// Engines return these in relevance order; the aggregator assigns the
/// 1-indexed position from the order within each engine's list.
#[derive(Debug, Clone)]
pub struct EngineResult {
    pub url: String,
    pub title: String,
    pub content: String,
    pub img_src: Option<String>,
    pub thumbnail: Option<String>,
    pub published_date: Option<String>,
    pub template: Option<String>,
    pub category: Option<String>,
    /// standard priority hint (e.g. "high"); usually empty.
    pub priority: Option<String>,
    /// Publisher homepage from RSS `<source url="">` (Google News etc.).
    pub publisher_url: Option<String>,
    /// Language code from DB (e.g. "ko", "sl")
    pub language: Option<String>,
}

impl EngineResult {
    /// Convenience constructor for the common "url + title + snippet" case.
    pub fn new(
        url: impl Into<String>,
        title: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        EngineResult {
            url: url.into(),
            title: title.into(),
            content: content.into(),
            img_src: None,
            thumbnail: None,
            published_date: None,
            template: None,
            category: None,
            priority: None,
            publisher_url: None,
            language: None,
        }
    }

    /// Builder: mark this result as belonging to the image template/category.
    pub fn image(mut self, img_src: impl Into<String>, thumbnail: impl Into<String>) -> Self {
        self.img_src = Some(img_src.into());
        self.thumbnail = Some(thumbnail.into());
        self.template = Some("images.html".into());
        self.category = Some("images".into());
        self
    }
}

/// An aggregated, de-duplicated, scored result. Field names and shape mirror
/// so the JSON API is drop-in compatible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResult {
    pub url: String,
    pub title: String,
    pub content: String,
    /// The first engine that produced this result.
    pub engine: String,
    pub template: String,
    /// `[scheme, netloc, path, params, query, fragment]` (Python `urlparse` order).
    pub parsed_url: [String; 6],
    pub img_src: String,
    pub thumbnail: String,
    pub priority: String,
    /// Every engine that returned this URL.
    pub engines: Vec<String>,
    /// 1-indexed positions this URL held in each contributing engine.
    pub positions: Vec<usize>,
    pub score: f64,
    pub category: String,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<String>,
    /// A resolved favicon URL for the result domain (extension over standard).
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub favicon: String,
    /// Topic-cluster index assigned by the optional clustering pass.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cluster: Option<u32>,
    /// Optional one-line summary card (Tier 5 differentiation; AI-generated or
    /// derived from the snippet). Omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    /// Query terms highlighted within the snippet (source-highlight metadata).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub highlights: Vec<String>,
    /// Publisher homepage when the engine provides one (Google News RSS `<source url>`).
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub publisher_url: String,
}

/// A standard instant answer (calculator output, definition, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub answer: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub url: Option<String>,
    pub engine: String,
    pub template: String,
}

impl Answer {
    pub fn new(answer: impl Into<String>, engine: impl Into<String>) -> Self {
        Answer {
            answer: answer.into(),
            url: None,
            engine: engine.into(),
            template: "answer/legacy.html".into(),
        }
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }
}

/// A labelled link inside an infobox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoboxUrl {
    pub title: String,
    pub url: String,
}

/// A `label: value` attribute row inside an infobox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoboxAttribute {
    pub label: String,
    pub value: String,
}

/// A standard infobox (knowledge panel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Infobox {
    pub infobox: String,
    pub id: String,
    pub content: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub img_src: String,
    pub urls: Vec<InfoboxUrl>,
    pub attributes: Vec<InfoboxAttribute>,
    pub engine: String,
    pub engines: Vec<String>,
}

impl Infobox {
    pub fn new(
        title: impl Into<String>,
        content: impl Into<String>,
        engine: impl Into<String>,
    ) -> Self {
        let engine = engine.into();
        Infobox {
            infobox: title.into(),
            id: String::new(),
            content: content.into(),
            img_src: String::new(),
            urls: Vec::new(),
            attributes: Vec::new(),
            engines: vec![engine.clone()],
            engine,
        }
    }
}
