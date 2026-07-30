use octorus::{language::SupportedLanguage, ParserPool};

#[test]
fn public_tags_query_compatibility_api_remains_available() {
    let source = SupportedLanguage::Rust
        .tags_query()
        .expect("Rust must expose its symbol tags query");
    assert!(source.contains("@definition.function"));

    let mut pool = ParserPool::new();
    assert!(pool
        .get_or_create_tags_query(SupportedLanguage::Rust)
        .is_some());
    assert!(pool
        .get_or_create_tags_query(SupportedLanguage::Css)
        .is_none());
}
