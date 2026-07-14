//! Wikisource engine via the MediaWiki search API (JSON, keyless). `general`.
//!
//! Free library of source texts. Shares the MediaWiki `list=search` parser
//! (see [`super::mediawiki_search`]). Language-aware.

use super::EngineContext;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    super::mediawiki_search(ctx, "wikisource.org", "general").await
}

#[cfg(test)]
mod tests {
    use super::super::parse_mediawiki;
    use crate::types::EngineResult;
    use serde_json::Value;

    fn parse(body: &Value) -> Vec<EngineResult> {
        parse_mediawiki(body, "en.wikisource.org", "general")
    }

    #[test]
    fn parses_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/wikisource.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].url.contains("en.wikisource.org/wiki/"));
        assert!(!results[0].content.contains('<'));
    }
}
