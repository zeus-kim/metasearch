//! Ask Ubuntu (Stack Exchange site) search (JSON, keyless). `it` category.
//!
//! A thin wrapper over the shared Stack Exchange adapter (see
//! [`super::stackexchange::search_site`]) parameterized to the `askubuntu`
//! site. Keyless. Parsing is exercised by the Stack Exchange fixture test plus
//! the site-specific fixture below.

use super::EngineContext;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    super::stackexchange::search_site(ctx, "askubuntu").await
}

#[cfg(test)]
mod tests {
    use super::super::stackexchange::parse;
    use serde_json::Value;

    #[test]
    fn parses_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/askubuntu.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].url.contains("askubuntu.com"));
        assert!(results[0].content.contains("score"));
        assert_eq!(results[0].category.as_deref(), Some("it"));
    }
}
