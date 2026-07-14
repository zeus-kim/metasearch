//! Wiktionary engine via the MediaWiki search API (JSON, keyless).
//!
//! Shares the MediaWiki `list=search` parser with the other Wikimedia engines
//! (see [`super::mediawiki_search`]). Language-aware: `:de` searches the German
//! Wiktionary.

use super::EngineContext;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    super::mediawiki_search(ctx, "wiktionary.org", "general").await
}

#[cfg(test)]
mod tests {
    use super::super::parse_mediawiki;
    use crate::types::EngineResult;
    use serde_json::Value;

    fn parse(body: &Value) -> Vec<EngineResult> {
        parse_mediawiki(body, "en.wiktionary.org", "general")
    }

    #[test]
    fn parses_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/wiktionary.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "ostensible");
        assert!(results[0].url.contains("en.wiktionary.org/wiki/ostensible"));
        // HTML stripped from the snippet.
        assert!(!results[0].content.contains('<'));
    }
}
