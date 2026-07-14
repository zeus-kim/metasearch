//! A self-hostable, privacy-first answer engine built on a metasearch core.
//!
//! Queries 200+ search engines concurrently, de-duplicates by URL,
//! ranks with the standard scoring formula, and exposes a compatible JSON
//! API. Engines are pluggable and configured via a `settings.yml`-style file.
//!
//! On top of feature parity it adds optional, opt-in LLM enhancements that talk
//! **only** to a user-configured local Ollama and degrade gracefully when no
//! model is reachable: cited answer synthesis (one-shot and token-streaming),
//! query expansion, follow-up suggestions, semantic re-ranking, clustering, and
//! vision captioning.
//!
//! The crate carries no GUI/Tauri dependency, so it runs as a standalone HTTP
//! server ([`serve`]), a terminal client (`cargo run --bin ask`), or as a
//! library embedded in another project.
//!
//! # Library usage
//!
//! The primary programmatic entry points are [`Settings`] (config loading) and
//! [`search_all`] (the full search + enhancement pipeline). A long-lived
//! [`Runtime`] holds the shared HTTP client, cache, rate limiter and health
//! tracker; build it once and reuse it across requests.
//!
//! ```no_run
//! # async fn demo() {
//! use metasearch::{Settings, Runtime, SearchParams, search_all};
//!
//! // Load config (env overlays applied); falls back to built-in defaults.
//! let settings = Settings::load_or_default("settings.yml");
//! let runtime = Runtime::new(&settings);
//!
//! let params = SearchParams::new("rust async runtimes");
//! let response = search_all(&params, &settings, &runtime).await;
//! for r in response.results.iter().take(5) {
//!     println!("{} — {}", r.title, r.url);
//! }
//! # }
//! ```
//!
//! For a cited answer, pass the results to
//! [`ai::synthesize_answer`] (one-shot) or [`ai::stream_answer`] (token stream),
//! and build the structured source list with [`ai::build_citations`].

pub mod aggregate;
pub mod ai;
pub mod answerers;
pub mod api;
pub mod article;
pub mod article_analysis;
pub mod build_info;
pub mod cache;
pub mod config;
pub mod engines;
pub mod feeds;
pub mod googlenews_decode;
pub mod health;
pub mod logging;
pub mod news_article;
pub mod news_digest;
pub mod obs;
pub mod personalization;
pub mod query;
pub mod ratelimit;
pub mod search;
pub mod server;
pub mod thumbnail;
pub mod types;
pub mod url_safety;

pub use ai::{
    build_citations, stream_answer, suggest_followups, AnswerChunk, Citation, GroundedAnswer,
};
pub use config::{AiSettings, Settings};
pub use health::{FailureClass, HealthInfo, HealthTracker};
pub use query::{parse as parse_query, ParsedQuery};
pub use search::{autocomplete, build_client, search_all, Runtime, SearchParams, SearchResponse};
pub use server::serve;
pub use types::{Answer, EngineResult, Infobox, SearchResult};
