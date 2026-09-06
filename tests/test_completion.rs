use std::path::Path;

use clj_pulse::handlers::completion::complete_symbols;
use clj_pulse::index::scanner;
use clj_pulse::index::Index;

fn build_test_index() -> Index {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let mut index =
        scanner::build_index(root, &paths, &clj_pulse::index::ExtractConfig::default()).unwrap();
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

use clj_pulse::index::{DefKind, NsMeta, Symbol, SymbolSource};
use std::collections::HashMap;
use std::path::PathBuf;
use tower_lsp::lsp_types::{CompletionItemKind, Range};

/// A namespace with no requires beyond what the caller fills in.
fn ns_meta(name: &str) -> NsMeta {
    NsMeta {
        name: name.to_string(),
        file: PathBuf::from(format!("{}.clj", name)),
        aliases: HashMap::new(),
        refers: HashMap::new(),
        requires: vec![],
        imports: HashMap::new(),
        refer_all: vec![],
        as_aliases: vec![],
        core_excludes: vec![],
    }
}

fn macro_sym(name: &str, ns: &str) -> Symbol {
    Symbol {
        name: name.to_string(),
        fqn: format!("{}/{}", ns, name),
        ns: ns.to_string(),
        kind: DefKind::Defmacro,
        params: vec!["[name & body]".to_string()],
        doc: None,
        file: PathBuf::from("clojure/test.clj"),
        source: SymbolSource::Dir(PathBuf::from("clojure")),
        range: Range::default(),
        name_range: Range::default(),
        private: false,
    }
}

#[test]
fn test_completes_referred_name_before_library_is_indexed() {
    // The user explicitly referred `deftest`, so it is valid regardless of
    // whether the clojure JAR has been indexed yet.
    let index = Index::new_with_core();
    let mut meta = ns_meta("a.t");
    meta.refers
        .insert("deftest".to_string(), "clojure.test/deftest".to_string());
    index.insert_file(meta, vec![], vec![]);

    let items = complete_symbols(&index, "deft", "a.t");
    let item = items
        .iter()
        .find(|i| i.label == "deftest")
        .unwrap_or_else(|| panic!("deftest not offered: {:?}", labels(&items)));
    assert_eq!(item.detail.as_deref(), Some("clojure.test (referred)"));
}

#[test]
fn test_completes_refer_all_namespace_symbols() {
    let index = Index::new_with_core();
    index.insert_lib_file(
        ns_meta("clojure.test"),
        vec![
            macro_sym("deftest", "clojure.test"),
            macro_sym("deftest-", "clojure.test"),
            macro_sym("is", "clojure.test"),
        ],
    );
    let mut meta = ns_meta("a.t");
    meta.refer_all.push("clojure.test".to_string());
    index.insert_file(meta, vec![], vec![]);

    let items = complete_symbols(&index, "deft", "a.t");
    let names = labels(&items);
    assert!(names.contains(&"deftest".to_string()), "{:?}", names);
    assert!(names.contains(&"deftest-".to_string()), "{:?}", names);
    assert!(!names.contains(&"is".to_string()), "{:?}", names);

    let deftest = items.iter().find(|i| i.label == "deftest").unwrap();
    assert_eq!(deftest.kind, Some(CompletionItemKind::FUNCTION));
}

#[test]
fn test_refer_all_does_not_duplicate_explicit_refers() {
    let index = Index::new_with_core();
    index.insert_lib_file(
        ns_meta("clojure.test"),
        vec![macro_sym("deftest", "clojure.test")],
    );
    let mut meta = ns_meta("a.t");
    meta.refers
        .insert("deftest".to_string(), "clojure.test/deftest".to_string());
    meta.refer_all.push("clojure.test".to_string());
    index.insert_file(meta, vec![], vec![]);

    let names = labels(&complete_symbols(&index, "deft", "a.t"));
    assert_eq!(
        names.iter().filter(|l| *l == "deftest").count(),
        1,
        "deftest offered more than once: {:?}",
        names
    );
}

fn labels(items: &[tower_lsp::lsp_types::CompletionItem]) -> Vec<String> {
    items.iter().map(|i| i.label.clone()).collect()
}

#[test]
fn test_core_exclude_hides_core_symbol() {
    // `(:refer-clojure :exclude [update])` — `update` here is the file's own
    // var, so core must not offer its version alongside.
    let index = Index::new_with_core();
    let mut meta = ns_meta("a.x");
    meta.core_excludes = vec!["update".to_string()];
    index.insert_file(
        meta,
        vec![Symbol {
            name: "update".to_string(),
            fqn: "a.x/update".to_string(),
            ns: "a.x".to_string(),
            kind: DefKind::Defn,
            params: vec!["[m]".to_string()],
            doc: None,
            file: PathBuf::from("a/x.clj"),
            source: SymbolSource::Project,
            range: Range::default(),
            name_range: Range::default(),
            private: false,
        }],
        vec![],
    );

    let items = complete_symbols(&index, "upd", "a.x");
    assert!(
        !items.iter().any(|i| i.label == "update"
            && i.detail
                .as_deref()
                .is_some_and(|d| d.starts_with("clojure.core"))),
        "core update still offered: {:?}",
        labels(&items)
    );
    assert!(
        items.iter().any(|i| i.label == "update"),
        "own update missing: {:?}",
        labels(&items)
    );
}

/// A plain project defn, so tests can hand-build a namespace's symbol set.
fn defn_sym(name: &str, ns: &str) -> Symbol {
    Symbol {
        name: name.to_string(),
        fqn: format!("{}/{}", ns, name),
        ns: ns.to_string(),
        kind: DefKind::Defn,
        params: vec!["[x]".to_string()],
        doc: None,
        file: PathBuf::from(format!("{}.clj", ns)),
        source: SymbolSource::Project,
        range: Range::default(),
        name_range: Range::default(),
        private: false,
    }
}

#[test]
fn test_fuzzy_substring_match() {
    // `dd` matches `add` in the middle: a tier-2 (substring) hit from the
    // current-namespace pool (1).
    let index = build_test_index();
    let items = complete_symbols(&index, "dd", "simple.core");
    let add = items
        .iter()
        .find(|i| i.label == "add")
        .unwrap_or_else(|| panic!("add not offered for `dd`: {:?}", labels(&items)));
    assert_eq!(add.sort_text.as_deref(), Some("2-1-add"));
}

#[test]
fn test_fuzzy_subsequence_ranks_below_prefix() {
    // `add` prefix-matches `add-more` (tier 1) and subsequence-matches `a-d-d`
    // (tier 3); both live in the current namespace, so sort_text orders them.
    let index = Index::new();
    index.insert_file(
        ns_meta("a.x"),
        vec![defn_sym("add-more", "a.x"), defn_sym("a-d-d", "a.x")],
        vec![],
    );

    let items = complete_symbols(&index, "add", "a.x");
    let sort_text = |label: &str| -> String {
        items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("{} not offered: {:?}", label, labels(&items)))
            .sort_text
            .clone()
            .unwrap_or_else(|| panic!("{} has no sort_text", label))
    };
    let prefix = sort_text("add-more");
    let subsequence = sort_text("a-d-d");
    assert!(
        prefix < subsequence,
        "prefix match must rank first: {} vs {}",
        prefix,
        subsequence
    );
}

