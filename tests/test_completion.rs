use std::path::Path;

use clj_pulse::handlers::completion::complete_symbols;
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
fn test_completes_symbols_in_current_ns() {
    let index = build_test_index();
    let completions = complete_symbols(&index, "add", "simple.core");
    assert!(completions.iter().any(|c| c.label == "add"));
    assert!(!completions.iter().any(|c| c.label == "add-and-double"));
}

#[test]
fn test_completes_with_alias_prefix() {
    let index = build_test_index();
    let completions = complete_symbols(&index, "core/ad", "simple.utils");
    assert!(completions.iter().any(|c| c.label == "core/add"));
}

#[test]
fn test_completes_clojure_core_builtins() {
    let index = Index::new_with_core();
    let completions = complete_symbols(&index, "map", "any.ns");
    assert!(completions.iter().any(|c| c.label == "map"));
    assert!(completions.iter().any(|c| c.label == "mapv"));
    assert!(completions.iter().any(|c| c.label == "map-indexed"));
}

#[test]
fn test_completion_item_has_doc_and_detail() {
    let index = build_test_index();
    let completions = complete_symbols(&index, "add", "simple.core");
    let item = completions.iter().find(|c| c.label == "add").unwrap();
    assert!(item.detail.is_some());
    assert!(item.documentation.is_some());
}

#[test]
fn test_empty_prefix_returns_all_visible_symbols() {
    let index = build_test_index();
    let completions = complete_symbols(&index, "", "simple.core");
    assert!(completions.len() >= 3);
}

#[test]
fn test_completes_alias_names() {
    let index = build_test_index();
    // simple.utils requires [simple.core :as core]
    let completions = complete_symbols(&index, "co", "simple.utils");
    let alias = completions.iter().find(|c| c.label == "core").unwrap();
    assert_eq!(alias.detail.as_deref(), Some("alias for simple.core"));
}

#[test]
fn test_completes_namespace_names() {
    let index = build_test_index();
    // typing inside (:require [simple. …]) completes known namespaces
    let completions = complete_symbols(&index, "simple.", "simple.utils");
    assert!(completions.iter().any(|c| c.label == "simple.core"));
    assert!(completions.iter().any(|c| c.label == "simple.utils"));
}

#[test]
fn test_empty_prefix_excludes_namespace_dump() {
    let index = build_test_index();
    let completions = complete_symbols(&index, "", "simple.utils");
    assert!(!completions.iter().any(|c| c.label == "simple.core"));
}
