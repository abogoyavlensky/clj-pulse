//! ClojureDocs lookup: which `ns/name` the word under the cursor means, and
//! the wire shape of the `clojurePulse/clojureDocs` answer.
//!
//! Resolution reuses [`resolve_symbol`] — the same alias/refer/core lookup
//! hover uses — and adds a fallback for namespaces ClojureDocs covers but the
//! index may not have yet (`clojure.string` before any jar is indexed): a
//! qualified word expands its alias through the current ns form, a bare word
//! is tried in `clojure.core`. The lookup, not the resolver, decides whether
//! an entry exists.

use serde::Serialize;

use super::{resolve_symbol, ResolvedSymbol};
use crate::clojuredocs::{ClojureDocs, Entry};
use crate::index::Index;

/// The `clojurePulse/clojureDocs` response: the var the request resolved to
/// (`None` when nothing was under the cursor) and its entry, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocsResult {
    pub symbol: Option<String>,
    pub entry: Option<DocsEntry>,
}

/// An entry on the wire; camelCase keys for the editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocsEntry {
    pub ns: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub arglists: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<String>,
    pub examples: Vec<String>,
    pub see_alsos: Vec<String>,
    pub url: String,
}

impl From<&Entry> for DocsEntry {
    fn from(entry: &Entry) -> Self {
        Self {
            ns: entry.ns.clone(),
            name: entry.name.clone(),
            doc: entry.doc.clone(),
            arglists: entry.arglists.clone(),
            added: entry.added.clone(),
            examples: entry.examples.clone(),
            see_alsos: entry.see_alsos.clone(),
            url: entry.url.clone(),
        }
    }
}

/// The `ns/name` to look up for `word` as seen from `current_ns`, or `None`
/// when the word cannot name a var at all.
pub fn resolve_var(index: &Index, word: &str, current_ns: &str) -> Option<String> {
    // `#'map` and `'map` name the same var as `map`.
    let word = word
        .strip_prefix("#'")
        .or_else(|| word.strip_prefix('\''))
        .unwrap_or(word);
    if word.is_empty() {
        return None;
    }

    if let Some(resolved) = resolve_symbol(index, word, current_ns) {
        return Some(match resolved {
            ResolvedSymbol::Project(sym) => sym.fqn,
            ResolvedSymbol::Core(core) | ResolvedSymbol::LetgoNative(core) => {
                format!("clojure.core/{}", core.name)
            }
            // ClojureDocs files `if`, `do`, `let`, … under clojure.core.
            ResolvedSymbol::SpecialForm(sf) => format!("clojure.core/{}", sf.name),
        });
    }

    match word.split_once('/') {
        // `str/join` with the alias known, or a literal `clojure.set/union`.
        Some((alias, name)) if !alias.is_empty() && !name.is_empty() => {
            let ns = index
                .ns_meta(current_ns)
                .and_then(|meta| meta.aliases.get(alias).cloned())
                .unwrap_or_else(|| alias.to_string());
            Some(format!("{ns}/{name}"))
        }
        // `foo/` names nothing; a bare `/` is clojure.core's division.
        Some((_, name)) if !name.is_empty() || word == "/" => Some(format!("clojure.core/{word}")),
        Some(_) => None,
        None => Some(format!("clojure.core/{word}")),
    }
}

