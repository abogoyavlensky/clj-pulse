use std::path::Path;

use clj_pulse::handlers::completion::{complete_symbols, resolve};
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
    let completions = complete_symbols(&index, "add", "simple.core", None);
    assert!(completions.iter().any(|c| c.label == "add"));
    assert!(!completions.iter().any(|c| c.label == "add-and-double"));
}

#[test]
fn test_completes_with_alias_prefix() {
    let index = build_test_index();
    let completions = complete_symbols(&index, "core/ad", "simple.utils", None);
    assert!(completions.iter().any(|c| c.label == "core/add"));
}

#[test]
fn test_completes_clojure_core_builtins() {
    let index = Index::new_with_core();
    let completions = complete_symbols(&index, "map", "any.ns", None);
    assert!(completions.iter().any(|c| c.label == "map"));
    assert!(completions.iter().any(|c| c.label == "mapv"));
    assert!(completions.iter().any(|c| c.label == "map-indexed"));
}

#[test]
fn test_completion_item_has_doc_and_detail() {
    // The item carries its signature up front and its docstring only after
    // `completionItem/resolve`.
    let index = build_test_index();
    let completions = complete_symbols(&index, "add", "simple.core", None);
    let item = completions.iter().find(|c| c.label == "add").unwrap();
    assert!(item.detail.is_some());
    assert!(resolve(&index, item.clone()).documentation.is_some());
}

#[test]
fn test_empty_prefix_returns_all_visible_symbols() {
    let index = build_test_index();
    let completions = complete_symbols(&index, "", "simple.core", None);
    assert!(completions.len() >= 3);
}

#[test]
fn test_completes_alias_names() {
    let index = build_test_index();
    // simple.utils requires [simple.core :as core]
    let completions = complete_symbols(&index, "co", "simple.utils", None);
    let alias = completions.iter().find(|c| c.label == "core").unwrap();
    assert_eq!(alias.detail.as_deref(), Some("alias for simple.core"));
}

#[test]
fn test_completes_namespace_names() {
    let index = build_test_index();
    // typing inside (:require [simple. …]) completes known namespaces
    let completions = complete_symbols(&index, "simple.", "simple.utils", None);
    assert!(completions.iter().any(|c| c.label == "simple.core"));
    assert!(completions.iter().any(|c| c.label == "simple.utils"));
}

#[test]
fn test_empty_prefix_excludes_namespace_dump() {
    let index = build_test_index();
    let completions = complete_symbols(&index, "", "simple.utils", None);
    assert!(!completions.iter().any(|c| c.label == "simple.core"));
}

use clj_pulse::index::CoreSymbol;
use clj_pulse::index::{DefKind, NsMeta, Symbol, SymbolSource};
use std::collections::HashMap;
use std::path::PathBuf;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, Range};

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

    let items = complete_symbols(&index, "deft", "a.t", None);
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

    let items = complete_symbols(&index, "deft", "a.t", None);
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

    let names = labels(&complete_symbols(&index, "deft", "a.t", None));
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

    let items = complete_symbols(&index, "upd", "a.x", None);
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
    let items = complete_symbols(&index, "dd", "simple.core", None);
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

    let items = complete_symbols(&index, "add", "a.x", None);
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
    let items = complete_symbols(&index, "d", "simple.core", None);
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

    let items = complete_symbols(&index, "widget", "a.x", None);
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

    let names = labels(&complete_symbols(&index, "str", "a.x", None));
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

/// The Markdown body of a resolved item's documentation.
fn doc_value(item: &tower_lsp::lsp_types::CompletionItem) -> String {
    match item.documentation.as_ref() {
        Some(tower_lsp::lsp_types::Documentation::MarkupContent(m)) => m.value.clone(),
        other => panic!("expected markup documentation, got {:?}", other),
    }
}

fn item_named(items: &[tower_lsp::lsp_types::CompletionItem], label: &str) -> CompletionItem {
    items
        .iter()
        .find(|i| i.label == label)
        .unwrap_or_else(|| panic!("{} not offered: {:?}", label, labels(items)))
        .clone()
}

/// A let-go project: special forms, native core fns and the live `.lg` `core`.
fn letgo_index() -> Index {
    let mut index = Index::new();
    index.insert_file(ns_meta("app"), vec![], vec![]);
    index.insert_lib_file(ns_meta("core"), vec![]);
    index.core_symbols = vec![CoreSymbol {
        name: "count".to_string(),
        params: "([coll])".to_string(),
        doc: "Returns the number of items in the collection.".to_string(),
    }];
    index.mark_letgo_core();
    index.set_letgo_native(vec!["count".to_string()]);
    index
}

