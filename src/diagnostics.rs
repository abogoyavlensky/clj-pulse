use std::path::Path;

use tower_lsp::lsp_types::*;

use crate::index::{extractor, DefKind, ExtractConfig};

/// Computes this file's native diagnostics. Pure and index-free — every answer
/// comes from the file's own text, so a project without an indexed classpath
/// never produces false positives. `cfg` is only consulted for `:lint-as`, so a
/// custom defining macro binds its params like the form it stands in for.
pub fn compute(source: &str, path: &Path, cfg: &ExtractConfig) -> Vec<Diagnostic> {
    // EDN config files (deps.edn / lgx.edn) are not source: their dependency
    // coordinates (`my/loc`, `org.clojure/clojure`) look like qualified usages
    // but must never be flagged.
    if !crate::config::is_clojure_source(path) {
        return vec![];
    }

    let Ok(analysis) = extractor::extract_analysis_with(source, path, cfg) else {
        return vec![];
    };
    let ns_meta = &analysis.ns_meta;

    // A warning for each qualified usage (`prefix/name`) whose prefix isn't
    // resolvable from this file and isn't Java/JS interop.
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

    // Locals the file binds but never reads. Tagged UNNECESSARY so editors fade
    // them; a leading `_` is the opt-out, applied by the extractor.
    diags.extend(analysis.unused_bindings.iter().map(|b| Diagnostic {
        range: b.name_range,
        severity: Some(DiagnosticSeverity::WARNING),
        code: Some(NumberOrString::String("unused-binding".to_string())),
        source: Some("clj-pulse".to_string()),
        message: format!("Unused binding: {}", b.name),
        tags: Some(vec![DiagnosticTag::UNNECESSARY]),
        ..Default::default()
    }));

    // Private vars nothing in this file uses. Private means the var can only be
    // reached from here, so the file's own occurrences settle it — but a usage
    // inside the var's own form (recursion) does not count.
    diags.extend(
        analysis
            .symbols
            .iter()
            .filter(|sym| sym.private && is_lintable_private_kind(&sym.kind))
            .filter(|sym| {
                !analysis
                    .occurrences
                    .iter()
                    .any(|occ| occ.fqn == sym.fqn && !range_within(&occ.name_range, &sym.range))
            })
            .map(|sym| Diagnostic {
                range: sym.name_range,
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("unused-private-var".to_string())),
                source: Some("clj-pulse".to_string()),
                message: format!("Unused private var: {}", sym.name),
                tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            }),
    );

    diags
}

/// The `def`-family kinds whose private members are worth reporting.
/// `deftest-` is deliberately absent: a test runner calls a private test by
/// var, so it is never dead. Type/record/protocol forms are excluded too —
/// their vars are reached through generated constructors and method dispatch.
fn is_lintable_private_kind(kind: &DefKind) -> bool {
    matches!(
        kind,
        DefKind::Def
            | DefKind::Defonce
            | DefKind::Defn
            | DefKind::DefnPrivate
            | DefKind::Defmacro
            | DefKind::Defmulti
    )
}

/// Whether `inner` sits inside `outer` — used to tell a recursive self-call
/// (inside the def's own form) from a real use elsewhere in the file.
fn range_within(inner: &Range, outer: &Range) -> bool {
    let starts_after =
        (inner.start.line, inner.start.character) >= (outer.start.line, outer.start.character);
    let ends_before =
        (inner.end.line, inner.end.character) <= (outer.end.line, outer.end.character);
    starts_after && ends_before
}

/// The native codes clj-kondo also emits. When a clj-kondo run succeeds it
/// owns these — publishing both sets would double every squiggle, with two
/// slightly different messages. `unused-binding` and `unused-private-var` are
/// clj-kondo's `:unused-binding` and `:unused-private-var` linters; the native
/// versions exist for the (common) case of no clj-kondo on the machine.
const KONDO_OWNED_CODES: [&str; 5] = [
    "unresolved-namespace",
    "unused-namespace",
    "duplicate-require",
    "unused-binding",
    "unused-private-var",
];

