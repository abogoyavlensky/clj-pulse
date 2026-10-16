//! clj-pulse project settings.
//!
//! Loads `.clj-pulse/config.edn` (clj-pulse's own settings, primary) and merges
//! it over the compatible subset of `.clj-kondo/config.edn` (read via `kondo`),
//! producing the resolved [`ExtractConfig`] the extractor consumes. For now the
//! only setting is `:lint-as`.
//!
//! Merge rule: clj-pulse wins per key. The raw `(macro, target)` pairs are
//! merged first (so a clj-pulse entry fully overrides a clj-kondo one, even when
//! it remaps a macro to another kind of target), then each target is mapped: a
//! `def`-family form (or clj-kondo's `def-catch-all`) to a [`DefKind`], a core
//! binding form (`clojure.core/for`, `let`, `fn`, …) to `lint_as_forms`.
//! Anything else (`clojure.core/->`) binds and defines nothing and is dropped.
//!
//! `.clj-pulse/config.edn` mirrors clj-kondo's `{:lint-as {sym sym}}` shape, so
//! both files are parsed by [`crate::kondo::parse_lint_as`].

use std::collections::HashMap;
use std::path::Path;

use crate::index::{DefKind, ExtractConfig};
use crate::kondo;

/// The core forms that bind locals, as the walkers name them: what a
/// `:lint-as` target in `clojure.core` / `cljs.core` may map a macro to.
const BINDING_FORMS: &[&str] = &[
    "let",
    "loop",
    "for",
    "doseq",
    "when-let",
    "if-let",
    "when-some",
    "if-some",
    "with-open",
    "dotimes",
    "fn",
    "letfn",
    "as->",
    "reify",
    "extend-type",
    "extend-protocol",
];

/// Merges clj-kondo `:lint-as` pairs (base) with clj-pulse pairs (overlay,
/// wins per key) into an [`ExtractConfig`]. Targets that neither define nor
/// bind are dropped.
fn merge(kondo_pairs: Vec<(String, String)>, pulse_pairs: Vec<(String, String)>) -> ExtractConfig {
    // Merge raw pairs first so clj-pulse overrides clj-kondo per macro, then map
    // targets. Mapping after the overlay lets a clj-pulse remap to a target
    // that does nothing (e.g. `clojure.core/->`) drop the macro entirely.
    let mut raw: HashMap<String, String> = HashMap::new();
    for (macro_fqn, target) in kondo_pairs.into_iter().chain(pulse_pairs) {
        raw.insert(macro_fqn, target);
    }

    let mut lint_as = HashMap::new();
    let mut lint_as_forms = HashMap::new();
    for (macro_fqn, target) in raw {
        let (ns, name) = target
            .rsplit_once('/')
            .map_or((None, target.as_str()), |(ns, n)| (Some(ns), n));
        // clj-kondo's own catch-all: the first symbol names a var, the rest
        // is not checked.
        let kind = if target == "clj-kondo.lint-as/def-catch-all" {
            Some(DefKind::Def)
        } else {
            DefKind::from_def_symbol(name)
        };
        if let Some(kind) = kind {
            lint_as.insert(macro_fqn, kind);
        } else if matches!(ns, None | Some("clojure.core" | "cljs.core"))
            && BINDING_FORMS.contains(&name)
        {
            lint_as_forms.insert(macro_fqn, name.to_string());
        } else {
            tracing::debug!(
                "settings: ignoring :lint-as target {} => {}",
                macro_fqn,
                target
            );
        }
    }
    ExtractConfig {
        lint_as,
        lint_as_forms,
    }
}

/// Loads the resolved [`ExtractConfig`] for the project rooted at `root`.
/// Reads `.clj-kondo/config.edn` and `.clj-pulse/config.edn` (both optional),
/// merges their `:lint-as` maps, and resolves them to [`DefKind`]s. Missing or
/// unparseable files contribute nothing.
pub fn load(root: &Path) -> ExtractConfig {
    let kondo_pairs = kondo::lint_as(root);
    let pulse_pairs = std::fs::read_to_string(root.join(".clj-pulse").join("config.edn"))
        .ok()
        .map(|src| kondo::parse_lint_as(&src))
        .unwrap_or_default();
    merge(kondo_pairs, pulse_pairs)
}

/// Loads the `:kondo` overrides from the project's `.clj-pulse/config.edn`.
/// Missing or unparseable files contribute nothing, so the editor layer and
/// the defaults decide on their own.
pub fn load_kondo(root: &Path) -> kondo::KondoOverride {
    std::fs::read_to_string(root.join(".clj-pulse").join("config.edn"))
        .ok()
        .map(|src| kondo::parse_config_edn(&src))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(m: &str, t: &str) -> (String, String) {
        (m.to_string(), t.to_string())
    }

    #[test]
    fn clj_pulse_wins_per_key() {
        let cfg = merge(
            vec![pair("dc/dc", "clojure.core/def")],
            vec![pair("dc/dc", "clojure.core/defn")],
        );
        assert_eq!(cfg.lint_as.get("dc/dc"), Some(&DefKind::Defn));
        assert_eq!(cfg.lint_as.len(), 1);
    }

    #[test]
    fn targets_split_into_defs_and_binding_forms() {
        let cfg = merge(
            vec![
                pair("dc/dc", "clojure.core/def"),
                pair("p/for-map", "clojure.core/for"),
                pair("p/fn->", "clojure.core/->"),
                pair("p/plet", "cljs.core/let"),
                pair("p/other-let", "other.ns/let"),
                pair("conch/programs", "clojure.core/declare"),
                pair("m/define-migration", "clj-kondo.lint-as/def-catch-all"),
            ],
            vec![],
        );
        assert_eq!(cfg.lint_as.get("dc/dc"), Some(&DefKind::Def));
        // `declare` defines nothing yet, but declares a var per name.
        assert_eq!(cfg.lint_as.get("conch/programs"), Some(&DefKind::Declare));
        assert_eq!(cfg.lint_as.get("m/define-migration"), Some(&DefKind::Def));
        assert_eq!(cfg.lint_as.len(), 3);
        assert_eq!(
            cfg.lint_as_forms.get("p/for-map").map(String::as_str),
            Some("for")
        );
        assert_eq!(
            cfg.lint_as_forms.get("p/plet").map(String::as_str),
            Some("let")
        );
        // A threading macro binds nothing; a `let` outside core is not core's.
        assert_eq!(cfg.lint_as_forms.len(), 2);
    }

    #[test]
    fn clj_pulse_remap_wins_across_kinds() {
        // clj-kondo says `def`, clj-pulse overrides to `for`: the macro binds
        // and must NOT survive as a def.
        let cfg = merge(
            vec![pair("x/x", "clojure.core/def")],
            vec![pair("x/x", "clojure.core/for")],
        );
        assert!(cfg.lint_as.is_empty());
        assert_eq!(
            cfg.lint_as_forms.get("x/x").map(String::as_str),
            Some("for")
        );
        let cfg = merge(
            vec![pair("x/x", "clojure.core/def")],
            vec![pair("x/x", "clojure.core/->")],
        );
        assert!(cfg.lint_as.is_empty() && cfg.lint_as_forms.is_empty());
    }

    #[test]
    fn bare_target_name_maps() {
        let cfg = merge(vec![pair("d", "def")], vec![]);
        assert_eq!(cfg.lint_as.get("d"), Some(&DefKind::Def));
    }

    #[test]
    fn empty_inputs_yield_empty() {
        assert!(merge(vec![], vec![]).lint_as.is_empty());
    }
}
