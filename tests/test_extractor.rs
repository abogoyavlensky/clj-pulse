use clj_pulse::index::extractor::extract;
use clj_pulse::index::DefKind;
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
fn test_extracts_required_namespaces() {
    let (meta, _) = extract(
        include_str!("fixtures/snippets/ns_with_requires.clj"),
        Path::new("ns_with_requires.clj"),
    )
    .unwrap();
    // Every required namespace is recorded, regardless of :as / :refer.
    assert!(meta.requires.contains(&"clojure.string".to_string()));
    assert!(meta.requires.contains(&"my.core".to_string()));
    assert!(meta.requires.contains(&"my.utils".to_string()));
}

#[test]
fn test_records_bare_symbol_require() {
    // `(:require clojure.set)` is a legal non-vector libspec; it must still
    // land in `requires` so a fully-qualified usage isn't flagged as missing.
    let (meta, _) = extract(
        "(ns my.app\n  (:require clojure.set\n            [clojure.string :as str]))\n",
        Path::new("app.clj"),
    )
    .unwrap();
    assert!(meta.requires.contains(&"clojure.set".to_string()));
    assert!(meta.requires.contains(&"clojure.string".to_string()));
}

#[test]
fn test_qualified_usages_collects_and_skips_quotes() {
    use clj_pulse::index::extractor::qualified_usages;
    let src = "(ns my.app\n  (:require [clojure.string :as str]))\n\n\
               (defn f []\n  \
                 (str/join \", \" [1 2])\n  \
                 (clojure.set/union #{1} #{2})\n  \
                 'foo/bar\n  \
                 #?(:clj (edn/read-string \"1\")))\n";
    let usages = qualified_usages(src);
    let pairs: Vec<(&str, &str)> = usages
        .iter()
        .map(|u| (u.prefix.as_str(), u.name.as_str()))
        .collect();

    assert!(pairs.contains(&("str", "join")));
    assert!(pairs.contains(&("clojure.set", "union")));
    // Descends into reader conditionals.
    assert!(pairs.contains(&("edn", "read-string")));
    // 'foo/bar is quoted data, not a usage.
    assert!(
        !pairs.iter().any(|(p, _)| *p == "foo"),
        "quoted symbol should be excluded: {:?}",
        pairs
    );

    // Range spans the whole `str/join` symbol (8 chars).
    let join = usages.iter().find(|u| u.name == "join").unwrap();
    assert_eq!(join.range.end.character - join.range.start.character, 8);
}

