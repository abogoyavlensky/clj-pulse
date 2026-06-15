use std::path::Path;

use clj_pulse::handlers::{resolve_symbol, ResolvedSymbol};
use clj_pulse::index::scanner;

fn build_test_index() -> clj_pulse::index::Index {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    scanner::build_index(root, &paths).unwrap()
}

#[test]
fn test_resolves_definition_in_same_namespace() {
    let index = build_test_index();
    let result = resolve_symbol(&index, "add", "simple.core");
    match result {
        Some(ResolvedSymbol::Project(sym)) => {
            assert_eq!(sym.fqn, "simple.core/add");
            assert!(sym.file.ends_with("core.clj"));
        }
        other => panic!("expected Project symbol, got {:?}", other),
    }
}

#[test]
fn test_resolves_definition_via_alias() {
    let index = build_test_index();
    let result = resolve_symbol(&index, "core/add", "simple.utils");
    match result {
        Some(ResolvedSymbol::Project(sym)) => {
            assert_eq!(sym.fqn, "simple.core/add");
            assert!(sym.file.ends_with("core.clj"));
        }
        other => panic!("expected Project symbol, got {:?}", other),
    }
}

#[test]
fn test_resolves_definition_via_refer() {
    // Build index with a ns that has :refer imports
    let index = clj_pulse::index::Index::new();
    let source = include_str!("fixtures/snippets/ns_with_requires.clj");
    let (meta, syms) =
        clj_pulse::index::extractor::extract(source, Path::new("ns_with_requires.clj")).unwrap();
    index.insert_file(meta, syms, vec![]);

    // Also add the referred-to symbol
    let utils_source = r#"(ns my.utils) (defn format-date [d] d)"#;
    let (meta2, syms2) =
        clj_pulse::index::extractor::extract(utils_source, Path::new("utils.clj")).unwrap();
    index.insert_file(meta2, syms2, vec![]);

    let result = resolve_symbol(&index, "format-date", "my.service");
    match result {
        Some(ResolvedSymbol::Project(sym)) => {
            assert_eq!(sym.fqn, "my.utils/format-date");
        }
        other => panic!("expected Project symbol, got {:?}", other),
    }
}

#[test]
fn test_returns_none_for_unknown_symbol() {
    let index = build_test_index();
    let result = resolve_symbol(&index, "nonexistent/thing", "simple.core");
    assert!(result.is_none());
}

#[test]
fn test_returns_none_for_unknown_bare_symbol() {
    let index = build_test_index();
    let result = resolve_symbol(&index, "doesnotexist", "simple.core");
    assert!(result.is_none());
}
