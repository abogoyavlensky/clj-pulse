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
    pub core_symbols: Vec<CoreSymbol>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            symbols: DashMap::new(),
            namespaces: DashMap::new(),
            ns_symbols: DashMap::new(),
            file_to_ns: DashMap::new(),
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
        if let Some((_, ns_name)) = self.file_to_ns.remove(path) {
            if let Some((_, fqns)) = self.ns_symbols.remove(&ns_name) {
                for fqn in fqns {
                    self.symbols.remove(&fqn);
                }
            }
            self.namespaces.remove(&ns_name);
        }
    }

    pub fn insert_file(&self, meta: NsMeta, symbols: Vec<Symbol>) {
        let ns_name = meta.name.clone();
        let file = meta.file.clone();

        let mut fqns = Vec::with_capacity(symbols.len());
        for sym in symbols {
            fqns.push(sym.fqn.clone());
            self.symbols.insert(sym.fqn.clone(), sym);
        }

        self.ns_symbols.insert(ns_name.clone(), fqns);
        self.file_to_ns.insert(file, ns_name.clone());
        self.namespaces.insert(ns_name, meta);
    }

    pub fn file_ns(&self, path: &Path) -> Option<String> {
        self.file_to_ns.get(path).map(|r| r.clone())
    }
}
