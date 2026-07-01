//! clj-kondo configuration compatibility.
//!
//! Reads the subset of `.clj-kondo/config.edn` that clj-pulse understands. For
//! now that is only `:lint-as`, the map clj-kondo uses to treat a custom macro
//! like a known one (`{defcomponent/defcomponent clojure.core/def}`). We read
//! the project's `.clj-kondo/config.edn` only - the `config/` directory,
//! JAR-exported configs, and the `~/.config` global are not consulted yet.
//!
//! This module is the compatibility boundary: it returns raw
//! `(macro-fqn, target-fqn)` string pairs and makes no decision about which
//! targets are meaningful. `settings` owns the merge and the mapping to
//! `DefKind`.

use std::path::Path;

use edn_format::Value;

use crate::edn::{get, kw};

/// A `Value::Symbol` rendered as `ns/name` (or `name` when unqualified), else
/// `None` for any non-symbol value.
fn sym_to_string(value: &Value) -> Option<String> {
    let Value::Symbol(sym) = value else {
        return None;
    };
    Some(match sym.namespace() {
        Some(ns) => format!("{}/{}", ns, sym.name()),
        None => sym.name().to_string(),
    })
}

/// Parses the top-level `:lint-as` map of a clj-kondo config, returning each
/// `(macro-fqn, target-fqn)` pair as fully-qualified symbol strings. Returns an
/// empty vec when the input is not a map, has no `:lint-as` map, or that map is
/// empty; non-symbol keys/values are skipped.
pub(crate) fn parse_lint_as(edn: &str) -> Vec<(String, String)> {
    let Ok(Value::Map(top)) = edn_format::parse_str(edn) else {
        return vec![];
    };
    let Some(Value::Map(map)) = get(&top, kw("lint-as")) else {
        return vec![];
    };
    map.iter()
        .filter_map(|(k, v)| Some((sym_to_string(k)?, sym_to_string(v)?)))
        .collect()
}

/// Reads `root/.clj-kondo/config.edn` and returns its `:lint-as` pairs. Missing
/// or unparseable files yield an empty vec - clj-kondo config is optional.
pub(crate) fn lint_as(root: &Path) -> Vec<(String, String)> {
    let path = root.join(".clj-kondo").join("config.edn");
    match std::fs::read_to_string(&path) {
        Ok(src) => parse_lint_as(&src),
        Err(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(pairs: &[(String, String)], macro_fqn: &str, target: &str) -> bool {
        pairs.iter().any(|(m, t)| m == macro_fqn && t == target)
    }

    #[test]
    fn parses_lint_as_pairs() {
        let edn = r#"{:linters {:foo {:level :off}}
                      :lint-as {defcomponent/defcomponent clojure.core/def
                                plumbing.core/for-map clojure.core/for}}"#;
        let pairs = parse_lint_as(edn);
        assert_eq!(pairs.len(), 2);
        assert!(has(&pairs, "defcomponent/defcomponent", "clojure.core/def"));
        assert!(has(&pairs, "plumbing.core/for-map", "clojure.core/for"));
    }

    #[test]
    fn missing_lint_as_yields_empty() {
        assert!(parse_lint_as(r#"{:linters {:foo {:level :off}}}"#).is_empty());
    }

    #[test]
    fn non_map_input_yields_empty() {
        assert!(parse_lint_as("123").is_empty());
        assert!(parse_lint_as("not edn (((").is_empty());
    }

    #[test]
    fn lint_as_value_not_a_map_yields_empty() {
        assert!(parse_lint_as(r#"{:lint-as :nope}"#).is_empty());
    }

    #[test]
    fn unqualified_symbols_are_kept_as_bare_names() {
        // Rare, but a bare symbol key/value should round-trip without a slash.
        let pairs = parse_lint_as(r#"{:lint-as {defthing def}}"#);
        assert!(has(&pairs, "defthing", "def"));
    }
}