#[test]
fn test_items_carry_data_not_documentation() {
    let index = build_test_index();
    let item = item_named(&complete_symbols(&index, "add", "simple.core", None), "add");
    assert!(
        item.documentation.is_none(),
        "documentation sent up front: {:?}",
        item.documentation
    );
    assert_eq!(
        item.data,
        Some(serde_json::json!({ "src": "symbol", "fqn": "simple.core/add" }))
    );
}

#[test]
fn test_resolve_fills_symbol_documentation() {
    let index = build_test_index();
    let item = item_named(&complete_symbols(&index, "add", "simple.core", None), "add");
    assert!(
        doc_value(&resolve(&index, item)).contains("Adds two numbers"),
        "docstring missing after resolve"
    );
}

#[test]
fn test_resolve_fills_core_documentation() {
    let index = Index::new_with_core();
    let item = item_named(&complete_symbols(&index, "map", "any.ns", None), "map");
    assert!(item.documentation.is_none());
    assert!(!doc_value(&resolve(&index, item)).is_empty());
}

#[test]
fn test_resolve_fills_special_form_documentation() {
    let index = Index::new_with_core();
    let item = item_named(&complete_symbols(&index, "if", "any.ns", None), "if");
    assert!(item.documentation.is_none());
    assert!(
        doc_value(&resolve(&index, item)).contains("Evaluates"),
        "special form doc missing after resolve"
    );
}

#[test]
fn test_resolve_fills_letgo_native_documentation() {
    let index = letgo_index();
    let item = item_named(&complete_symbols(&index, "count", "app", None), "count");
    assert!(item.documentation.is_none());
    assert!(doc_value(&resolve(&index, item)).contains("number of items"));
}

#[test]
fn test_resolve_passes_unknown_item_through() {
    // A namespace item has nothing to resolve, so it comes back untouched.
    let index = build_test_index();
    let item = item_named(
        &complete_symbols(&index, "simple.", "simple.utils", None),
        "simple.core",
    );
    assert_eq!(item.data, None);
    assert_eq!(resolve(&index, item.clone()), item);
}

#[test]
fn test_resolve_ignores_malformed_data() {
    let index = build_test_index();
    let base = item_named(&complete_symbols(&index, "add", "simple.core", None), "add");
    for data in [
        serde_json::json!("simple.core/add"),
        serde_json::json!({ "fqn": "simple.core/add" }),
        serde_json::json!({ "src": "nonsense", "fqn": "simple.core/add" }),
        serde_json::json!({ "src": "symbol", "fqn": "simple.core/nope" }),
    ] {
        let item = CompletionItem {
            data: Some(data.clone()),
            ..base.clone()
        };
        assert_eq!(
            resolve(&index, item.clone()),
            item,
            "malformed data changed the item: {}",
            data
        );
    }
}

// --- keyword completion -----------------------------------------------------

use clj_pulse::document::KeywordContext;
use clj_pulse::handlers::completion::complete_keywords;
use tower_lsp::lsp_types::Position;

/// A keyword context as `DocumentStore::keyword_at` would build it, with the
/// span the item replaces taken from the typed text alone.
fn kw_ctx(auto_resolved: bool, text: &str) -> KeywordContext {
    let marker = if auto_resolved { 2 } else { 1 };
    let end = marker + text.chars().count() as u32;
    KeywordContext {
        auto_resolved,
        text: text.to_string(),
        start: Position::new(0, 0),
        end: Position::new(0, end),
    }
}

fn kw_labels(index: &Index, ctx: &KeywordContext, current_ns: &str) -> Vec<String> {
    let meta = index.ns_meta(current_ns);
    complete_keywords(index, ctx, current_ns, meta.as_ref())
        .into_iter()
        .map(|i| i.label)
        .collect()
}

#[test]
fn test_keywords_auto_resolved_offers_current_ns_and_aliases() {
    // `::` offers this namespace's own keywords in `::name` form, plus every
    // alias as `::alias/` so the user can carry on typing.
    let index = build_test_index();
    let labels = kw_labels(&index, &kw_ctx(true, ""), "simple.keywords");
    assert!(labels.contains(&"::local".to_string()), "{:?}", labels);
    assert!(labels.contains(&"::c/".to_string()), "{:?}", labels);
    // A keyword of another namespace is not reachable through a bare `::`.
    assert!(
        !labels.iter().any(|l| l.contains("thing")),
        "foreign keyword offered on bare `::`: {:?}",
        labels
    );
}