/// The response for a resolved var.
pub fn lookup(docs: &ClojureDocs, fqn: &str) -> DocsResult {
    DocsResult {
        symbol: Some(fqn.to_string()),
        entry: docs.get(fqn).map(DocsEntry::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{core, extractor, scanner, ExtractConfig};
    use std::path::Path;

    fn fixture_index() -> Index {
        let root = Path::new("tests/fixtures/simple_project");
        let mut index =
            scanner::build_index(root, &[root.join("src")], &ExtractConfig::default()).unwrap();
        index.core_symbols = core::core_symbols();
        index
    }

    /// A core-only index plus one namespace with a `clojure.string` alias and
    /// nothing indexed for `clojure.string` itself.
    fn aliased_index() -> Index {
        let index = Index::new_with_core();
        let (meta, symbols) = extractor::extract(
            "(ns demo (:require [clojure.string :as str]))\n(defn go [] (str/join \",\" [1]))\n",
            Path::new("demo.clj"),
        )
        .unwrap();
        index.insert_file(meta, symbols, vec![]);
        index
    }

    #[test]
    fn core_symbol_resolves_to_clojure_core() {
        let index = fixture_index();
        assert_eq!(
            resolve_var(&index, "map", "simple.core").as_deref(),
            Some("clojure.core/map")
        );
    }

    #[test]
    fn project_symbol_resolves_to_its_fqn() {
        let index = fixture_index();
        assert_eq!(
            resolve_var(&index, "add", "simple.core").as_deref(),
            Some("simple.core/add")
        );
    }

    #[test]
    fn special_form_resolves_to_clojure_core() {
        let index = fixture_index();
        assert_eq!(
            resolve_var(&index, "if", "simple.core").as_deref(),
            Some("clojure.core/if")
        );
    }

    #[test]
    fn alias_expands_without_an_indexed_namespace() {
        let index = aliased_index();
        assert_eq!(
            resolve_var(&index, "str/join", "demo").as_deref(),
            Some("clojure.string/join")
        );
    }

    #[test]
    fn unknown_qualifier_is_taken_literally() {
        let index = aliased_index();
        assert_eq!(
            resolve_var(&index, "clojure.set/union", "demo").as_deref(),
            Some("clojure.set/union")
        );
    }

    #[test]
    fn var_quote_and_quote_are_stripped() {
        let index = fixture_index();
        assert_eq!(
            resolve_var(&index, "#'map", "simple.core").as_deref(),
            Some("clojure.core/map")
        );
        assert_eq!(
            resolve_var(&index, "'map", "simple.core").as_deref(),
            Some("clojure.core/map")
        );
    }

    #[test]
    fn unknown_bare_word_falls_back_to_clojure_core() {
        let index = fixture_index();
        assert_eq!(
            resolve_var(&index, "frobnicate", "simple.core").as_deref(),
            Some("clojure.core/frobnicate")
        );
    }

    #[test]
    fn division_and_malformed_words() {
        let index = fixture_index();
        assert_eq!(
            resolve_var(&index, "/", "simple.core").as_deref(),
            Some("clojure.core//")
        );
        assert_eq!(resolve_var(&index, "foo/", "simple.core"), None);
        assert_eq!(resolve_var(&index, "", "simple.core"), None);
        assert_eq!(resolve_var(&index, "#'", "simple.core"), None);
    }

    #[test]
    fn result_serializes_camel_case_and_null_entry() {
        let docs = crate::clojuredocs::parse(
            r#"{"vars": [{"ns": "clojure.core", "name": "map", "arglists": ["f coll"],
                "examples": [{"body": "(map inc [1])"}],
                "see-alsos": [{"to-var": {"ns": "clojure.core", "name": "mapv"}}]}]}"#,
        )
        .unwrap();
        let found = serde_json::to_value(lookup(&docs, "clojure.core/map")).unwrap();
        assert_eq!(found["symbol"], "clojure.core/map");
        assert_eq!(found["entry"]["arglists"][0], "[f coll]");
        assert_eq!(found["entry"]["seeAlsos"][0], "clojure.core/mapv");
        assert_eq!(
            found["entry"]["url"],
            "https://clojuredocs.org/clojure.core/map"
        );
        assert!(found["entry"].get("doc").is_none());
        assert!(found["entry"].get("notes").is_none());

        let missing = serde_json::to_value(lookup(&docs, "clojure.core/frob")).unwrap();
        assert_eq!(missing["symbol"], "clojure.core/frob");
        assert!(missing["entry"].is_null());
    }
}
