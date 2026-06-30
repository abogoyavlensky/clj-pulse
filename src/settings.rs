//! clj-pulse project settings.
//!
//! Loads `.clj-pulse/config.edn` (clj-pulse's own settings, primary) and merges
//! it over the compatible subset of `.clj-kondo/config.edn` (read via `kondo`),
//! producing the resolved [`ExtractConfig`] the extractor consumes. For now the
//! only setting is `:lint-as`.
//!
//! Merge rule: clj-pulse wins per key. The raw `(macro, target)` pairs are
//! merged first (so a clj-pulse entry fully overrides a clj-kondo one, even when
//! it remaps a macro to a non-def target), then each target is mapped to a
//! [`DefKind`]; targets that name no `def`-family form (e.g. `clojure.core/for`)
//! define nothing and are dropped.
//!
//! `.clj-pulse/config.edn` mirrors clj-kondo's `{:lint-as {sym sym}}` shape, so
//! both files are parsed by [`crate::kondo::parse_lint_as`].

use std::collections::HashMap;
use std::path::Path;

use crate::index::{DefKind, ExtractConfig};
use crate::kondo;

/// Merges clj-kondo `:lint-as` pairs (base) with clj-pulse pairs (overlay,
/// wins per key) into an [`ExtractConfig`]. Non-def targets are dropped.
fn merge(kondo_pairs: Vec<(String, String)>, pulse_pairs: Vec<(String, String)>) -> ExtractConfig {
    // Merge raw pairs first so clj-pulse overrides clj-kondo per macro, then map
    // targets to DefKind. Mapping after the overlay lets a clj-pulse remap to a
    // non-def target (e.g. `clojure.core/for`) drop the macro entirely.
    let mut raw: HashMap<String, String> = HashMap::new();
    for (macro_fqn, target) in kondo_pairs.into_iter().chain(pulse_pairs) {
        raw.insert(macro_fqn, target);
    }

    let mut lint_as = HashMap::new();
    for (macro_fqn, target) in raw {
        let name = target.rsplit_once('/').map_or(target.as_str(), |(_, n)| n);
        match DefKind::from_def_symbol(name) {
            Some(kind) => {
                lint_as.insert(macro_fqn, kind);
            }
            None => tracing::debug!(
                "settings: ignoring non-def :lint-as target {} => {}",
                macro_fqn,
                target
            ),
        }
    }
    ExtractConfig { lint_as }
}

/// Loads the resolved [`ExtractConfig`] for the project rooted at `root`.
/// Reads `.clj-kondo/config.edn` and `.clj-pulse/config.edn` (both optional),
/// merges their `:lint-as` maps, and resolves them to [`DefKind`]s. Missing or
/// unparseable files contribute nothing.
pub(crate) fn load(root: &Path) -> ExtractConfig {
    let kondo_pairs = kondo::lint_as(root);
    let pulse_pairs = std::fs::read_to_string(root.join(".clj-pulse").join("config.edn"))
        .ok()
        .map(|src| kondo::parse_lint_as(&src))
        .unwrap_or_default();
    merge(kondo_pairs, pulse_pairs)
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
    fn non_def_targets_are_dropped() {
        let cfg = merge(
            vec![
                pair("dc/dc", "clojure.core/def"),
                pair("p/for-map", "clojure.core/for"),
                pair("p/fn->", "clojure.core/->"),
            ],
            vec![],
        );
        assert_eq!(cfg.lint_as.get("dc/dc"), Some(&DefKind::Def));
        assert!(!cfg.lint_as.contains_key("p/for-map"));
        assert!(!cfg.lint_as.contains_key("p/fn->"));
        assert_eq!(cfg.lint_as.len(), 1);
    }

    #[test]
    fn clj_pulse_remap_to_non_def_removes_entry() {
        // clj-kondo says `def`, clj-pulse overrides to `for` (a non-def): the
        // macro must NOT survive as a def.
        let cfg = merge(
            vec![pair("x/x", "clojure.core/def")],
            vec![pair("x/x", "clojure.core/for")],
        );
        assert!(cfg.lint_as.is_empty());
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