/// Combines this pass's native diagnostics with clj-kondo's.
///
/// A successful clj-kondo run (`Ok`, including a clean `Ok(vec![])`) takes
/// ownership of every code it can emit, so the native diagnostics carrying
/// those codes are dropped for this pass. The rule is stated per-code rather
/// than "drop everything native" so a future native lint clj-kondo has no
/// equivalent for keeps showing up beside its findings.
///
/// Any failure — kondo absent, disabled, timed out, crashed — is `Err`, and
/// the native set is published unchanged. Diagnostics never silently vanish
/// because a subprocess had a bad day.
pub fn merge(native: Vec<Diagnostic>, kondo: Result<Vec<Diagnostic>, String>) -> Vec<Diagnostic> {
    let Ok(kondo) = kondo else {
        return native;
    };
    let mut merged = kondo;
    merged.extend(native.into_iter().filter(|d| !is_kondo_owned(d)));
    merged
}

/// Whether a native diagnostic carries a code clj-kondo also reports.
fn is_kondo_owned(diagnostic: &Diagnostic) -> bool {
    matches!(
        &diagnostic.code,
        Some(NumberOrString::String(code)) if KONDO_OWNED_CODES.contains(&code.as_str())
    )
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
    use crate::index::ExtractConfig;

    fn diags(source: &str) -> Vec<Diagnostic> {
        compute(source, Path::new("test.clj"), &ExtractConfig::default())
    }

    /// Every diagnostic carrying `code`.
    fn of_code(source: &str, code: &str) -> Vec<Diagnostic> {
        diags(source)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String(code.to_string())))
            .collect()
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
        assert!(compute(lgx, Path::new("lgx.edn"), &ExtractConfig::default()).is_empty());

        let deps = r#"{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}}"#;
        assert!(compute(deps, Path::new("deps.edn"), &ExtractConfig::default()).is_empty());
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

    // --- native / clj-kondo ownership merge -------------------------------

    fn diag(code: &str, source: &str) -> Diagnostic {
        Diagnostic {
            code: Some(NumberOrString::String(code.to_string())),
            source: Some(source.to_string()),
            message: format!("{source}: {code}"),
            ..Default::default()
        }
    }

    fn merged_codes(
        native: Vec<Diagnostic>,
        kondo: Result<Vec<Diagnostic>, String>,
    ) -> Vec<String> {
        merge(native, kondo)
            .into_iter()
            .map(|d| match d.code {
                Some(NumberOrString::String(c)) => c,
                other => panic!("unexpected code {other:?}"),
            })
            .collect()
    }

    #[test]
    fn flags_unused_let_binding() {
        let d = of_code("(ns a)\n(defn f [x]\n  (let [y 1] x))\n", "unused-binding");
        assert_eq!(d.len(), 1, "diagnostics: {:?}", d);
        let d = &d[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(d.source.as_deref(), Some("clj-pulse"));
        assert_eq!(d.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
        assert!(d.message.contains('y'), "message: {}", d.message);
        assert_eq!(d.range.start.line, 2);
        assert_eq!(d.range.end.character - d.range.start.character, 1);
    }

    #[test]
    fn no_flag_for_used_binding() {
        assert!(of_code("(ns a)\n(defn f []\n  (let [y 1] y))\n", "unused-binding").is_empty());
    }

    #[test]
    fn no_flag_for_underscore_binding() {
        assert!(of_code("(ns a)\n(defn f [_x] 1)\n", "unused-binding").is_empty());
    }

    #[test]
    fn lint_as_defn_params_are_linted() {
        // A `:lint-as` macro mapped to `defn` binds its params like `defn`, so
        // an unused one is reported the same way.
        let cfg = ExtractConfig {
            lint_as: std::collections::HashMap::from([(
                "my/defthing".to_string(),
                crate::index::DefKind::Defn,
            )]),
        };
        let src = "(ns x (:require [my :refer [defthing]]))\n(defthing foo [p] 1)\n";
        let found: Vec<_> = compute(src, Path::new("x.clj"), &cfg)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("unused-binding".to_string())))
            .collect();
        assert_eq!(found.len(), 1, "diagnostics: {:?}", found);
        assert!(found[0].message.contains('p'), "{}", found[0].message);
    }

    #[test]
    fn flags_unused_defn_private() {
        let d = of_code("(ns a)\n(defn- helper [] 1)\n", "unused-private-var");
        assert_eq!(d.len(), 1, "diagnostics: {:?}", d);
        let d = &d[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(d.source.as_deref(), Some("clj-pulse"));
        assert_eq!(d.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
        assert!(d.message.contains("helper"), "message: {}", d.message);
        // The name, not the whole form: `helper` on line 1.
        assert_eq!(d.range.start.line, 1);
        assert_eq!(d.range.start.character, "(defn- ".len() as u32);
        assert_eq!(d.range.end.character - d.range.start.character, 6);
    }

    #[test]
    fn flags_unused_private_meta_def() {
        let d = of_code("(ns a)\n(def ^:private x 1)\n", "unused-private-var");
        assert_eq!(d.len(), 1, "diagnostics: {:?}", d);
        assert!(d[0].message.contains('x'), "message: {}", d[0].message);
    }

    #[test]
    fn no_flag_when_private_var_is_called() {
        assert!(of_code(
            "(ns a)\n(defn- h [] 1)\n(defn g [] (h))\n",
            "unused-private-var"
        )
        .is_empty());
    }

    #[test]
    fn no_flag_when_private_var_is_var_quoted() {
        assert!(of_code(
            "(ns a)\n(defn- h [] 1)\n(defn g [] #'h)\n",
            "unused-private-var"
        )
        .is_empty());
    }

    #[test]
    fn recursion_only_is_still_unused() {
        // A self-call inside the var's own form is not a use of it.
        let d = of_code("(ns a)\n(defn- h [n] (h n))\n", "unused-private-var");
        assert_eq!(d.len(), 1, "diagnostics: {:?}", d);
    }

    #[test]
    fn no_flag_for_public_var() {
        assert!(of_code("(ns a)\n(defn h [] 1)\n", "unused-private-var").is_empty());
    }

    #[test]
    fn no_flag_for_private_deftest() {
        // Test runners call private tests by var, so `deftest-` is never dead.
        let src = "(ns a (:require [clojure.test :refer [deftest-]]))\n(deftest- t 1)\n";
        assert!(of_code(src, "unused-private-var").is_empty());
    }

    #[test]
    fn successful_kondo_run_cedes_the_codes_it_owns() {
        // Every native code today is one clj-kondo also emits, so a successful
        // run publishes clj-kondo's findings alone — no doubled squiggles.
        let native = vec![
            diag("unresolved-namespace", "clj-pulse"),
            diag("unused-namespace", "clj-pulse"),
            diag("duplicate-require", "clj-pulse"),
            diag("unused-binding", "clj-pulse"),
            diag("unused-private-var", "clj-pulse"),
        ];
        let kondo = vec![diag("unresolved-namespace", "clj-kondo")];
        assert_eq!(
            merged_codes(native, Ok(kondo)),
            vec!["unresolved-namespace".to_string()]
        );
    }

    #[test]
    fn native_codes_kondo_does_not_cover_survive_a_successful_run() {
        // The rule is per-code, so a future native-only lint keeps showing up.
        let native = vec![
            diag("unresolved-namespace", "clj-pulse"),
            diag("some-future-native-lint", "clj-pulse"),
        ];
        assert_eq!(
            merged_codes(native, Ok(vec![diag("invalid-arity", "clj-kondo")])),
            vec![
                "invalid-arity".to_string(),
                "some-future-native-lint".to_string()
            ]
        );
    }

    #[test]
    fn a_clean_kondo_run_still_cedes_ownership() {
        // `Ok(vec![])` means clj-kondo looked and found nothing — the native
        // squiggle it disagrees with must go, or the user can never clear it.
        assert!(
            merged_codes(vec![diag("unresolved-namespace", "clj-pulse")], Ok(vec![])).is_empty()
        );
    }

    #[test]
    fn a_failed_kondo_run_leaves_the_native_set_alone() {
        // Absent, disabled, timed out, crashed: all the same here. Losing
        // diagnostics because a subprocess failed would be worse than nothing.
        let native = vec![
            diag("unresolved-namespace", "clj-pulse"),
            diag("unused-namespace", "clj-pulse"),
        ];
        assert_eq!(
            merged_codes(native, Err("clj-kondo timed out".to_string())),
            vec![
                "unresolved-namespace".to_string(),
                "unused-namespace".to_string()
            ]
        );
    }

    #[test]
    fn a_native_diagnostic_without_a_code_is_never_dropped() {
        let mut bare = diag("x", "clj-pulse");
        bare.code = None;
        assert_eq!(merge(vec![bare], Ok(vec![])).len(), 1);
    }
}
