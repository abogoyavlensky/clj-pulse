use std::path::Path;

use tower_lsp::lsp_types::*;

use crate::index::extractor;

/// Computes `unresolved-namespace` diagnostics for `source`: a warning for each
/// qualified usage (`prefix/name`) whose prefix isn't resolvable from this file
/// and isn't Java/JS interop. Pure and index-free — availability is decided
/// from the file's own `ns` form, so a project without an indexed classpath
/// never produces false positives.
pub fn compute(source: &str, path: &Path) -> Vec<Diagnostic> {
    // EDN config files (deps.edn / lgx.edn) are not source: their dependency
    // coordinates (`my/loc`, `org.clojure/clojure`) look like qualified usages
    // but must never be flagged.
    if !crate::config::is_clojure_source(path) {
        return vec![];
    }

    let Ok((ns_meta, _)) = extractor::extract(source, path) else {
        return vec![];
    };

    let mut diags: Vec<Diagnostic> = extractor::qualified_usages(source)
        .into_iter()
        .filter(|u| {
            !ns_meta.resolves_prefix(&u.prefix)
                && u.prefix != "clojure.core"
                && !is_interop(&u.prefix)
        })
        .map(|u| Diagnostic {
            range: u.range,
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("unresolved-namespace".to_string())),
            source: Some("clj-pulse".to_string()),
            message: format!("Unresolved namespace: {}", u.prefix),
            ..Default::default()
        })
        .collect();

    // Required namespaces the file never uses — the squiggle counterpart to the
    // "Clean namespace" code action. Tagged UNNECESSARY so editors fade them
    // (clojure-lsp's treatment), and built from the same usage analysis that
    // action uses, so the squiggle and the fix never disagree.
    diags.extend(
        crate::handlers::code_action::unused_requires(source)
            .into_iter()
            .map(|u| Diagnostic {
                range: u.range,
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("unused-namespace".to_string())),
                source: Some("clj-pulse".to_string()),
                message: format!("Unused namespace: {}", u.namespace),
                tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            }),
    );

    // A namespace required more than once (even with a different `:as` alias).
    // Keyed on the namespace, so it catches duplicates the exact-text dedup in
    // "Clean namespace" misses. Deliberately *not* tagged UNNECESSARY: when each
    // require provides a distinct, used alias/refer the later one is redundant
    // but not dead, and fading it would wrongly imply it is safe to delete.
    diags.extend(
        crate::handlers::code_action::duplicate_requires(source)
            .into_iter()
            .map(|d| Diagnostic {
                range: d.range,
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("duplicate-require".to_string())),
                source: Some("clj-pulse".to_string()),
                message: format!("Duplicate require: {}", d.namespace),
                ..Default::default()
            }),
    );

    diags
}