#[test]
fn test_fuzzy_single_char_prefix_is_prefix_only() {
    // One character is too little to fuzzy-match on: `d` stays a prefix search,
    // so `add` (a substring hit) is not offered.
    let index = build_test_index();
    let items = complete_symbols(&index, "d", "simple.core");
    assert!(
        !items.iter().any(|i| i.label == "add"),
        "single-char prefix fuzzy-matched: {:?}",
        labels(&items)
    );
}

#[test]
fn test_fuzzy_namespace_pool_is_capped() {
    // Substring matching over namespaces would offer every library namespace
    // sharing the typed text; the pool is capped.
    let index = Index::new();
    for i in 0..60 {
        index.insert_file(ns_meta(&format!("lib{:02}.widget", i)), vec![], vec![]);
    }
    index.insert_file(ns_meta("a.x"), vec![], vec![]);

    let items = complete_symbols(&index, "widget", "a.x");
    let namespaces = items
        .iter()
        .filter(|i| i.detail.as_deref() == Some("namespace"))
        .count();
    assert!(
        namespaces > 0 && namespaces <= 50,
        "namespace pool not capped: {} items",
        namespaces
    );
}

#[test]
fn test_fuzzy_namespace_pool_skips_subsequence() {
    // `str` subsequence-matches hundreds of library namespaces; only prefix and
    // substring hits are namespace candidates.
    let index = Index::new();
    index.insert_file(ns_meta("s.t.r"), vec![], vec![]);
    index.insert_file(ns_meta("clojure.string"), vec![], vec![]);
    index.insert_file(ns_meta("a.x"), vec![], vec![]);

    let names = labels(&complete_symbols(&index, "str", "a.x"));
    assert!(
        names.contains(&"clojure.string".to_string()),
        "substring namespace missing: {:?}",
        names
    );
    assert!(
        !names.contains(&"s.t.r".to_string()),
        "subsequence namespace offered: {:?}",
        names
    );
}
