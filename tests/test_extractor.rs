use clj_lsp::index::extractor::extract;
use clj_lsp::index::DefKind;
use std::path::Path;

#[test]
fn test_extracts_namespace_name() {
    let (meta, _) = extract(
        include_str!("fixtures/snippets/basic_defn.clj"),
        Path::new("basic_defn.clj"),
    )
    .unwrap();
    assert_eq!(meta.name, "my.core");
}

#[test]
fn test_extracts_defn_with_doc_and_params() {
    let (_, syms) = extract(
        include_str!("fixtures/snippets/basic_defn.clj"),
        Path::new("basic_defn.clj"),
    )
    .unwrap();
    let hello = syms
        .iter()
        .find(|s| s.name == "hello")
        .expect("hello not found");
    assert_eq!(hello.kind, DefKind::Defn);
    assert_eq!(hello.fqn, "my.core/hello");
    assert_eq!(hello.doc.as_deref(), Some("Says hello to someone."));
    assert_eq!(hello.params, vec!["[name]"]);
}

#[test]
fn test_extracts_def_and_defmacro() {
    let (_, syms) = extract(
        include_str!("fixtures/snippets/basic_defn.clj"),
        Path::new("basic_defn.clj"),
    )
    .unwrap();
    assert!(syms
        .iter()
        .any(|s| s.name == "PI" && s.kind == DefKind::Def));
    assert!(syms
        .iter()
        .any(|s| s.name == "when-pos" && s.kind == DefKind::Defmacro));
}

#[test]
fn test_extracts_defn_private() {
    let (_, syms) = extract(
        include_str!("fixtures/snippets/basic_defn.clj"),
        Path::new("basic_defn.clj"),
    )
    .unwrap();
    let p = syms.iter().find(|s| s.name == "private-thing").unwrap();
    assert_eq!(p.kind, DefKind::DefnPrivate);
}

#[test]
fn test_extracts_multi_arity_params() {
    let (_, syms) = extract(
        include_str!("fixtures/snippets/multi_arity.clj"),
        Path::new("multi_arity.clj"),
    )
    .unwrap();
    let greet = syms.iter().find(|s| s.name == "greet").unwrap();
    assert_eq!(greet.params.len(), 2);
    assert!(greet.params.contains(&"[name]".to_string()));
    assert!(greet.params.contains(&"[title name]".to_string()));
}

#[test]
fn test_extracts_ns_aliases_and_refers() {
    let (meta, _) = extract(
        include_str!("fixtures/snippets/ns_with_requires.clj"),
        Path::new("ns_with_requires.clj"),
    )
    .unwrap();
    assert_eq!(
        meta.aliases.get("str").map(|s| s.as_str()),
        Some("clojure.string")
    );
    assert_eq!(
        meta.aliases.get("core").map(|s| s.as_str()),
        Some("my.core")
    );
    assert_eq!(
        meta.refers.get("format-date").map(|s| s.as_str()),
        Some("my.utils/format-date")
    );
    assert_eq!(
        meta.refers.get("parse-id").map(|s| s.as_str()),
        Some("my.utils/parse-id")
    );
}

#[test]
fn test_handles_reader_conditionals() {
    let (_, syms) = extract(
        include_str!("fixtures/snippets/reader_conditional.cljc"),
        Path::new("reader_conditional.cljc"),
    )
    .unwrap();
    assert!(syms.iter().any(|s| s.name == "read-file"));
    assert!(syms.iter().any(|s| s.name == "shared-fn"));
}

#[test]
fn test_name_range_is_just_name_not_full_form() {
    let (_, syms) = extract(
        include_str!("fixtures/snippets/basic_defn.clj"),
        Path::new("basic_defn.clj"),
    )
    .unwrap();
    let hello = syms.iter().find(|s| s.name == "hello").unwrap();
    // name_range should be narrower than range (which covers the whole defn)
    assert!(
        hello.name_range.start.line == hello.range.start.line
            || hello.name_range.start.character > hello.range.start.character
    );
    assert!(hello.name_range.end.character > hello.name_range.start.character);
}

#[test]
fn test_extracts_defonce() {
    let src = r#"(ns my.app) (defonce state (atom {}))"#;
    let (_, syms) = extract(src, Path::new("app.clj")).unwrap();
    let s = syms.iter().find(|s| s.name == "state").unwrap();
    assert_eq!(s.kind, DefKind::Defonce);
    assert_eq!(s.fqn, "my.app/state");
}

#[test]
fn test_extracts_ns_with_metadata() {
    // Real-world pattern (clojure.core, data.json, …): metadata on the ns name
    let src = "(ns ^{:author \"X\"\n      :doc \"docs\"}\n  my.lib\n  (:require [other.ns :as o]))\n\n(defn run [x] x)";
    let (meta, syms) = extract(src, Path::new("lib.clj")).unwrap();
    assert_eq!(meta.name, "my.lib");
    assert_eq!(meta.aliases.get("o"), Some(&"other.ns".to_string()));
    assert_eq!(syms[0].fqn, "my.lib/run");
}

#[test]
fn test_extracts_def_with_metadata() {
    let src = "(ns my.app)\n(def ^:dynamic *conn* nil)\n(defn ^:deprecated old-fn [x] x)";
    let (_, syms) = extract(src, Path::new("app.clj")).unwrap();

    let conn = syms
        .iter()
        .find(|s| s.name == "*conn*")
        .expect("*conn* extracted");
    assert_eq!(conn.fqn, "my.app/*conn*");
    // name_range must cover just the symbol, not the ^:dynamic metadata
    assert_eq!(conn.name_range.start.character, 15);

    let old = syms
        .iter()
        .find(|s| s.name == "old-fn")
        .expect("old-fn extracted");
    assert_eq!(old.fqn, "my.app/old-fn");
}

#[test]
fn test_ranges_are_utf16_columns() {
    // '😀' is 4 bytes, 2 UTF-16 units, 1 char — ranges must use UTF-16
    let src = "(def smile \"😀\") (defn add [a b] a)";
    let (_, syms) = extract(src, Path::new("u.clj")).unwrap();
    let add = syms.iter().find(|s| s.name == "add").unwrap();

    let name_start = src.find("add").unwrap();
    let expected = src[..name_start].encode_utf16().count() as u32;
    assert_eq!(add.name_range.start.character, expected);
    assert_eq!(add.name_range.end.character, expected + 3);
}