/// Java classes (`Math`, `java.util.Date`, `clojure.lang.RT`) and the cljs
/// `js` global are not namespaces and never need a require. Clojure namespaces
/// are lowercase by convention, so an uppercase final segment marks a class.
fn is_interop(prefix: &str) -> bool {
    prefix == "js"
        || prefix
            .rsplit('.')
            .next()
            .and_then(|seg| seg.chars().next())
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diags(source: &str) -> Vec<Diagnostic> {
        compute(source, Path::new("test.clj"))
    }

    fn codes(source: &str) -> Vec<String> {
        diags(source)
            .into_iter()
            .filter_map(|d| match d.code {
                Some(NumberOrString::String(s)) => Some(s),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn no_flag_on_edn_dependency_coordinates() {
        // Dependency coordinates (`my/loc`, `org.clojure/clojure`) are
        // namespaced symbols structurally identical to qualified usages, but
        // EDN config files are not source and must never be linted.
        let lgx = r#"{:deps {my/loc {:local/root "v"}
                             ext/lib {:git/url "u" :git/sha "s"}}}"#;
        assert!(compute(lgx, Path::new("lgx.edn")).is_empty());

        let deps = r#"{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}}"#;
        assert!(compute(deps, Path::new("deps.edn")).is_empty());
    }

    #[test]
    fn flags_unrequired_qualified_usage() {
        let d = diags("(ns my.app)\n(str/join \", \" [1 2])\n");
        assert_eq!(d.len(), 1);
        let d = &d[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            d.code,
            Some(NumberOrString::String("unresolved-namespace".to_string()))
        );
        assert_eq!(d.source.as_deref(), Some("clj-pulse"));
        assert!(d.message.contains("str"), "message: {}", d.message);
        // Whole-symbol range: `str/join` is 8 chars on line 1.
        assert_eq!(d.range.start.line, 1);
        assert_eq!(d.range.end.character - d.range.start.character, 8);
    }

    #[test]
    fn no_flag_when_aliased() {
        assert!(
            diags("(ns my.app\n  (:require [clojure.string :as str]))\n(str/join \"\" [])\n")
                .is_empty()
        );
    }

    #[test]
    fn no_flag_when_plainly_required() {
        assert!(
            diags("(ns my.app\n  (:require [clojure.set]))\n(clojure.set/union #{} #{})\n")
                .is_empty()
        );
    }

    #[test]
    fn no_flag_for_current_namespace() {
        assert!(diags("(ns my.app)\n(my.app/foo 1)\n").is_empty());
    }

    #[test]
    fn no_flag_for_clojure_core() {
        assert!(diags("(ns my.app)\n(clojure.core/map inc [1])\n").is_empty());
    }

    #[test]
    fn no_flag_for_class_interop() {
        assert!(diags("(ns my.app)\n(Math/sqrt 4)\n").is_empty());
        assert!(diags("(ns my.app)\n(java.util.Date/from x)\n").is_empty());
        assert!(diags("(ns my.app)\n(clojure.lang.RT/iter x)\n").is_empty());
    }

    #[test]
    fn no_flag_for_js_global() {
        assert!(diags("(ns my.app)\n(js/parseInt \"1\")\n").is_empty());
    }

    #[test]
    fn flags_unknown_prefix_without_suggestion() {
        // No require, not interop, not in any index — still flagged.
        assert_eq!(
            codes("(ns my.app)\n(unknown/thing 1)\n"),
            vec!["unresolved-namespace"]
        );
    }

    #[test]
    fn no_flag_for_empty_name() {
        // `str/` mid-type must not warn.
        assert!(diags("(ns my.app)\n(str/ )\n").is_empty());
    }

    #[test]
    fn no_flag_for_reader_conditional_require() {
        // .cljc: alias required inside a reader conditional, used in another.
        let src = "(ns my.app\n  (:require\n   #?(:clj [clojure.string :as str]\n      :cljs [clojure.string :as str])))\n#?(:cljs (str/join \"\" []))\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn no_flag_for_splicing_reader_conditional_require() {
        let src = "(ns my.app\n  (:require\n   #?@(:clj [[clojure.string :as str]]\n       :cljs [[clojure.string :as str]])))\n(str/join \"\" [])\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn no_flag_for_namespaced_destructuring_keys() {
        // {:keys [foo/bar]} binds `bar` from key :foo/bar — not a usage.
        let src = "(ns my.app)\n(defn f [{:keys [foo/bar baz/qux]}] [bar qux])\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn flags_qualified_usage_in_map_value() {
        // A real qualified usage as a map value is still flagged.
        assert_eq!(
            codes("(ns my.app)\n{:x (str/join \"\" [])}\n"),
            vec!["unresolved-namespace"]
        );
    }

    #[test]
    fn prefix_list_require_does_not_suppress() {
        // Legacy `(clojure set)` prefix-list is unsupported; `set/union` must
        // still be flagged (the real namespace is clojure.set, not `set`).
        let src = "(ns my.app\n  (:require (clojure set)))\n(set/union #{} #{})\n";
        assert_eq!(codes(src), vec!["unresolved-namespace"]);
    }

    #[test]
    fn range_excludes_type_hint() {
        // `^String foo/bar` — squiggle covers `foo/bar` (7 chars), not the hint.
        let d = diags("(ns my.app)\n^String foo/bar\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].range.end.character - d[0].range.start.character, 7);
    }

    /// The `unused-namespace` diagnostics for `source`.
    fn unused(source: &str) -> Vec<Diagnostic> {
        diags(source)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("unused-namespace".to_string())))
            .collect()
    }

    #[test]
    fn flags_unused_alias_require() {
        let d = unused("(ns my.app\n  (:require [clojure.string :as str]))\n(def x 1)\n");
        assert_eq!(d.len(), 1);
        let d = &d[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            d.code,
            Some(NumberOrString::String("unused-namespace".to_string()))
        );
        assert_eq!(d.source.as_deref(), Some("clj-pulse"));
        assert!(
            d.message.contains("clojure.string"),
            "message: {}",
            d.message
        );
        // Tagged UNNECESSARY so editors fade the require.
        assert_eq!(d.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
        // Range spans the namespace symbol `clojure.string` (14 chars) on line 1.
        assert_eq!(d.range.start.line, 1);
        assert_eq!(d.range.end.character - d.range.start.character, 14);
    }

    #[test]
    fn no_flag_when_alias_used() {
        assert!(
            unused("(ns my.app\n  (:require [clojure.string :as str]))\n(str/join \"\" [])\n")
                .is_empty()
        );
    }

    #[test]
    fn no_flag_when_used_fully_qualified() {
        // Alias `s` is unused, but `clojure.set/union` uses the namespace — kept.
        assert!(unused(
            "(ns my.app\n  (:require [clojure.set :as s]))\n(clojure.set/union #{} #{})\n"
        )
        .is_empty());
    }

    #[test]
    fn no_flag_for_plain_side_effecting_require() {
        // Plain `[some.ns]` / bare `some.ns` may load side effects — never flagged,
        // even when nothing references them.
        assert!(unused("(ns my.app\n  (:require [some.ns]))\n(def x 1)\n").is_empty());
        assert!(unused("(ns my.app\n  (:require some.side))\n(def x 1)\n").is_empty());
    }

    #[test]
    fn flags_unused_refer_only() {
        let d = unused("(ns my.app\n  (:require [clojure.set :refer [union]]))\n(def x 1)\n");
        assert_eq!(d.len(), 1);
        assert!(
            d[0].message.contains("clojure.set"),
            "message: {}",
            d[0].message
        );
    }

    #[test]
    fn no_flag_when_refer_used() {
        assert!(unused(
            "(ns my.app\n  (:require [clojure.set :refer [union]]))\n(union #{} #{})\n"
        )
        .is_empty());
    }

    #[test]
    fn no_flag_when_alias_used_only_in_keyword() {
        // `s` appears only in the auto-resolved keyword `::s/problem`.
        assert!(unused(
            "(ns my.app\n  (:require [clojure.spec.alpha :as s]))\n(defn f [x] (::s/problem x))\n"
        )
        .is_empty());
    }

    #[test]
    fn no_unused_flag_for_reader_conditional_require() {
        // Reader-conditional specs are handled conservatively (kept), so an
        // alias unused across visible branches is still not flagged.
        let src = "(ns my.app\n  (:require\n   #?(:clj [clojure.string :as str])))\n(def x 1)\n";
        assert!(unused(src).is_empty(), "{:?}", unused(src));
    }

    #[test]
    fn no_flag_for_unmodeled_option() {
        // `:rename` introduces a usable name we don't track — keep the spec.
        let src =
            "(ns my.app\n  (:require [clojure.string :refer [join] :rename {join j}]))\n(j)\n";
        assert!(unused(src).is_empty(), "{:?}", unused(src));
    }

    #[test]
    fn no_flag_without_require_clause() {
        assert!(unused("(ns my.app)\n(def x 1)\n").is_empty());
    }

    #[test]
    fn no_flag_for_self_require() {
        // A require of the file's own namespace is never flagged, even when its
        // alias/refer is unused.
        assert!(unused("(ns my.app\n  (:require [my.app :as app]))\n(def x 1)\n").is_empty());
        assert!(
            unused("(ns my.app\n  (:require [my.app :refer [helper]]))\n(def x 1)\n").is_empty()
        );
        // Metadata-wrapped ns name must still be recognised as the self-namespace.
        assert!(
            unused("(ns ^{:doc \"d\"} my.app\n  (:require [my.app :as app]))\n(def x 1)\n")
                .is_empty()
        );
    }

    #[test]
    fn flags_only_the_unused_among_several() {
        // One unused alias, one used alias: exactly the unused one is flagged.
        let src = "(ns my.app\n  (:require [a.b :as b]\n            [c.d :as d]))\n(d/run)\n";
        let d = unused(src);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("a.b"), "message: {}", d[0].message);
    }

    /// The `duplicate-require` diagnostics for `source`.
    fn dups(source: &str) -> Vec<Diagnostic> {
        diags(source)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("duplicate-require".to_string())))
            .collect()
    }

    #[test]
    fn flags_duplicate_require_with_different_alias() {
        // Same namespace, different `:as` alias — the second is a duplicate.
        let src = "(ns my.app\n  (:require [clojure.string :as str]\n            \
                   [clojure.string :as s]))\n(str/join \"\" [])\n(s/trim \"\")\n";
        let d = dups(src);
        assert_eq!(d.len(), 1);
        let d = &d[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            d.code,
            Some(NumberOrString::String("duplicate-require".to_string()))
        );
        assert_eq!(d.source.as_deref(), Some("clj-pulse"));
        assert!(
            d.message.contains("clojure.string"),
            "message: {}",
            d.message
        );
        // Not tagged UNNECESSARY: both aliases are used, so the require is
        // redundant but not dead — fading it would mislead.
        assert_eq!(d.tags, None);
        // The duplicate is the second occurrence, on line 2.
        assert_eq!(d.range.start.line, 2);
    }

    #[test]
    fn no_duplicate_for_distinct_namespaces() {
        let src = "(ns my.app\n  (:require [a.b :as b]\n            [c.d :as d]))\n(b/x)\n(d/y)\n";
        assert!(dups(src).is_empty());
    }

    #[test]
    fn flags_duplicate_across_require_clauses() {
        let src = "(ns my.app\n  (:require [c.d :as d])\n  (:require [c.d :as e]))\n(d/x)\n(e/y)\n";
        assert_eq!(dups(src).len(), 1);
    }

    #[test]
    fn flags_third_occurrence_too() {
        // Three requires of the same ns flag the 2nd and 3rd.
        let src = "(ns my.app\n  (:require [c.d :as d]\n            [c.d :as e]\n            \
                   [c.d :as f]))\n(d/x)\n(e/y)\n(f/z)\n";
        assert_eq!(dups(src).len(), 2);
    }

    #[test]
    fn flags_duplicate_bare_and_vector_require() {
        // A bare `c.d` and a `[c.d :as d]` are the same namespace twice.
        let src = "(ns my.app\n  (:require c.d\n            [c.d :as d]))\n(d/x)\n";
        assert_eq!(dups(src).len(), 1);
    }

    #[test]
    fn no_duplicate_across_reader_conditional_branches() {
        // Platform branches are mutually exclusive — not a duplicate.
        let src = "(ns my.app\n  (:require\n   #?(:clj [c.d :as d]\n      :cljs [c.d :as e])))\n#?(:clj (d/x) :cljs (e/y))\n";
        assert!(dups(src).is_empty(), "{:?}", dups(src));
    }

    #[test]
    fn no_duplicate_for_single_require() {
        assert!(dups("(ns my.app\n  (:require [c.d :as d]))\n(d/x)\n").is_empty());
    }
}
