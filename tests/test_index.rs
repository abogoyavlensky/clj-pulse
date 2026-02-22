use std::path::Path;

use clj_lsp::index::extractor;
use clj_lsp::index::scanner;

#[test]
fn test_indexes_all_files_in_project() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index = scanner::build_index(root, &paths).unwrap();

    assert!(index.lookup("simple.core/add").is_some());
    assert!(index.lookup("simple.core/multiply").is_some());
    assert!(index.lookup("simple.core/VERSION").is_some());
    assert!(index.lookup("simple.utils/greet").is_some());
    assert!(index.lookup("simple.utils/add-and-double").is_some());
}

#[test]
fn test_index_contains_ns_metadata() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index = scanner::build_index(root, &paths).unwrap();

    let meta = index.ns_meta("simple.utils").unwrap();
    assert_eq!(
        meta.aliases.get("core").map(|s| s.as_str()),
        Some("simple.core")
    );
}

#[test]
fn test_remove_file_cleans_up_all_symbols() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index = scanner::build_index(root, &paths).unwrap();

    assert!(index.lookup("simple.utils/greet").is_some());

    let utils_path = root.join("src/utils.clj");
    index.remove_file(&utils_path);

    assert!(index.lookup("simple.utils/greet").is_none());
    assert!(index.ns_meta("simple.utils").is_none());
}

#[test]
fn test_insert_file_updates_index() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index = scanner::build_index(root, &paths).unwrap();

    let new_source = r#"
        (ns simple.utils (:require [simple.core :as core]))
        (defn new-fn [x] x)
    "#;
    let fake_path = root.join("src/utils.clj");
    index.remove_file(&fake_path);
    let (meta, syms) = extractor::extract(new_source, &fake_path).unwrap();
    index.insert_file(meta, syms);

    assert!(index.lookup("simple.utils/new-fn").is_some());
    assert!(index.lookup("simple.utils/greet").is_none());
}
