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
