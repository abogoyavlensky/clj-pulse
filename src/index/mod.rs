pub mod core;
pub mod extractor;
pub mod jar;
pub mod jar_cache;
pub mod jdk;
pub mod scanner;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Range;

/// Synthetic `file_to_ns` namespace for indexed EDN config files, which have no
/// real namespace. NUL-prefixed so it can never collide with a real namespace
/// or the empty-string ns of a no-`ns` `.clj` file. Lets `merge_project_from`'s
/// stale-filter keep EDN files across re-scans (see [`Index::insert_edn_file`]).
const EDN_NS_SENTINEL: &str = "\0edn";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SymbolSource {
    Project,
    Jar(PathBuf),
    /// Library source directory on the classpath (git deps in ~/.gitlibs,
    /// :local/root deps). Files are real paths on disk.
    Dir(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DefKind {
    Def,
    Defonce,
    Defn,
    DefnPrivate,
    Defmacro,
    Defmulti,
    Defmethod,
    Defprotocol,
    Defrecord,
    Deftype,
    /// An Integrant component key, defined by `(defmethod ig/init-key ::x …)`.
    /// Its `fqn` is the canonical colon-prefixed keyword (`:my.ns/x`), keyed
    /// disjointly from var fqns (which never start with `:`).
    IntegrantKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub fqn: String,
    pub ns: String,
    pub kind: DefKind,
    pub params: Vec<String>,
    pub doc: Option<String>,
    pub file: PathBuf,
    pub source: SymbolSource,
    pub range: Range,
    pub name_range: Range,
}

/// A resolved usage of a symbol in a project file. For symbols, `name_range`
/// covers only the name part of a qualified usage (`core/add` → just `add`), so
/// rename edits never touch the alias. Keyword occurrences (fqn starts with
/// `:`) instead span the whole keyword token — navigation-only in v1; keyword
/// rename is rejected.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub fqn: String,
    pub name_range: Range,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NsMeta {
    pub name: String,
    pub file: PathBuf,
    pub aliases: HashMap<String, String>,
    pub refers: HashMap<String, String>,
    /// Every namespace required by this file, regardless of `:as`/`:refer`
    /// (a plain `[clojure.set]` lands here too). Used to tell whether a
    /// qualified usage's namespace is already required.
    pub requires: Vec<String>,
}

impl NsMeta {
    /// Whether `prefix` is resolvable from this file: its own namespace name,
    /// an `:as` alias, or a required namespace (plain `[clojure.set]` included).
    pub fn resolves_prefix(&self, prefix: &str) -> bool {
        prefix == self.name
            || self.aliases.contains_key(prefix)
            || self.requires.iter().any(|r| r == prefix)
    }
}

#[derive(Debug, Clone)]
pub struct CoreSymbol {
    pub name: String,
    pub params: String,
    pub doc: String,
}

pub struct Index {
    pub symbols: DashMap<String, Symbol>,
    pub namespaces: DashMap<String, NsMeta>,
    pub ns_symbols: DashMap<String, Vec<String>>,
    pub file_to_ns: DashMap<PathBuf, String>,
    /// Resolved symbol usages per project file (libraries excluded).
    pub occurrences: DashMap<PathBuf, Vec<Occurrence>>,
    pub core_symbols: Vec<CoreSymbol>,
    /// Set once let-go's built-in `core` namespace has been indexed from the
    /// fetched let-go source. Interior mutability because the `Arc<Index>` is
    /// already shared with handlers when background library indexing runs.
    letgo_core: AtomicBool,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            symbols: DashMap::new(),
            namespaces: DashMap::new(),
            ns_symbols: DashMap::new(),
            file_to_ns: DashMap::new(),
            occurrences: DashMap::new(),
            core_symbols: Vec::new(),
            letgo_core: AtomicBool::new(false),
        }
    }
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_with_core() -> Self {
        Self {
            core_symbols: core::core_symbols(),
            ..Self::default()
        }
    }

    pub fn lookup(&self, fqn: &str) -> Option<Symbol> {
        self.symbols.get(fqn).map(|r| r.clone())
    }

    pub fn lookup_in_ns(&self, ns: &str, name: &str) -> Option<Symbol> {
        let fqn = format!("{}/{}", ns, name);
        self.lookup(&fqn)
    }

    pub fn complete(&self, _prefix: &str, _current_ns: &str) -> Vec<Symbol> {
        vec![]
    }

    pub fn ns_meta(&self, ns: &str) -> Option<NsMeta> {
        self.namespaces.get(ns).map(|r| r.clone())
    }

    /// Records that let-go's built-in `core` namespace has been indexed, so the
    /// bare-word resolver treats `core` as the auto-referred builtin instead of
    /// the static clojure.core list.
    pub fn mark_letgo_core(&self) {
        self.letgo_core.store(true, Ordering::Relaxed);
    }

    /// Whether let-go core has been indexed (see [`Index::mark_letgo_core`]).
    pub fn letgo_core(&self) -> bool {
        self.letgo_core.load(Ordering::Relaxed)
    }

    pub fn remove_file(&self, path: &Path) {
        self.occurrences.remove(path);
        if let Some((_, ns_name)) = self.file_to_ns.remove(path) {
            if let Some((_, fqns)) = self.ns_symbols.remove(&ns_name) {
                for fqn in fqns {
                    self.symbols.remove(&fqn);
                }
            }
            self.namespaces.remove(&ns_name);
        }
    }

    pub fn insert_file(&self, meta: NsMeta, symbols: Vec<Symbol>, occurrences: Vec<Occurrence>) {
        let ns_name = meta.name.clone();
        let file = meta.file.clone();

        let mut fqns = Vec::with_capacity(symbols.len());
        for sym in symbols {
            fqns.push(sym.fqn.clone());
            self.symbols.insert(sym.fqn.clone(), sym);
        }

        self.ns_symbols.insert(ns_name.clone(), fqns);
        self.occurrences.insert(file.clone(), occurrences);
        self.file_to_ns.insert(file, ns_name.clone());
        self.namespaces.insert(ns_name, meta);
    }

    /// Inserts an EDN config file's keyword occurrences. EDN files contribute
    /// only occurrences — no namespace, no symbols — so this touches only
    /// `occurrences` and registers the file under [`EDN_NS_SENTINEL`] in
    /// `file_to_ns` (which keeps `merge_project_from` from dropping it). It
    /// deliberately leaves `namespaces`/`ns_symbols` untouched; `remove_file`
    /// no-ops cleanly on the absent sentinel ns.
    pub fn insert_edn_file(&self, file: PathBuf, occurrences: Vec<Occurrence>) {
        self.occurrences.insert(file.clone(), occurrences);
        self.file_to_ns.insert(file, EDN_NS_SENTINEL.to_string());
    }

    pub fn file_ns(&self, path: &Path) -> Option<String> {
        self.file_to_ns.get(path).map(|r| r.clone())
    }

    /// Whether `path` is an editable project file. Project files always have an
    /// occurrences entry; JAR virtual paths and dir-library files never do, so
    /// this tells an editable buffer apart from read-only library source.
    pub fn is_project_path(&self, path: &Path) -> bool {
        self.occurrences.contains_key(path)
    }

    /// Merges a freshly built project index into this one, removing project
    /// files that no longer exist in the new scan (e.g. source roots dropped
    /// from deps.edn `:paths`). Files in `keep` (currently open documents,
    /// which may legitimately live outside `:paths`) and library entries are
    /// untouched.
    pub fn merge_project_from(&self, new_index: Index, keep: &std::collections::HashSet<PathBuf>) {
        // Project files are exactly the keys of `occurrences`
        let stale: Vec<PathBuf> = self
            .occurrences
            .iter()
            .map(|entry| entry.key().clone())
            .filter(|path| !new_index.file_to_ns.contains_key(path) && !keep.contains(path))
            .collect();
        for path in stale {
            self.remove_file(&path);
        }

        for entry in new_index.symbols.iter() {
            self.symbols
                .insert(entry.key().clone(), entry.value().clone());
        }
        for entry in new_index.namespaces.iter() {
            self.namespaces
                .insert(entry.key().clone(), entry.value().clone());
        }
        for entry in new_index.ns_symbols.iter() {
            self.ns_symbols
                .insert(entry.key().clone(), entry.value().clone());
        }
        for entry in new_index.file_to_ns.iter() {
            self.file_to_ns
                .insert(entry.key().clone(), entry.value().clone());
        }
        for entry in new_index.occurrences.iter() {
            self.occurrences
                .insert(entry.key().clone(), entry.value().clone());
        }
    }

    /// Removes all library-sourced data (JARs and classpath dirs), keeping
    /// project symbols and occurrences. Called when the classpath changes
    /// so removed dependencies don't linger in completion/navigation.
    pub fn clear_libs(&self) {
        // The let-go-core marker is library-derived state: a re-index that no
        // longer finds pinned core (`:lg-version` removed, project switched to
        // Clojure, source dir gone) must drop it, or the bare-word resolver
        // keeps skipping the static clojure.core fallback while `core` is empty.
        // `index_letgo_core` re-sets it when core is actually re-indexed.
        self.letgo_core.store(false, Ordering::Relaxed);

        self.symbols
            .retain(|_, sym| sym.source == SymbolSource::Project);
        self.ns_symbols.retain(|ns, fqns| {
            if fqns.iter().any(|fqn| self.symbols.contains_key(fqn)) {
                return true;
            }
            // Symbol-less namespaces: keep only project-owned ones (project
            // files always have an occurrences entry; jar virtual paths and
            // dir-lib files never do).
            fqns.is_empty()
                && self
                    .namespaces
                    .get(ns)
                    .map(|meta| self.occurrences.contains_key(&meta.file))
                    .unwrap_or(false)
        });
        self.namespaces
            .retain(|ns, _| self.ns_symbols.contains_key(ns));
        self.file_to_ns
            .retain(|_, ns| self.namespaces.contains_key(ns));
    }

    /// Inserts a library namespace (from a JAR or a classpath source dir)
    /// without ever shadowing project code. Project and library indexing run
    /// concurrently, so insertion order is nondeterministic; project sources
    /// must win regardless of which task finishes last.
    pub fn insert_lib_file(&self, meta: NsMeta, symbols: Vec<Symbol>) {
        use dashmap::mapref::entry::Entry;

        // Project files always have an occurrences entry; jar virtual paths
        // and dir-lib files never do.
        let ns_owned_by_project = self
            .namespaces
            .get(&meta.name)
            .map(|ns| self.occurrences.contains_key(&ns.file))
            .unwrap_or(false);
        if ns_owned_by_project {
            return;
        }

        let mut fqns = Vec::with_capacity(symbols.len());
        for sym in symbols {
            fqns.push(sym.fqn.clone());
            match self.symbols.entry(sym.fqn.clone()) {
                Entry::Occupied(mut e) => {
                    if e.get().source != SymbolSource::Project {
                        e.insert(sym);
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(sym);
                }
            }
        }

        self.ns_symbols.insert(meta.name.clone(), fqns);
        self.file_to_ns.insert(meta.file.clone(), meta.name.clone());
        self.namespaces.insert(meta.name.clone(), meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_libs_resets_letgo_core_marker() {
        // The marker is library-derived: clearing libs (e.g. on an lgx.edn
        // change that un-pins :lg-version) must drop it so the bare-word
        // resolver can fall back to the static clojure.core list again.
        let index = Index::new();
        index.mark_letgo_core();
        assert!(index.letgo_core());

        index.clear_libs();
        assert!(
            !index.letgo_core(),
            "clear_libs must reset the let-go core marker"
        );
    }
}