#[test]
fn test_qualified_usages_skips_reader_discard() {
    use clj_pulse::index::extractor::qualified_usages;
    let src = "(ns my.app)\n(defn f []\n  #_unused/sym\n  #_(discarded/call 1)\n  (real/use))\n";
    let usages = qualified_usages(src);
    let prefixes: Vec<&str> = usages.iter().map(|u| u.prefix.as_str()).collect();
    assert_eq!(prefixes, vec!["real"], "discarded forms must be excluded");
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
fn test_extracts_protocol_methods_as_symbols() {
    let src =
        "(ns my.ns)\n(defprotocol Storage\n  (fetch [this id])\n  (store [this x] [this x y]))";
    let (_, syms) = extract(src, Path::new("a.clj")).unwrap();

    // The protocol itself is still extracted.
    assert!(syms
        .iter()
        .any(|s| s.name == "Storage" && s.kind == DefKind::Defprotocol));

    let fetch = syms
        .iter()
        .find(|s| s.name == "fetch")
        .expect("fetch method not extracted");
    assert_eq!(fetch.kind, DefKind::Defn);
    assert_eq!(fetch.fqn, "my.ns/fetch");
    // name_range points at the method name, not the whole signature.
    assert_eq!(fetch.name_range.start.line, 2);
    assert_eq!(fetch.name_range.start.character, 3);
    assert_eq!(fetch.name_range.end.character, 3 + "fetch".len() as u32);

    let store = syms
        .iter()
        .find(|s| s.name == "store")
        .expect("store method not extracted");
    assert_eq!(store.fqn, "my.ns/store");
    assert_eq!(store.params, vec!["[this x]", "[this x y]"]);
}

#[test]
fn test_protocol_doc_and_options_skipped() {
    let src =
        "(ns my.ns)\n(defprotocol P\n  \"proto doc\"\n  :extend-via-metadata true\n  (foo [this] \"foo doc\"))";
    let (_, syms) = extract(src, Path::new("a.clj")).unwrap();

    let foo = syms
        .iter()
        .find(|s| s.name == "foo")
        .expect("foo method not extracted");
    assert_eq!(foo.kind, DefKind::Defn);
    assert_eq!(foo.doc.as_deref(), Some("foo doc"));

    // Options (kwd + value) and the protocol doc string must not become symbols.
    assert_eq!(
        syms.iter().filter(|s| s.kind == DefKind::Defn).count(),
        1,
        "expected exactly one method symbol, got {:?}",
        syms.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
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

// --- occurrence extraction (Phase 2) ---

use clj_pulse::index::extractor::extract_full;

fn occurrences_of<'a>(
    occs: &'a [clj_pulse::index::Occurrence],
    fqn: &str,
) -> Vec<&'a clj_pulse::index::Occurrence> {
    occs.iter().filter(|o| o.fqn == fqn).collect()
}

#[test]
fn test_occurrence_qualified_alias_name_only_range() {
    let src = "(ns my.app\n  (:require [other.lib :as lib]))\n\n(defn f [x]\n  (lib/process x))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();

    let found = occurrences_of(&occs, "other.lib/process");
    assert_eq!(found.len(), 1, "occurrences: {:?}", occs);
    let line = "  (lib/process x))";
    let col = line.find("process").unwrap() as u32;
    assert_eq!(found[0].name_range.start.line, 4);
    assert_eq!(found[0].name_range.start.character, col);
    assert_eq!(
        found[0].name_range.end.character,
        col + "process".len() as u32
    );
}

#[test]
fn test_occurrence_bare_symbol_resolves_to_current_ns() {
    let src = "(ns my.app)\n(defn helper [x] x)\n(defn caller [y] (helper y))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();

    // Only the usage in `caller` — the defn name itself is not an occurrence
    let found = occurrences_of(&occs, "my.app/helper");
    assert_eq!(found.len(), 1, "occurrences: {:?}", occs);
    assert_eq!(found[0].name_range.start.line, 2);
}

#[test]
fn test_occurrence_protocol_method_decl_not_recorded() {
    // The method declaration inside defprotocol must not count as a usage,
    // else references/rename double-count it. Only the real call does.
    let src = "(ns my.ns)\n(defprotocol Storage (fetch [this id]))\n(defn use-it [s] (fetch s 1))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();

    let found = occurrences_of(&occs, "my.ns/fetch");
    assert_eq!(found.len(), 1, "occurrences: {:?}", occs);
    assert_eq!(found[0].name_range.start.line, 2);
}

#[test]
fn test_occurrence_defrecord_impl_resolves_to_protocol_ns() {
    let src = "(ns app.impl\n  (:require [proto.ns :as p]))\n(defrecord R [x]\n  p/Worker\n  (run-task [this job] x))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();

    // The impl head resolves to the protocol's namespace, not the current one.
    assert_eq!(
        occurrences_of(&occs, "proto.ns/run-task").len(),
        1,
        "occs: {:?}",
        occs
    );
    // No phantom current-ns occurrences for the head, params, or fields.
    assert!(occurrences_of(&occs, "app.impl/run-task").is_empty());
    assert!(occurrences_of(&occs, "app.impl/this").is_empty());
    assert!(occurrences_of(&occs, "app.impl/job").is_empty());
    assert!(occurrences_of(&occs, "app.impl/x").is_empty());
}

#[test]
fn test_occurrence_object_method_head_skipped() {
    // Object/interface methods have no protocol declaration to target, so the
    // head must not become a phantom occurrence.
    let src = "(ns app)\n(defrecord R [x]\n  Object\n  (toString [this] \"s\"))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert!(
        occs.iter().all(|o| !o.fqn.ends_with("/toString")),
        "Object method head leaked: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_extend_protocol_methods_resolve_to_fixed_protocol() {
    let src = "(ns app\n  (:require [proto.ns :as p]))\n(extend-protocol p/Worker\n  String\n  (run-task [this job] job))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(
        occurrences_of(&occs, "proto.ns/run-task").len(),
        1,
        "occs: {:?}",
        occs
    );
    assert!(occurrences_of(&occs, "app/job").is_empty());
}

