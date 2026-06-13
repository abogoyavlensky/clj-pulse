use std::path::Path;

use tower_lsp::lsp_types::*;

use crate::index::extractor;

/// Computes `unresolved-namespace` diagnostics for `source`: a warning for each
/// qualified usage (`prefix/name`) whose prefix isn't resolvable from this file
/// and isn't Java/JS interop. Pure and index-free — availability is decided
/// from the file's own `ns` form, so a project without an indexed classpath
/// never produces false positives.
pub fn compute(source: &str, path: &Path) -> Vec<Diagnostic> {
    let Ok((ns_meta, _)) = extractor::extract(source, path) else {
        return vec![];
    };

    extractor::qualified_usages(source)
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
            source: Some("clj-lsp".to_string()),
            message: format!("Unresolved namespace: {}", u.prefix),
            ..Default::default()
        })
        .collect()
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
    fn flags_unrequired_qualified_usage() {
        let d = diags("(ns my.app)\n(str/join \", \" [1 2])\n");
        assert_eq!(d.len(), 1);
        let d = &d[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            d.code,
            Some(NumberOrString::String("unresolved-namespace".to_string()))
        );
        assert_eq!(d.source.as_deref(), Some("clj-lsp"));
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
}