#[test]
fn test_keywords_auto_resolved_through_alias() {
    let index = build_test_index();
    let labels = kw_labels(&index, &kw_ctx(true, "c/th"), "simple.keywords");
    assert_eq!(labels, vec!["::c/thing".to_string()]);
}

#[test]
fn test_keywords_single_colon_ranks_by_frequency() {
    // `:` offers every keyword the project uses; the fixture writes `:id`
    // three times and `:name` once, so `:id` comes first.
    let index = build_test_index();
    let labels = kw_labels(&index, &kw_ctx(false, ""), "simple.utils");
    let id = labels.iter().position(|l| l == ":id").expect("no :id");
    let name = labels.iter().position(|l| l == ":name").expect("no :name");
    assert!(id < name, "`:id` is used more often: {:?}", labels);
    // Qualified keywords keep their own notation under a single colon.
    assert!(
        labels.contains(&":simple.keywords/local".to_string()),
        "{:?}",
        labels
    );
}

#[test]
fn test_keywords_single_colon_prefix_filters() {
    let index = build_test_index();
    let labels = kw_labels(&index, &kw_ctx(false, "na"), "simple.utils");
    assert!(labels.contains(&":name".to_string()), "{:?}", labels);
    assert!(!labels.contains(&":id".to_string()), "{:?}", labels);
}

#[test]
fn test_keywords_current_ns_sorts_before_foreign() {
    // Within a match tier, this namespace's keywords come first.
    let index = build_test_index();
    let meta = index.ns_meta("simple.keywords");
    let items = complete_keywords(&index, &kw_ctx(false, ""), "simple.keywords", meta.as_ref());
    let sort_of = |label: &str| {
        items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("{} not offered", label))
            .sort_text
            .clone()
            .expect("sort_text")
    };
    assert!(
        sort_of(":simple.keywords/local") < sort_of(":id"),
        "current-ns keyword must sort first"
    );
}

#[test]
fn test_keyword_item_replaces_the_whole_token() {
    // The edit spans the token, not just the typed half, and the item carries
    // its usage count and a KEYWORD kind.
    let index = build_test_index();
    let ctx = KeywordContext {
        auto_resolved: false,
        text: "na".to_string(),
        start: Position::new(3, 5),
        end: Position::new(3, 11),
    };
    let items = complete_keywords(&index, &ctx, "simple.utils", None);
    let item = items
        .iter()
        .find(|i| i.label == ":name")
        .unwrap_or_else(|| panic!("`:name` not offered: {:?}", items));
    assert_eq!(item.kind, Some(CompletionItemKind::KEYWORD));
    assert_eq!(item.detail.as_deref(), Some("keyword, 1 use"));
    assert_eq!(item.filter_text.as_deref(), Some(":name"));
    match item.text_edit.as_ref().expect("text_edit") {
        tower_lsp::lsp_types::CompletionTextEdit::Edit(edit) => {
            assert_eq!(edit.new_text, ":name");
            assert_eq!(edit.range.start, Position::new(3, 5));
            assert_eq!(edit.range.end, Position::new(3, 11));
        }
        other => panic!("expected a plain edit: {:?}", other),
    }
}

// --- auto-require -----------------------------------------------------------

/// The `ns` form of the file under test — auto-require items need one to build
/// their edit against.
const AUTO_REQUIRE_SOURCE: &str = "(ns app.core)\n\n(defn go [] nil)\n";

/// An index holding one project file (`app.core`) and `clojure.string` as a
/// library namespace, the shape the curated-alias path needs.
fn auto_require_index() -> Index {
    let index = Index::new();
    index.insert_file(ns_meta("app.core"), vec![], vec![]);
    index.insert_lib_file(
        ns_meta("clojure.string"),
        vec![defn_sym("join", "clojure.string")],
    );
    index
}