#[test]
fn test_occurrence_extend_type_methods_resolve_to_protocol() {
    let src = "(ns app\n  (:require [proto.ns :as p]))\n(extend-type String\n  p/Worker\n  (run-task [this job] job))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(
        occurrences_of(&occs, "proto.ns/run-task").len(),
        1,
        "occs: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_multi_arity_method_impl_binds_params() {
    let src = "(ns app\n  (:require [proto.ns :as p]))\n(extend-type String\n  p/Worker\n  (run-task ([x] x) ([x y] y)))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();

    // Each arity's params are bound, not recorded as phantom global usages.
    assert!(
        occurrences_of(&occs, "app/x").is_empty(),
        "occs: {:?}",
        occs
    );
    assert!(
        occurrences_of(&occs, "app/y").is_empty(),
        "occs: {:?}",
        occs
    );
    // The head still resolves to the protocol, recorded once.
    assert_eq!(
        occurrences_of(&occs, "proto.ns/run-task").len(),
        1,
        "occs: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_reify_methods_resolve_to_protocol() {
    let src = "(ns app\n  (:require [proto.ns :as p]))\n(defn make []\n  (reify p/Worker\n    (run-task [this job] job)))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(
        occurrences_of(&occs, "proto.ns/run-task").len(),
        1,
        "occs: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_refer_usage_and_vector_entry() {
    let src = "(ns my.app\n  (:require [other.lib :refer [process]]))\n\n(process 1)";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();

    // Both the :refer vector entry and the body usage
    let found = occurrences_of(&occs, "other.lib/process");
    assert_eq!(found.len(), 2, "occurrences: {:?}", occs);
    let lines: Vec<u32> = found.iter().map(|o| o.name_range.start.line).collect();
    assert!(
        lines.contains(&1) && lines.contains(&3),
        "lines: {:?}",
        lines
    );
}

#[test]
fn test_occurrence_locals_shadow_defs() {
    let src = "(ns my.app)\n(defn helper [x] x)\n(defn f [helper] (helper 1))\n(defn g [] (let [helper 2] (helper 3)))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert!(
        occurrences_of(&occs, "my.app/helper").is_empty(),
        "locally bound names must not be occurrences: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_destructured_binding_shadows() {
    let src = "(ns my.app)\n(defn helper [x] x)\n(defn f [{:keys [helper]}] (helper 1))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert!(
        occurrences_of(&occs, "my.app/helper").is_empty(),
        "destructured bindings must shadow: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_core_symbols_resolve_to_clojure_core() {
    let src = "(ns my.app)\n(defn f [x] (map inc x))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(occurrences_of(&occs, "clojure.core/map").len(), 1);
    assert_eq!(occurrences_of(&occs, "clojure.core/inc").len(), 1);
}

#[test]
fn test_occurrence_let_rhs_is_usage_binding_is_not() {
    let src = "(ns my.app)\n(def base 1)\n(defn f [] (let [x base] x))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(occurrences_of(&occs, "my.app/base").len(), 1);
    assert!(occurrences_of(&occs, "my.app/x").is_empty());
}

#[test]
fn test_occurrence_binding_and_with_redefs_are_usages() {
    // binding/with-redefs rebind Vars — both LHS and body are usages
    let src = "(ns my.app)\n(def ^:dynamic *x* 1)\n(defn f [] (binding [*x* 2] *x*))\n(defn g [] (with-redefs [*x* 3] *x*))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(
        occurrences_of(&occs, "my.app/*x*").len(),
        4,
        "binding LHS + body usages: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_for_let_clause_binds() {
    let src = "(ns my.app)\n(def base 1)\n(defn f [xs] (for [x xs :let [base 2]] base))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert!(
        occurrences_of(&occs, "my.app/base").is_empty(),
        ":let-bound names must shadow: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_def_vector_initializer_is_usage() {
    // A vector after a plain def is an initializer, not a param list
    let src = "(ns my.app)\n(def helper 1)\n(def xs [helper])\n(defonce ys [helper])";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(
        occurrences_of(&occs, "my.app/helper").len(),
        2,
        "vector initializers must be walked: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_letfn_names_are_locals() {
    let src = "(ns my.app)\n(defn helper [x] x)\n(defn g [] (letfn [(helper [y] y)] (helper 1)))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert!(
        occurrences_of(&occs, "my.app/helper").is_empty(),
        "letfn-bound fns must shadow: {:?}",
        occs
    );
}

#[test]
fn test_occurrence_defmethod_dispatch_vector_is_usage() {
    let src = "(ns my.app)\n(def t 1)\n(defmulti m (fn [a b] [a b]))\n(defmethod m [:a :b] [x y] (+ x t))";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(occurrences_of(&occs, "my.app/t").len(), 1);
    // params x/y are bound, not occurrences
    assert!(occurrences_of(&occs, "my.app/x").is_empty());
    assert!(occurrences_of(&occs, "my.app/y").is_empty());
}

#[test]
fn test_occurrence_defmethod_name_references_multimethod() {
    let src = "(ns my.app)\n(defmulti m :type)\n(defmethod m :a [x] x)";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    let found = occurrences_of(&occs, "my.app/m");
    assert_eq!(
        found.len(),
        1,
        "defmethod name must reference the multimethod: {:?}",
        occs
    );
    assert_eq!(found[0].name_range.start.line, 2);
}

#[test]
fn test_occurrence_destructuring_or_defaults_are_usages() {
    let src = "(ns my.app)\n(def dflt 1)\n(defn f [{:keys [x] :or {x dflt}}] x)";
    let (_, _, occs) = extract_full(src, Path::new("a.clj")).unwrap();
    assert_eq!(
        occurrences_of(&occs, "my.app/dflt").len(),
        1,
        ":or defaults are expressions: {:?}",
        occs
    );
    assert!(occurrences_of(&occs, "my.app/x").is_empty());
}
