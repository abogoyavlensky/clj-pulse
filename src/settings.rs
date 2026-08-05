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
/// Classpath-resolution settings from `.clj-pulse/config.edn`'s `:classpath`
/// key: `{:classpath {:enabled false :aliases [:dev :test]}}`. Every missing or
/// unreadable piece falls back to its default, so a config that never mentions
/// `:classpath` gets automatic resolution with the `:dev`/`:test` aliases.
#[derive(Debug, Clone, PartialEq)]
pub struct ClasspathConfig {
    pub enabled: bool,
    /// Alias names without the leading `:`; qualified keywords keep the
    /// `namespace/name` form.
    pub aliases: Vec<String>,
}

impl Default for ClasspathConfig {
    fn default() -> Self {
        ClasspathConfig {
            enabled: true,
            aliases: vec!["dev".to_string(), "test".to_string()],
        }
    }
}

/// Loads the [`ClasspathConfig`] for the project rooted at `root`.
///
/// A non-empty `CLJ_PULSE_DISABLE_CLASSPATH_CLI` env var forces
/// `enabled = false` regardless of config: the e2e harness sets it because
/// its fixtures contain a `deps.edn`, and without the switch every harness
/// test would spawn `clojure`.
pub fn classpath(root: &Path) -> ClasspathConfig {
    let mut cfg = std::fs::read_to_string(root.join(".clj-pulse").join("config.edn"))
        .ok()
        .map(|src| parse_classpath(&src))
        .unwrap_or_default();
    if std::env::var("CLJ_PULSE_DISABLE_CLASSPATH_CLI").is_ok_and(|v| !v.is_empty()) {
        cfg.enabled = false;
    }
    cfg
}

fn parse_classpath(contents: &str) -> ClasspathConfig {
    use crate::edn::{get, kw};
    use edn_format::Value;

    let mut cfg = ClasspathConfig::default();
    let Ok(Value::Map(top)) = edn_format::parse_str(contents) else {
        return cfg;
    };
    let Some(Value::Map(spec)) = get(&top, kw("classpath")) else {
        return cfg;
    };

    if let Some(Value::Boolean(enabled)) = get(spec, kw("enabled")) {
        cfg.enabled = *enabled;
    }
    if let Some(Value::Vector(aliases)) = get(spec, kw("aliases")) {
        // Keywords are the natural spelling; strings are accepted leniently.
        // An explicitly empty vector is honored: it means "plain `clojure
        // -Spath`, no aliases" — only an *absent* key keeps the defaults.
        cfg.aliases = aliases
            .iter()
            .filter_map(|v| match v {
                Value::Keyword(k) => Some(match k.namespace() {
                    Some(ns) => format!("{}/{}", ns, k.name()),
                    None => k.name().to_string(),
                }),
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
    }
    cfg
}

pub fn load(root: &Path) -> ExtractConfig {
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

    fn write_config(dir: &Path, contents: &str) {
        let cfg_dir = dir.join(".clj-pulse");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.edn"), contents).unwrap();
    }

    #[test]
    fn classpath_defaults_when_no_config_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = classpath(dir.path());
        assert!(cfg.enabled);
        assert_eq!(cfg.aliases, vec!["dev".to_string(), "test".to_string()]);
    }

    #[test]
    fn classpath_defaults_when_key_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        write_config(dir.path(), "{:lint-as {a/b clojure.core/def}}");
        let cfg = classpath(dir.path());
        assert!(cfg.enabled);
        assert_eq!(cfg.aliases, vec!["dev".to_string(), "test".to_string()]);
    }

    #[test]
    fn classpath_enabled_false_keeps_default_aliases() {
        let dir = tempfile::TempDir::new().unwrap();
        write_config(dir.path(), "{:classpath {:enabled false}}");
        let cfg = classpath(dir.path());
        assert!(!cfg.enabled);
        assert_eq!(cfg.aliases, vec!["dev".to_string(), "test".to_string()]);
    }

    #[test]
    fn classpath_aliases_from_keywords_incl_qualified() {
        let dir = tempfile::TempDir::new().unwrap();
        write_config(dir.path(), "{:classpath {:aliases [:bench :ci/int]}}");
        let cfg = classpath(dir.path());
        assert!(cfg.enabled);
        assert_eq!(cfg.aliases, vec!["bench".to_string(), "ci/int".to_string()]);
    }

    #[test]
    fn classpath_aliases_accept_strings() {
        let dir = tempfile::TempDir::new().unwrap();
        write_config(dir.path(), r#"{:classpath {:aliases ["dev"]}}"#);
        let cfg = classpath(dir.path());
        assert_eq!(cfg.aliases, vec!["dev".to_string()]);
    }

    #[test]
    fn classpath_explicitly_empty_aliases_stay_empty() {
        // `[]` means "resolve with no aliases" (plain `clojure -Spath`), not
        // "use the defaults".
        let dir = tempfile::TempDir::new().unwrap();
        write_config(dir.path(), "{:classpath {:aliases []}}");
        let cfg = classpath(dir.path());
        assert!(cfg.enabled);
        assert!(cfg.aliases.is_empty());
    }

    #[test]
    fn classpath_malformed_edn_falls_back_to_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        write_config(dir.path(), "{:classpath {:enabled");
        let cfg = classpath(dir.path());
        assert!(cfg.enabled);
        assert_eq!(cfg.aliases, vec!["dev".to_string(), "test".to_string()]);
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
