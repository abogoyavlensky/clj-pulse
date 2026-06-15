use std::path::Path;

use clj_pulse::handlers::hover::{format_for_symbol, resolve_and_format};
use clj_pulse::index::scanner;
use clj_pulse::index::Index;

fn build_test_index() -> Index {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let mut index = scanner::build_index(root, &paths).unwrap();
    index.core_symbols = clj_pulse::index::core::core_symbols();
    index
}

#[test]
fn test_hover_returns_doc_for_known_symbol() {
    let index = build_test_index();
    let result = resolve_and_format(&index, "add", "simple.core").unwrap();
    assert!(result.contains("add"));
    assert!(result.contains("[a b]"));
    assert!(result.contains("Adds two numbers"));
    assert!(result.contains("simple.core"));
}

#[test]
fn test_hover_formats_as_clojure_code_block() {
    let index = build_test_index();
    let sym = index.lookup("simple.core/add").unwrap();
    let md = format_for_symbol(&sym);
    assert!(md.contains("```clojure"));
    assert!(md.contains("```\n"));
}

#[test]
fn test_hover_returns_none_for_unknown() {
    let index = Index::new_with_core();
    let result = resolve_and_format(&index, "nonexistent/fn", "any.ns");
    assert!(result.is_none());
}

#[test]
fn test_hover_for_core_symbol() {
    let index = Index::new_with_core();
    let result = resolve_and_format(&index, "map", "any.ns").unwrap();
    assert!(result.contains("map"));
    assert!(result.contains("clojure.core"));
}

#[test]
fn test_hover_for_def_kind() {
    let index = build_test_index();
    let result = resolve_and_format(&index, "VERSION", "simple.core").unwrap();
    assert!(result.contains("(def VERSION)"));
    assert!(!result.contains("defn"));
}
