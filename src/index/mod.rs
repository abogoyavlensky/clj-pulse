pub mod core;
pub mod extractor;
pub mod jar;
pub mod jar_cache;
pub mod scanner;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Range;

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

/// A resolved usage of a symbol in a project file. `name_range` covers only
/// the name part of a qualified usage (`core/add` → just `add`), so rename
/// edits never touch the alias.
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

    pub fn file_ns(&self, path: &Path) -> Option<String> {
        self.file_to_ns.get(path).map(|r| r.clone())
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