fn edits_of(item: &CompletionItem) -> Vec<String> {
    item.additional_text_edits
        .as_ref()
        .map(|edits| edits.iter().map(|e| e.new_text.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn test_auto_require_qualified_unknown_alias() {
    // `str/jo` in a file that never required clojure.string: the alias resolves
    // to nothing today, so offer the var and the require that would make it
    // resolve.
    let index = auto_require_index();
    let items = complete_symbols(&index, "str/jo", "app.core", Some(AUTO_REQUIRE_SOURCE));
    let item = item_named(&items, "str/join");
    assert_eq!(
        item.detail.as_deref(),
        Some("requires [clojure.string :as str]")
    );
    assert_eq!(
        edits_of(&item),
        vec!["\n  (:require [clojure.string :as str])".to_string()]
    );
}

#[test]
fn test_auto_require_bare_prefix_project_ns() {
    // A bare prefix reaches vars of project namespaces this file has not
    // required, aliased by their last segment.
    let index = auto_require_index();
    index.insert_file(
        ns_meta("app.util"),
        vec![defn_sym("slugify", "app.util")],
        vec![],
    );

    let items = complete_symbols(&index, "slug", "app.core", Some(AUTO_REQUIRE_SOURCE));
    let item = item_named(&items, "util/slugify");
    assert_eq!(item.detail.as_deref(), Some("requires [app.util :as util]"));
    assert!(
        edits_of(&item)[0].contains("[app.util :as util]"),
        "edit: {:?}",
        edits_of(&item)
    );
}

#[test]
fn test_no_auto_require_when_already_required() {
    // The namespace is already required under that alias, so the var is offered
    // by the ordinary alias path — never a second time with an edit.
    let index = auto_require_index();
    let mut meta = ns_meta("app.core");
    meta.aliases
        .insert("str".to_string(), "clojure.string".to_string());
    meta.requires.push("clojure.string".to_string());
    index.insert_file(meta, vec![], vec![]);

    let items = complete_symbols(&index, "str/jo", "app.core", Some(AUTO_REQUIRE_SOURCE));
    let item = item_named(&items, "str/join");
    assert!(
        item.additional_text_edits.is_none(),
        "an already-required namespace must not carry a require edit: {:?}",
        item
    );

    // The same through a bare prefix.
    let items = complete_symbols(&index, "joi", "app.core", Some(AUTO_REQUIRE_SOURCE));
    assert!(
        !labels(&items).contains(&"str/join".to_string()),
        "already required: {:?}",
        labels(&items)
    );
}

#[test]
fn test_no_auto_require_on_alias_collision() {
    // `str` is bound to another namespace here, so inserting
    // `[clojure.string :as str]` would conflict — offer nothing.
    let index = auto_require_index();
    index.insert_lib_file(ns_meta("other.lib"), vec![defn_sym("other", "other.lib")]);
    let mut meta = ns_meta("app.core");
    meta.aliases
        .insert("str".to_string(), "other.lib".to_string());
    meta.requires.push("other.lib".to_string());
    index.insert_file(meta, vec![], vec![]);

    let items = complete_symbols(&index, "joi", "app.core", Some(AUTO_REQUIRE_SOURCE));
    assert!(
        !labels(&items).contains(&"str/join".to_string()),
        "alias `str` is taken: {:?}",
        labels(&items)
    );
}

#[test]
fn test_auto_require_sorts_after_in_scope() {
    // An exact auto-require match still ranks below an in-scope prefix match:
    // picking a name already in scope beats editing the ns form.
    let index = auto_require_index();
    index.insert_file(
        ns_meta("app.util"),
        vec![defn_sym("join", "app.util")],
        vec![],
    );
    let mut meta = ns_meta("app.core");
    meta.aliases
        .insert("s".to_string(), "clojure.string".to_string());
    meta.requires.push("clojure.string".to_string());
    index.insert_file(meta, vec![defn_sym("joiner", "app.core")], vec![]);

    let items = complete_symbols(&index, "join", "app.core", Some(AUTO_REQUIRE_SOURCE));
    let in_scope = item_named(&items, "joiner").sort_text.unwrap();
    let auto = item_named(&items, "util/join").sort_text.unwrap();
    assert!(
        in_scope < auto,
        "in-scope {:?} must sort before auto-require {:?}",
        in_scope,
        auto
    );
}

#[test]
fn test_auto_require_pool_capped() {
    let index = auto_require_index();
    for i in 0..60 {
        let ns = format!("app.mod{}", i);
        index.insert_file(ns_meta(&ns), vec![defn_sym("widget", &ns)], vec![]);
    }
    let items = complete_symbols(&index, "widg", "app.core", Some(AUTO_REQUIRE_SOURCE));
    let auto: Vec<String> = items
        .iter()
        .filter(|i| i.sort_text.as_deref().is_some_and(|s| s.starts_with("9-")))
        .map(|i| i.label.clone())
        .collect();
    assert_eq!(
        auto.len(),
        30,
        "auto-require pool must be capped: {:?}",
        auto
    );
}

#[test]
fn test_auto_require_without_source_still_offers_the_item() {
    // No buffer to edit (the bare unit-test call): the item is still offered,
    // just without its require edit.
    let index = auto_require_index();
    let items = complete_symbols(&index, "str/jo", "app.core", None);
    let item = item_named(&items, "str/join");
    assert!(item.additional_text_edits.is_none());
}
