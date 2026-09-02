//! ClojureDocs data, read from a local copy of the official export
//! (`https://clojuredocs.org/clojuredocs-export.json`) that the editor points
//! at — never the network. Clojure Pulse bundles a stripped copy and passes
//! its path in `initializationOptions`; any client may pass the raw download,
//! since this reader accepts the export's own shape with every field optional.
//!
//! Notes are never read: ClojureDocs states a license for examples (CC0) but
//! none for notes, so they are left out of everything the server serves.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

const SITE: &str = "https://clojuredocs.org";

/// One var's ClojureDocs entry, normalized for serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub ns: String,
    pub name: String,
    pub doc: Option<String>,
    /// Each arglist bracketed (`[f coll]`); the export stores them bare.
    pub arglists: Vec<String>,
    /// The Clojure version the var appeared in, when ClojureDocs knows it.
    pub added: Option<String>,
    /// Example bodies, in the export's order.
    pub examples: Vec<String>,
    /// Related vars as `ns/name`.
    pub see_alsos: Vec<String>,
    /// The var's page on clojuredocs.org.
    pub url: String,
}

/// The loaded export, keyed by `ns/name`.
#[derive(Debug, Default)]
pub struct ClojureDocs {
    entries: HashMap<String, Entry>,
}

impl ClojureDocs {
    pub fn get(&self, fqn: &str) -> Option<&Entry> {
        self.entries.get(fqn)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// The export's shape. Everything defaults so a stripped copy, the raw
// download, and future fields all parse; unknown fields are ignored. The raw
// export writes `null` (not an absent key) for empty collections on hundreds
// of vars, which `#[serde(default)]` alone does not cover — hence `null_vec`.

fn null_vec<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Export {
    #[serde(deserialize_with = "null_vec")]
    vars: Vec<RawVar>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawVar {
    ns: Option<String>,
    name: Option<String>,
    doc: Option<String>,
    #[serde(deserialize_with = "null_vec")]
    arglists: Vec<String>,
    added: Option<String>,
    href: Option<String>,
    #[serde(deserialize_with = "null_vec")]
    examples: Vec<RawExample>,
    #[serde(rename = "see-alsos", deserialize_with = "null_vec")]
    see_alsos: Vec<RawSeeAlso>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawExample {
    body: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawSeeAlso {
    #[serde(rename = "to-var")]
    to_var: Option<RawRef>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRef {
    ns: Option<String>,
    name: Option<String>,
}

/// Parses export JSON. Vars without both `ns` and `name` are skipped.
pub fn parse(json: &str) -> Result<ClojureDocs> {
    let export: Export = serde_json::from_str(json).context("parsing ClojureDocs export")?;
    let mut entries = HashMap::with_capacity(export.vars.len());
    for var in export.vars {
        let (Some(ns), Some(name)) = (var.ns, var.name) else {
            continue;
        };
        if ns.is_empty() || name.is_empty() {
            continue;
        }
        // ClojureDocs munges names in URLs (`ends-with?` → `ends-with_q`), so
        // the export's `href` is authoritative when present.
        let href = var.href.unwrap_or_else(|| format!("/{ns}/{name}"));
        let entry = Entry {
            doc: var.doc,
            arglists: var.arglists.iter().map(|a| bracket(a)).collect(),
            added: var.added,
            examples: var.examples.into_iter().filter_map(|e| e.body).collect(),
            see_alsos: var
                .see_alsos
                .into_iter()
                .filter_map(|s| s.to_var)
                .filter_map(|r| Some(format!("{}/{}", r.ns?, r.name?)))
                .collect(),
            url: format!("{SITE}{href}"),
            ns: ns.clone(),
            name: name.clone(),
        };
        entries.insert(format!("{ns}/{name}"), entry);
    }
    Ok(ClojureDocs { entries })
}

/// `f coll` → `[f coll]`; an already bracketed arglist is left alone.
fn bracket(arglist: &str) -> String {
    let a = arglist.trim();
    if a.starts_with('[') {
        a.to_string()
    } else {
        format!("[{a}]")
    }
}

/// Reads and parses the export at `path`.
pub fn load(path: &Path) -> Result<ClojureDocs> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading ClojureDocs export {}", path.display()))?;
    parse(&text)
}

/// The data file the editor configured: `initializationOptions.clojuredocs.path`.
pub fn path_from_init_options(options: &serde_json::Value) -> Option<PathBuf> {
    options
        .get("clojuredocs")?
        .get("path")?
        .as_str()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EXPORT: &str = r#"{
      "created-at": 1788243505643,
      "description": "ClojureDocs Data Export",
      "vars": [
        {"ns": "clojure.core", "name": "map", "type": "function", "added": "1.0",
         "href": "/clojure.core/map", "file": "clojure/core.clj",
         "doc": "Returns a lazy sequence.",
         "arglists": ["f", "f coll", "[x]"],
         "examples": [
           {"body": "(map inc [1 2 3])", "author": {"login": "a", "avatar-url": "x"}, "_id": "1"},
           {"body": ";; two", "created-at": 1}
         ],
         "notes": [{"body": "a note"}],
         "see-alsos": [{"to-var": {"ns": "clojure.core", "name": "mapv", "library-url": "u"}, "_id": "2"}]},
        {"ns": "clojure.string", "name": "join",
         "doc": "Joins.", "notes": [{"body": "n"}]},
        {"ns": "clojure.core", "name": "nulls", "doc": null, "added": null, "href": null,
         "arglists": null, "examples": null, "see-alsos": null, "notes": null},
        {"name": "orphan", "doc": "no ns"},
        {"ns": "clojure.core", "doc": "no name"}
      ]
    }"#;

    #[test]
    fn parses_entries_with_normalized_fields() {
        let docs = parse(EXPORT).unwrap();
        assert_eq!(docs.len(), 3);
        let map = docs.get("clojure.core/map").unwrap();
        assert_eq!(map.ns, "clojure.core");
        assert_eq!(map.name, "map");
        assert_eq!(map.doc.as_deref(), Some("Returns a lazy sequence."));
        assert_eq!(map.arglists, vec!["[f]", "[f coll]", "[x]"]);
        assert_eq!(map.added.as_deref(), Some("1.0"));
        assert_eq!(map.examples, vec!["(map inc [1 2 3])", ";; two"]);
        assert_eq!(map.see_alsos, vec!["clojure.core/mapv"]);
        assert_eq!(map.url, "https://clojuredocs.org/clojure.core/map");
    }

    #[test]
    fn minimal_var_gets_defaults_and_derived_url() {
        let docs = parse(EXPORT).unwrap();
        let join = docs.get("clojure.string/join").unwrap();
        assert!(join.arglists.is_empty());
        assert!(join.examples.is_empty());
        assert!(join.see_alsos.is_empty());
        assert_eq!(join.added, None);
        assert_eq!(join.url, "https://clojuredocs.org/clojure.string/join");
    }

    #[test]
    fn null_fields_read_as_empty() {
        // The raw export writes `null` for empty collections and unknown
        // scalars; each must read as empty rather than fail the whole file.
        let docs = parse(EXPORT).unwrap();
        let nulls = docs.get("clojure.core/nulls").unwrap();
        assert!(nulls.arglists.is_empty());
        assert!(nulls.examples.is_empty());
        assert!(nulls.see_alsos.is_empty());
        assert_eq!(nulls.doc, None);
        assert_eq!(nulls.added, None);
        assert_eq!(nulls.url, "https://clojuredocs.org/clojure.core/nulls");
        assert!(parse(r#"{"vars": null}"#).unwrap().is_empty());
    }

    /// Against a real download: `CLJ_PULSE_CLOJUREDOCS_EXPORT=/path/to/clojuredocs-export.json
    /// cargo test clojuredocs -- --ignored`.
    #[test]
    #[ignore]
    fn loads_real_export_from_env() {
        let path = std::env::var("CLJ_PULSE_CLOJUREDOCS_EXPORT")
            .expect("set CLJ_PULSE_CLOJUREDOCS_EXPORT");
        let docs = load(Path::new(&path)).unwrap();
        assert!(docs.len() > 1000, "only {} vars", docs.len());
        let map = docs.get("clojure.core/map").unwrap();
        assert!(map.arglists.iter().all(|a| a.starts_with('[')));
        assert!(!map.examples.is_empty());
    }

    #[test]
    fn vars_without_ns_or_name_are_skipped() {
        let docs = parse(EXPORT).unwrap();
        assert!(docs.get("clojure.core/orphan").is_none());
        assert!(docs.get("/orphan").is_none());
    }

    #[test]
    fn unknown_fields_and_notes_are_ignored() {
        // Notes exist in the fixture; nothing on Entry can carry them, so the
        // check is that parsing succeeds with them and other extras present.
        let docs = parse(r#"{"vars": [{"ns": "a", "name": "b", "notes": [{"body": "x"}], "extra": 1}], "extra": true}"#).unwrap();
        assert!(docs.get("a/b").is_some());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse("{not json").is_err());
    }

    #[test]
    fn path_from_init_options_reads_nested_key() {
        assert_eq!(
            path_from_init_options(&json!({"clojuredocs": {"path": "/x/y.json"}})),
            Some(PathBuf::from("/x/y.json"))
        );
        assert_eq!(
            path_from_init_options(&json!({"dependency-scheme": "jar"})),
            None
        );
        assert_eq!(path_from_init_options(&json!({"clojuredocs": "/x"})), None);
        assert_eq!(path_from_init_options(&json!(null)), None);
    }

    #[test]
    fn load_missing_file_is_an_error() {
        assert!(load(Path::new("/nonexistent/clojuredocs.json")).is_err());
    }
}
