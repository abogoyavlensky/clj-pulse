pub mod core;
pub mod extractor;
pub mod jar;
pub mod jar_cache;
pub mod jdk;
pub mod scanner;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

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
    /// A `clojure.test/deftest` var (or `deftest-`/`cljs.test/deftest`). Defines
    /// a zero-arg test fn, so it carries no params.
    Deftest,
    /// A name introduced by `(declare foo)`: a var with no value yet. Kept only
    /// when nothing else in the file defines the same name, so navigation
    /// reaches the declaration when the real definition is elsewhere (or made
    /// by a macro we don't model).
    Declare,
    /// An Integrant component key, defined by `(defmethod ig/init-key ::x …)`.
    /// Its `fqn` is the canonical colon-prefixed keyword (`:my.ns/x`), keyed
    /// disjointly from var fqns (which never start with `:`).
    IntegrantKey,
}

impl DefKind {
    /// Maps a `def`-family symbol name (`def`, `defn`, `defmacro`, …) to its
    /// `DefKind`. The single source of truth for the extractor's top-level form
    /// dispatch and the `:lint-as` reader (which maps a target like
    /// `clojure.core/defn` by its name). Returns `None` for non-def names, so a
    /// `:lint-as … clojure.core/for` entry is ignored - it defines nothing.
    pub(crate) fn from_def_symbol(name: &str) -> Option<DefKind> {
        Some(match name {
            "def" => DefKind::Def,
            "defonce" => DefKind::Defonce,
            "defn" => DefKind::Defn,
            "defn-" => DefKind::DefnPrivate,
            "defmacro" => DefKind::Defmacro,
            "defmulti" => DefKind::Defmulti,
            "defmethod" => DefKind::Defmethod,
            "defprotocol" => DefKind::Defprotocol,
            "defrecord" => DefKind::Defrecord,
            "deftype" => DefKind::Deftype,
            _ => return None,
        })
    }

    /// Maps a *resolved* list-head fqn of a well-known defining macro to the
    /// `DefKind` it introduces. Consulted after the user's `:lint-as` map, so
    /// a config entry for the same fqn wins.
    pub(crate) fn from_macro_fqn(fqn: &str) -> Option<DefKind> {
        match fqn {
            "clojure.test/deftest" | "clojure.test/deftest-" | "cljs.test/deftest" => {
                Some(DefKind::Deftest)
            }
            _ => None,
        }
    }
}

/// Project configuration that influences extraction. Resolved once at startup
/// (see `crate::settings::load`) and borrowed by the extractor; the default
/// (empty) reproduces the stock behavior exactly.
#[derive(Debug, Clone, Default)]
pub struct ExtractConfig {
    /// Macro fqn (`defcomponent/defcomponent`) → the `def`-family form it should
    /// be treated as, from a merged `:lint-as` map. Only def-like targets are
    /// kept; a lint-as'd form's defined name is then extracted as that kind.
    pub lint_as: HashMap<String, DefKind>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Whether the var is private (`defn-`, or `^:private` / `^{:private true}`
    /// on the name). Feeds the `unused-private-var` lint: a private var can
    /// only be used from its own file.
    #[serde(default)]
    pub private: bool,
}

/// A resolved usage of a symbol in a project file. For symbols, `name_range`
/// covers only the name part of a qualified usage (`core/add` → just `add`), so
/// rename edits never touch the alias. Keyword occurrences (fqn starts with
/// `:`) instead span the whole keyword token — navigation-only in v1; keyword
/// rename is rejected. Both notations are recorded: a qualified keyword under
/// the namespace it resolves to (`:my.ns/id`), an unqualified one under
/// `:id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub fqn: String,
    pub name_range: Range,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NsMeta {
    pub name: String,
    pub file: PathBuf,
    pub aliases: HashMap<String, String>,
    pub refers: HashMap<String, String>,
    /// Every namespace required by this file, regardless of `:as`/`:refer`
    /// (a plain `[clojure.set]` lands here too). Used to tell whether a
    /// qualified usage's namespace is already required.
    pub requires: Vec<String>,
    /// Java class simple name → fully-qualified name, from `(:import …)`.
    /// Resolves interop class references (`Date`, `Instant/now`, `(File. …)`)
    /// to JDK source. Empty for files with no `:import`.
    pub imports: HashMap<String, String>,
    /// Namespaces whose every public var is referred, from `[ns :refer :all]`
    /// or `(:use ns)`. Bare names in this file may resolve there.
    #[serde(default)]
    pub refer_all: Vec<String>,
    /// Namespaces bound only through `[ns :as-alias x]`. Their aliases are in
    /// `aliases` too, so keyword and qualified-symbol resolution work as usual,
    /// but the namespace is never loaded — so it stays out of `requires`, and a
    /// usage that spells the full name is still an unresolved namespace.
    #[serde(default)]
    pub as_aliases: Vec<String>,
    /// Core names this file excludes, from `(:refer-clojure :exclude [...])`.
    /// A bare usage of one of them is this namespace's own var, not core's, and
    /// core completion must not offer it.
    #[serde(default)]
    pub core_excludes: Vec<String>,
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

/// Which dialect a file asks for: the source of a `.cljs` file, or everything
/// else. A `.cljc` file, a `.lg` file, an EDN config and a `jar:` virtual path
/// are all judged by extension, so a `.cljs` entry inside a JAR is `Cljs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Clj,
    Cljs,
}

impl Dialect {
    pub fn of_path(path: &Path) -> Dialect {
        match path.extension().and_then(|e| e.to_str()) {
            Some("cljs") => Dialect::Cljs,
            _ => Dialect::Clj,
        }
    }
}

/// The namespace a bare core name belongs to in a file of `dialect`:
/// `cljs.core` in ClojureScript, `clojure.core` everywhere else (`.cljc`
/// included, since it asks as Clojure).
pub fn core_ns(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Cljs => "cljs.core",
        Dialect::Clj => "clojure.core",
    }
}

/// Rank of a file's entry when two files define the same fqn or namespace:
/// lower wins. Every project file outranks every library file; within each,
/// `.clj` is the copy a Clojure reader wants, `.cljc` serves both dialects,
/// `.cljs` is the ClojureScript copy `Index::cljs_symbols` keeps aside.
/// Anything else (`.lg`, a `.clj`-less entry) ranks with `.cljc`. Project
/// `.clj` 0, other 1, `.cljs` 2; library `.clj` 3, other 4, `.cljs` 5.
fn slot_rank(project: bool, path: &Path) -> u8 {
    let dialect_rank = match path.extension().and_then(|e| e.to_str()) {
        Some("clj") => 0,
        Some("cljs") => 2,
        _ => 1,
    };
    if project {
        dialect_rank
    } else {
        3 + dialect_rank
    }
}

/// Whether `rank` is a `.cljs` file's, the one rank whose loser is kept aside.
fn is_cljs_rank(rank: u8) -> bool {
    rank % 3 == 2
}

fn symbol_rank(sym: &Symbol) -> u8 {
    slot_rank(is_project(sym), &sym.file)
}

/// What one project source file contributed to the index, whatever the
/// primary and shadow slots made of it: its own namespace metadata and the
/// fqns it defines. Two same-dialect files of one namespace (two projects both
/// defining `user`) keep one metadata slot between them, so the loser's lives
/// here alone.
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub meta: NsMeta,
    pub fqns: Vec<String>,
}

/// One indexed project file as [`Index::file_entries`] hands it back: enough
/// to re-insert it into another index.
#[derive(Debug, Clone)]
pub enum FileEntry {
    Source {
        /// Boxed: `NsMeta` dwarfs the `Edn` variant.
        meta: Box<NsMeta>,
        symbols: Vec<Symbol>,
        occurrences: Vec<Occurrence>,
    },
    Edn {
        file: PathBuf,
        occurrences: Vec<Occurrence>,
    },
}

pub struct Index {
    pub symbols: DashMap<String, Symbol>,
    pub namespaces: DashMap<String, NsMeta>,
    pub ns_symbols: DashMap<String, Vec<String>>,
    /// The ClojureScript copy of a symbol that a Clojure copy displaced from
    /// `symbols` (or that arrived after one) — a library's, or a project
    /// namespace's `.cljs` half. Read only through [`Index::lookup_for`] /
    /// [`Index::prefer_dialect`] with `Dialect::Cljs` and [`Index::lookup_all`].
    /// Every key here is also a key of `symbols`: removing the primary entry
    /// promotes this one.
    cljs_symbols: DashMap<String, Symbol>,
    /// The ClojureScript copy of a namespace's metadata that a Clojure copy
    /// displaced from `namespaces`; the twin of `cljs_symbols`, read only
    /// through [`Index::ns_meta_for`].
    cljs_namespaces: DashMap<String, NsMeta>,
    /// What each project source file contributed (see [`FileRecord`]):
    /// removal is by file, so saving one half of a namespace never touches
    /// the other half's entries.
    files: DashMap<PathBuf, FileRecord>,
    /// The project files of each namespace, the keys of `files` grouped by
    /// their record's namespace: what refills a namespace's metadata slot when
    /// the file holding it is removed.
    ns_files: DashMap<String, Vec<PathBuf>>,
    pub file_to_ns: DashMap<PathBuf, String>,
    /// Resolved symbol usages per project file (libraries excluded).
    pub occurrences: DashMap<PathBuf, Vec<Occurrence>>,
    /// Keyword fqn → how many times the project uses it, aggregated over
    /// `occurrences`. Maintained at every mutation point of that map so keyword
    /// completion can rank by frequency without scanning every file.
    keyword_counts: DashMap<String, u32>,
    pub core_symbols: Vec<CoreSymbol>,
    /// Set once let-go's built-in `core` namespace has been indexed from the
    /// fetched let-go source. Interior mutability because the `Arc<Index>` is
    /// already shared with handlers when background library indexing runs.
    letgo_core: AtomicBool,
    /// Names let-go defines natively in Go (`ns.Def(...)` in `lang.go`),
    /// harvested from the pinned version's source at index time so completion
    /// and hover track the actual vars/fns of *this* let-go version (e.g. a
    /// newly added `*command-line-args*`) instead of a hardcoded list. Empty
    /// until harvested, or when no source is on disk — callers then fall back
    /// to the static `NATIVE_NAMES`. Kept sorted for binary search. Interior
    /// mutability because the `Arc<Index>` is already shared with handlers.
    letgo_native: RwLock<Vec<String>>,
    /// JDK source index for built-in Java navigation/completion. Set once by the
    /// background discovery task; `None` until then, or when no JDK source is
    /// found. Interior mutability because the `Arc<Index>` is already shared.
    jdk: OnceLock<jdk::JdkIndex>,
    /// Project config (currently `:lint-as`), resolved at startup and reloaded
    /// when a watched config file changes. Interior mutability because the
    /// `Arc<Index>` is shared with handlers.
    extract_config: RwLock<ExtractConfig>,
}

/// The dialect rule of [`Index::insert_file`] and [`Index::insert_lib_file`]
/// for one primary/shadow map pair. `rank_of` ranks an entry already in a
/// slot (see [`slot_rank`]). Incoming rank at or below the old one takes the
/// primary slot; a `.cljs` copy that loses to, or is displaced by, a
/// non-`.cljs` one lands in `shadow`, unless the shadow already holds a
/// lower rank (a library `.cljs` never evicts a project one); any other loser
/// is dropped.
fn rank_insert<V>(
    primary: &DashMap<String, V>,
    shadow: &DashMap<String, V>,
    key: String,
    value: V,
    rank: u8,
    rank_of: impl Fn(&V) -> u8,
) {
    use dashmap::mapref::entry::Entry;

    match primary.entry(key.clone()) {
        Entry::Vacant(e) => {
            e.insert(value);
        }
        Entry::Occupied(mut e) => {
            let old = rank_of(e.get());
            if rank <= old {
                let displaced = e.insert(value);
                if is_cljs_rank(old) && !is_cljs_rank(rank) {
                    shadow_insert(shadow, key, displaced, old, &rank_of);
                }
            } else if is_cljs_rank(rank) {
                shadow_insert(shadow, key, value, rank, &rank_of);
            }
        }
    }
}

/// Puts a `.cljs` entry of `rank` in the shadow slot when that is vacant or
/// holds a rank not lower than its own.
fn shadow_insert<V>(
    shadow: &DashMap<String, V>,
    key: String,
    value: V,
    rank: u8,
    rank_of: &impl Fn(&V) -> u8,
) {
    use dashmap::mapref::entry::Entry;

    match shadow.entry(key) {
        Entry::Vacant(e) => {
            e.insert(value);
        }
        Entry::Occupied(mut e) => {
            if rank <= rank_of(e.get()) {
                e.insert(value);
            }
        }
    }
}

/// Removes `path`'s entry for `key` from a primary/shadow map pair. A primary
/// entry of `path` is replaced by the shadow one when there is one, so the
/// `.cljs` half of a namespace answers for every asker once its Clojure half
/// is gone (as a `.cljs`-only library already does); otherwise a shadow entry
/// of `path` is dropped. Entries of other files are untouched. The primary
/// entry lock is taken first, as in [`rank_insert`].
fn remove_slot<V>(
    primary: &DashMap<String, V>,
    shadow: &DashMap<String, V>,
    key: &str,
    path: &Path,
    file_of: impl Fn(&V) -> &Path,
) {
    use dashmap::mapref::entry::Entry;

    match primary.entry(key.to_string()) {
        Entry::Occupied(mut e) if file_of(e.get()) == path => match shadow.remove(key) {
            Some((_, promoted)) => {
                e.insert(promoted);
            }
            None => {
                e.remove();
            }
        },
        _ => {
            shadow.remove_if(key, |_, v| file_of(v) == path);
        }
    }
}

fn is_project(sym: &Symbol) -> bool {
    sym.source == SymbolSource::Project
}

impl Default for Index {
    fn default() -> Self {
        Self {
            symbols: DashMap::new(),
            namespaces: DashMap::new(),
            ns_symbols: DashMap::new(),
            cljs_symbols: DashMap::new(),
            cljs_namespaces: DashMap::new(),
            files: DashMap::new(),
            ns_files: DashMap::new(),
            file_to_ns: DashMap::new(),
            occurrences: DashMap::new(),
            keyword_counts: DashMap::new(),
            core_symbols: Vec::new(),
            letgo_core: AtomicBool::new(false),
            letgo_native: RwLock::new(Vec::new()),
            jdk: OnceLock::new(),
            extract_config: RwLock::new(ExtractConfig::default()),
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

    /// [`Index::lookup`], but a `.cljs` requester gets the ClojureScript copy
    /// when one exists — unless the primary copy is a project symbol and the
    /// ClojureScript one a library's: project code wins in either dialect. A
    /// namespace that ships only `.clj` still resolves for a `.cljs` file
    /// through the primary.
    pub fn lookup_for(&self, fqn: &str, dialect: Dialect) -> Option<Symbol> {
        let primary = self.lookup(fqn);
        if dialect == Dialect::Clj {
            return primary;
        }
        let Some(shadow) = self.cljs_symbols.get(fqn).map(|r| r.clone()) else {
            return primary;
        };
        match primary {
            Some(p) if is_project(&p) && !is_project(&shadow) => Some(p),
            _ => Some(shadow),
        }
    }

    /// Swaps an already-resolved symbol for its ClojureScript copy when
    /// `dialect` is `Cljs` and one exists, by the rule of
    /// [`Index::lookup_for`]; `Clj` passes through untouched.
    pub fn prefer_dialect(&self, sym: Symbol, dialect: Dialect) -> Symbol {
        if dialect == Dialect::Clj {
            return sym;
        }
        match self.cljs_symbols.get(&sym.fqn) {
            Some(shadow) if !is_project(&sym) || is_project(&shadow) => shadow.clone(),
            _ => sym,
        }
    }

    /// Every definition of `fqn`, in both dialects: the primary copy, then the
    /// ClojureScript one when it lives in another file. References and rename
    /// read this, since a namespace split across `.clj` and `.cljs` is one
    /// logical var defined twice.
    pub fn lookup_all(&self, fqn: &str) -> Vec<Symbol> {
        let mut out: Vec<Symbol> = self.lookup(fqn).into_iter().collect();
        if let Some(shadow) = self.cljs_symbols.get(fqn) {
            if out.iter().all(|p| p.file != shadow.file) {
                out.push(shadow.clone());
            }
        }
        out
    }

    /// The JDK source index, once background discovery has installed it.
    pub fn jdk(&self) -> Option<&jdk::JdkIndex> {
        self.jdk.get()
    }

    /// Installs the JDK source index (called once by the background discovery
    /// task). A second call is ignored.
    pub fn set_jdk(&self, jdk_index: jdk::JdkIndex) {
        let _ = self.jdk.set(jdk_index);
    }

    /// A snapshot of the resolved project config (`:lint-as`). Cloned so callers
    /// hold no lock; the `lint_as` map is tiny, and hot-path indexing borrows a
    /// single snapshot across all files rather than cloning per file.
    pub fn extract_config(&self) -> ExtractConfig {
        self.extract_config.read().unwrap().clone()
    }

    /// Replaces the project config. Called at startup and again whenever a
    /// watched config file changes.
    pub fn set_extract_config(&self, cfg: ExtractConfig) {
        *self.extract_config.write().unwrap() = cfg;
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

    /// [`Index::ns_meta`], with the rule of [`Index::lookup_for`]: a `.cljs`
    /// requester gets the ClojureScript copy unless the primary entry is a
    /// project namespace (one whose file has an occurrences entry) and the
    /// ClojureScript one is not.
    pub fn ns_meta_for(&self, ns: &str, dialect: Dialect) -> Option<NsMeta> {
        let primary = self.ns_meta(ns);
        if dialect == Dialect::Clj {
            return primary;
        }
        let Some(shadow) = self.cljs_namespaces.get(ns).map(|r| r.clone()) else {
            return primary;
        };
        match primary {
            Some(p) if self.is_project_path(&p.file) && !self.is_project_path(&shadow.file) => {
                Some(p)
            }
            _ => Some(shadow),
        }
    }

    fn meta_rank(&self, meta: &NsMeta) -> u8 {
        slot_rank(self.is_project_path(&meta.file), &meta.file)
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

    /// Records the let-go native names harvested from `lang.go`. Sorted and
    /// de-duplicated for binary search; an empty input clears back to the
    /// static fallback.
    pub fn set_letgo_native(&self, mut names: Vec<String>) {
        names.sort();
        names.dedup();
        *self.letgo_native.write().unwrap() = names;
    }

    /// The harvested let-go native names, or `None` when none were harvested
    /// (no source on disk) — callers then fall back to the static list.
    pub fn letgo_native_names(&self) -> Option<Vec<String>> {
        let g = self.letgo_native.read().unwrap();
        (!g.is_empty()).then(|| g.clone())
    }

    /// Whether `name` is a harvested let-go native, or `None` when nothing was
    /// harvested (callers fall back to the static list).
    pub fn letgo_native_contains(&self, name: &str) -> Option<bool> {
        let g = self.letgo_native.read().unwrap();
        (!g.is_empty()).then(|| g.binary_search_by(|n| n.as_str().cmp(name)).is_ok())
    }

    /// Removes what `path` contributed, and nothing another file did: its
    /// entry in each symbol slot (a displaced `.cljs` copy moving up into the
    /// primary), its namespace metadata by the same rule, its occurrences. The
    /// namespace keeps its `ns_symbols` list, trimmed to the fqns still
    /// defined, for as long as another file of it remains; when its metadata
    /// slot empties, another project file of the namespace refills it from
    /// its [`FileRecord`].
    pub fn remove_file(&self, path: &Path) {
        if let Some((_, occs)) = self.occurrences.remove(path) {
            self.sub_keyword_counts(&occs);
        }
        let record = self.files.remove(path).map(|(_, r)| r);
        if let Some(r) = &record {
            self.ns_files.remove_if_mut(&r.meta.name, |_, paths| {
                paths.retain(|p| p != path);
                paths.is_empty()
            });
        }
        let Some((_, ns_name)) = self.file_to_ns.remove(path) else {
            return;
        };
        if ns_name == EDN_NS_SENTINEL {
            return;
        }
        // A library file has no record (see `insert_lib_file`); its fqns are
        // among its namespace's.
        let fqns = match record {
            Some(r) => r.fqns,
            None => self
                .ns_symbols
                .get(&ns_name)
                .map(|r| r.clone())
                .unwrap_or_default(),
        };
        for fqn in &fqns {
            remove_slot(&self.symbols, &self.cljs_symbols, fqn, path, |s| {
                s.file.as_path()
            });
        }
        remove_slot(
            &self.namespaces,
            &self.cljs_namespaces,
            &ns_name,
            path,
            |m| m.file.as_path(),
        );
        if !self.namespaces.contains_key(&ns_name) {
            self.refill_namespace(&ns_name);
        }

        use dashmap::mapref::entry::Entry;
        if let Entry::Occupied(mut e) = self.ns_symbols.entry(ns_name.clone()) {
            e.get_mut().retain(|fqn| self.symbols.contains_key(fqn));
            if e.get().is_empty() && !self.namespaces.contains_key(&ns_name) {
                e.remove();
            }
        }
    }

    /// Re-inserts the metadata of every remaining project file of `ns` from
    /// its record, after the file holding the slot was removed.
    fn refill_namespace(&self, ns: &str) {
        let paths = self.ns_files.get(ns).map(|r| r.clone()).unwrap_or_default();
        let metas: Vec<NsMeta> = paths
            .iter()
            .filter_map(|p| self.files.get(p).map(|r| r.meta.clone()))
            .collect();
        for meta in metas {
            let rank = self.meta_rank(&meta);
            rank_insert(
                &self.namespaces,
                &self.cljs_namespaces,
                ns.to_string(),
                meta,
                rank,
                |old| self.meta_rank(old),
            );
        }
    }

    /// Adds `fqns` to `ns`'s list, de-duplicated: the files of one namespace
    /// share it, so completion's current-namespace pool sees every half.
    fn extend_ns_symbols(&self, ns: &str, fqns: &[String]) {
        let mut list = self.ns_symbols.entry(ns.to_string()).or_default();
        for fqn in fqns {
            if !list.contains(fqn) {
                list.push(fqn.clone());
            }
        }
    }

    /// Inserts a project source file. Every symbol and the namespace entry go
    /// through the rank rule of [`rank_insert`], so a namespace split across
    /// `.clj` and `.cljs` keeps both halves: the Clojure copy in the primary
    /// slot, the ClojureScript one in the shadow. A file already indexed is
    /// removed first.
    pub fn insert_file(&self, meta: NsMeta, symbols: Vec<Symbol>, occurrences: Vec<Occurrence>) {
        let ns_name = meta.name.clone();
        let file = meta.file.clone();
        if self.files.contains_key(&file) {
            self.remove_file(&file);
        }

        // First, so the file is a project path when its namespace is ranked.
        self.replace_occurrences(file.clone(), occurrences);

        let mut fqns: Vec<String> = Vec::with_capacity(symbols.len());
        for sym in symbols {
            if !fqns.contains(&sym.fqn) {
                fqns.push(sym.fqn.clone());
            }
            let rank = symbol_rank(&sym);
            rank_insert(
                &self.symbols,
                &self.cljs_symbols,
                sym.fqn.clone(),
                sym,
                rank,
                symbol_rank,
            );
        }

        self.extend_ns_symbols(&ns_name, &fqns);
        self.file_to_ns.insert(file.clone(), ns_name.clone());
        self.files.insert(
            file.clone(),
            FileRecord {
                meta: meta.clone(),
                fqns,
            },
        );
        self.ns_files
            .entry(ns_name.clone())
            .or_default()
            .push(file.clone());
        let rank = slot_rank(true, &file);
        rank_insert(
            &self.namespaces,
            &self.cljs_namespaces,
            ns_name,
            meta,
            rank,
            |old| self.meta_rank(old),
        );
    }

    /// Every project file in the index, as the entries that re-insert it: a
    /// source file's own metadata and the symbols it still holds a slot for,
    /// and each EDN config's occurrences. A same-dialect loser's shared fqn has
    /// no slot and is not reconstructed.
    pub fn file_entries(&self) -> Vec<FileEntry> {
        let mut out = Vec::with_capacity(self.files.len());
        for record in self.files.iter() {
            let file = record.key();
            let symbols = record
                .fqns
                .iter()
                .filter_map(|fqn| {
                    self.symbols
                        .get(fqn)
                        .filter(|s| &s.file == file)
                        .map(|s| s.clone())
                        .or_else(|| {
                            self.cljs_symbols
                                .get(fqn)
                                .filter(|s| &s.file == file)
                                .map(|s| s.clone())
                        })
                })
                .collect();
            out.push(FileEntry::Source {
                meta: Box::new(record.meta.clone()),
                symbols,
                occurrences: self
                    .occurrences
                    .get(file)
                    .map(|o| o.clone())
                    .unwrap_or_default(),
            });
        }
        for entry in self.file_to_ns.iter() {
            if entry.value() == EDN_NS_SENTINEL {
                out.push(FileEntry::Edn {
                    file: entry.key().clone(),
                    occurrences: self
                        .occurrences
                        .get(entry.key())
                        .map(|o| o.clone())
                        .unwrap_or_default(),
                });
            }
        }
        out
    }

    /// Inserts an EDN config file's keyword occurrences. EDN files contribute
    /// only occurrences — no namespace, no symbols — so this touches only
    /// `occurrences` and registers the file under [`EDN_NS_SENTINEL`] in
    /// `file_to_ns` (which keeps `merge_project_from` from dropping it). It
    /// deliberately leaves `namespaces`/`ns_symbols` untouched; `remove_file`
    /// no-ops cleanly on the absent sentinel ns.
    pub fn insert_edn_file(&self, file: PathBuf, occurrences: Vec<Occurrence>) {
        self.replace_occurrences(file.clone(), occurrences);
        self.file_to_ns.insert(file, EDN_NS_SENTINEL.to_string());
    }

    /// Sets `file`'s occurrences, keeping [`Index::keyword_counts`] in step:
    /// the vector being replaced is subtracted before the new one is added, so
    /// re-indexing a file whose keywords did not change leaves the counts
    /// exactly as they were. The single door onto `occurrences.insert`.
    fn replace_occurrences(&self, file: PathBuf, occurrences: Vec<Occurrence>) {
        self.add_keyword_counts(&occurrences);
        if let Some(old) = self.occurrences.insert(file, occurrences) {
            self.sub_keyword_counts(&old);
        }
    }

    fn add_keyword_counts(&self, occurrences: &[Occurrence]) {
        for occ in occurrences.iter().filter(|o| o.fqn.starts_with(':')) {
            *self.keyword_counts.entry(occ.fqn.clone()).or_insert(0) += 1;
        }
    }

    fn sub_keyword_counts(&self, occurrences: &[Occurrence]) {
        use dashmap::mapref::entry::Entry;

        for occ in occurrences.iter().filter(|o| o.fqn.starts_with(':')) {
            // Decrement and drop under one entry lock: a concurrent re-index of
            // another file may add the same keyword back, and a separate
            // `remove` would delete that live count.
            if let Entry::Occupied(mut e) = self.keyword_counts.entry(occ.fqn.clone()) {
                let count = e.get_mut();
                *count = count.saturating_sub(1);
                if *count == 0 {
                    e.remove();
                }
            }
        }
    }

    /// Every keyword the project uses, with its usage count — the pool keyword
    /// completion ranks. Snapshotted so callers hold no lock on the map.
    pub fn keyword_counts(&self) -> Vec<(String, u32)> {
        self.keyword_counts
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect()
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

    /// Whether `ns` is an indexed namespace that lives outside the project — a
    /// JAR entry or a dir-library source. Keyword rename refuses these: the
    /// keyword belongs to the library, and its sites are not ours to edit.
    /// An unknown namespace is not a library one; there is nothing to protect.
    pub fn is_library_namespace(&self, ns: &str) -> bool {
        self.namespaces
            .get(ns)
            .map(|meta| !self.is_project_path(&meta.file))
            .unwrap_or(false)
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

        // Each re-scanned file replaces its previous self, so a def removed
        // since the last scan (a file present in both scans but with fewer
        // symbols — e.g. after a `:lint-as` change) does not linger, and the
        // other half of a twin namespace is never touched.
        for entry in new_index.file_entries() {
            match entry {
                FileEntry::Source {
                    meta,
                    symbols,
                    occurrences,
                } => {
                    self.remove_file(&meta.file);
                    self.insert_file(*meta, symbols, occurrences);
                }
                FileEntry::Edn { file, occurrences } => {
                    self.remove_file(&file);
                    self.insert_edn_file(file, occurrences);
                }
            }
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
        // The harvested native list is likewise library-derived: drop it so a
        // project that stops being let-go (or bumps :lg-version) doesn't keep
        // serving stale names. Re-harvested by `index_letgo_core` on re-index.
        self.set_letgo_native(Vec::new());

        self.symbols
            .retain(|_, sym| sym.source == SymbolSource::Project);
        self.cljs_symbols
            .retain(|_, sym| sym.source == SymbolSource::Project);
        self.cljs_namespaces
            .retain(|_, meta| self.occurrences.contains_key(&meta.file));
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
    ///
    /// Between library files the primary slot is Clojure-preferred whatever
    /// the classpath order: `.clj` over `.cljc` over `.cljs` (see `slot_rank`),
    /// last writer among equals. A `.cljs` copy that loses to a Clojure one —
    /// displaced or arriving later — goes to `cljs_symbols` /
    /// `cljs_namespaces`, so a `.cljs` requester can still reach it; a `.cljc`
    /// that loses is dropped. `ns_symbols` is the union of the namespace's
    /// files. A library file gets no [`FileRecord`]: libraries leave through
    /// `clear_libs`, and a record would copy every library `NsMeta`.
    pub fn insert_lib_file(&self, meta: NsMeta, symbols: Vec<Symbol>) {
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

        let rank = slot_rank(false, &meta.file);
        let mut fqns = Vec::with_capacity(symbols.len());
        for sym in symbols {
            fqns.push(sym.fqn.clone());
            let fqn = sym.fqn.clone();
            rank_insert(
                &self.symbols,
                &self.cljs_symbols,
                fqn,
                sym,
                rank,
                symbol_rank,
            );
        }

        self.extend_ns_symbols(&meta.name, &fqns);
        self.file_to_ns.insert(meta.file.clone(), meta.name.clone());
        let ns_name = meta.name.clone();
        rank_insert(
            &self.namespaces,
            &self.cljs_namespaces,
            ns_name,
            meta,
            rank,
            |old| self.meta_rank(old),
        );
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

    #[test]
    fn merge_project_drops_symbols_removed_from_a_rescanned_file() {
        use std::path::PathBuf;
        let file = PathBuf::from("/p/src/a.clj");
        let mk = |name: &str| Symbol {
            name: name.to_string(),
            fqn: format!("a/{}", name),
            ns: "a".to_string(),
            kind: DefKind::Def,
            params: vec![],
            doc: None,
            file: file.clone(),
            source: SymbolSource::Project,
            range: Range::default(),
            name_range: Range::default(),
            private: false,
        };
        let meta = || NsMeta {
            name: "a".to_string(),
            file: file.clone(),
            aliases: HashMap::new(),
            refers: HashMap::new(),
            requires: vec![],
            imports: HashMap::new(),
            refer_all: vec![],
            as_aliases: vec![],
            core_excludes: vec![],
        };

        let index = Index::new();
        index.insert_file(meta(), vec![mk("keep"), mk("gone")], vec![]);
        assert!(index.lookup("a/gone").is_some());

        // A re-scan of the same file now defines only `keep` (e.g. a `:lint-as`
        // change stopped a macro from defining `gone`).
        let new_index = Index::new();
        new_index.insert_file(meta(), vec![mk("keep")], vec![]);
        index.merge_project_from(new_index, &std::collections::HashSet::new());

        assert!(index.lookup("a/keep").is_some(), "kept symbol must survive");
        assert!(
            index.lookup("a/gone").is_none(),
            "a symbol removed from a re-scanned file must be dropped"
        );
    }

    /// A library `Symbol` named `fqn`, defined in `file` inside `lib.jar`.
    fn lib_symbol(fqn: &str, file: &str) -> Symbol {
        let (ns, name) = fqn.split_once('/').unwrap();
        Symbol {
            name: name.to_string(),
            fqn: fqn.to_string(),
            ns: ns.to_string(),
            kind: DefKind::Defn,
            params: vec![],
            doc: None,
            file: PathBuf::from(file),
            source: SymbolSource::Jar(PathBuf::from("/m2/lib.jar")),
            range: Range::default(),
            name_range: Range::default(),
            private: false,
        }
    }

    fn ns_meta_in(ns: &str, file: &str) -> NsMeta {
        NsMeta {
            name: ns.to_string(),
            file: PathBuf::from(file),
            aliases: HashMap::new(),
            refers: HashMap::new(),
            requires: vec![],
            imports: HashMap::new(),
            refer_all: vec![],
            as_aliases: vec![],
            core_excludes: vec![],
        }
    }

    const CLJ: &str = "/m2/clojure.jar!/clojure/string.clj";
    const CLJS: &str = "/m2/clojurescript.jar!/clojure/string.cljs";
    const CLJC: &str = "/m2/lib.jar!/clojure/string.cljc";
    const TRIM: &str = "clojure.string/trim";
    const STRING_NS: &str = "clojure.string";

    fn insert_lib(index: &Index, file: &str) {
        index.insert_lib_file(ns_meta_in(STRING_NS, file), vec![lib_symbol(TRIM, file)]);
    }

    /// One index per insertion order of `files`.
    fn both_orders(files: [&str; 2]) -> [Index; 2] {
        let forward = Index::new();
        insert_lib(&forward, files[0]);
        insert_lib(&forward, files[1]);
        let reverse = Index::new();
        insert_lib(&reverse, files[1]);
        insert_lib(&reverse, files[0]);
        [forward, reverse]
    }

    fn file_of(sym: Option<Symbol>) -> String {
        sym.expect("symbol").file.display().to_string()
    }

    fn ns_file_of(meta: Option<NsMeta>) -> String {
        meta.expect("ns meta").file.display().to_string()
    }

    #[test]
    fn lib_insert_prefers_clj_over_cljs_in_either_order() {
        for index in both_orders([CLJS, CLJ]) {
            assert_eq!(file_of(index.lookup(TRIM)), CLJ);
            assert_eq!(ns_file_of(index.ns_meta(STRING_NS)), CLJ);
        }
    }

    #[test]
    fn lookup_for_cljs_returns_the_cljs_copy() {
        for index in both_orders([CLJS, CLJ]) {
            assert_eq!(file_of(index.lookup_for(TRIM, Dialect::Cljs)), CLJS);
            assert_eq!(
                ns_file_of(index.ns_meta_for(STRING_NS, Dialect::Cljs)),
                CLJS
            );
            assert_eq!(file_of(index.lookup_for(TRIM, Dialect::Clj)), CLJ);
            assert_eq!(ns_file_of(index.ns_meta_for(STRING_NS, Dialect::Clj)), CLJ);
        }
    }

    #[test]
    fn cljs_only_library_resolves_for_both_dialects() {
        let index = Index::new();
        insert_lib(&index, CLJS);
        assert_eq!(file_of(index.lookup_for(TRIM, Dialect::Clj)), CLJS);
        assert_eq!(file_of(index.lookup_for(TRIM, Dialect::Cljs)), CLJS);
        assert_eq!(ns_file_of(index.ns_meta_for(STRING_NS, Dialect::Clj)), CLJS);
        assert_eq!(
            ns_file_of(index.ns_meta_for(STRING_NS, Dialect::Cljs)),
            CLJS
        );
    }

    #[test]
    fn cljc_after_clj_is_dropped_and_before_clj_is_replaced() {
        for index in both_orders([CLJC, CLJ]) {
            assert_eq!(file_of(index.lookup(TRIM)), CLJ);
            assert_eq!(ns_file_of(index.ns_meta(STRING_NS)), CLJ);
            // A `.cljc` is not a ClojureScript copy: nothing lands in the shadow.
            assert_eq!(file_of(index.lookup_for(TRIM, Dialect::Cljs)), CLJ);
        }
        // `.cljc` still beats `.cljs` and is the answer for both dialects when
        // no `.clj` exists.
        for index in both_orders([CLJC, CLJS]) {
            assert_eq!(file_of(index.lookup(TRIM)), CLJC);
            assert_eq!(file_of(index.lookup_for(TRIM, Dialect::Cljs)), CLJS);
        }
    }

    #[test]
    fn project_symbol_wins_over_every_dialect() {
        let project = "/p/src/clojure/string.clj";
        let index = Index::new();
        let mut sym = lib_symbol(TRIM, project);
        sym.source = SymbolSource::Project;
        index.insert_file(ns_meta_in(STRING_NS, project), vec![sym], vec![]);
        insert_lib(&index, CLJ);
        insert_lib(&index, CLJS);
        assert_eq!(file_of(index.lookup(TRIM)), project);
        assert_eq!(file_of(index.lookup_for(TRIM, Dialect::Cljs)), project);
        assert_eq!(
            ns_file_of(index.ns_meta_for(STRING_NS, Dialect::Cljs)),
            project
        );
    }

    #[test]
    fn project_inserted_after_both_library_copies_still_wins() {
        let project = "/p/src/clojure/string.clj";
        let index = Index::new();
        insert_lib(&index, CLJ);
        insert_lib(&index, CLJS);
        let mut sym = lib_symbol(TRIM, project);
        sym.source = SymbolSource::Project;
        index.insert_file(ns_meta_in(STRING_NS, project), vec![sym.clone()], vec![]);
        assert_eq!(file_of(index.lookup_for(TRIM, Dialect::Cljs)), project);
        assert_eq!(
            ns_file_of(index.ns_meta_for(STRING_NS, Dialect::Cljs)),
            project
        );
        assert_eq!(
            index.prefer_dialect(sym.clone(), Dialect::Cljs).file,
            PathBuf::from(project)
        );
    }

    #[test]
    fn prefer_dialect_swaps_only_library_symbols() {
        let index = Index::new();
        insert_lib(&index, CLJ);
        insert_lib(&index, CLJS);
        let clj = index.lookup(TRIM).unwrap();
        assert_eq!(
            index.prefer_dialect(clj.clone(), Dialect::Cljs).file,
            PathBuf::from(CLJS)
        );
        assert_eq!(
            index.prefer_dialect(clj.clone(), Dialect::Clj).file,
            PathBuf::from(CLJ)
        );
        // A symbol with no ClojureScript copy passes through.
        let other = lib_symbol("clojure.string/join", CLJ);
        assert_eq!(
            index.prefer_dialect(other.clone(), Dialect::Cljs).file,
            PathBuf::from(CLJ)
        );
        // A project symbol is never swapped, even when a shadow entry exists.
        let mut project = clj.clone();
        project.source = SymbolSource::Project;
        project.file = PathBuf::from("/p/src/clojure/string.clj");
        assert_eq!(
            index.prefer_dialect(project.clone(), Dialect::Cljs).file,
            project.file
        );
    }

    #[test]
    fn clear_libs_drops_the_cljs_shadow() {
        let index = Index::new();
        insert_lib(&index, CLJ);
        insert_lib(&index, CLJS);
        assert!(index.lookup_for(TRIM, Dialect::Cljs).is_some());
        index.clear_libs();
        assert!(index.lookup_for(TRIM, Dialect::Cljs).is_none());
        assert!(index.ns_meta_for(STRING_NS, Dialect::Cljs).is_none());
    }

    /// A project `Symbol` named `fqn`, defined in `file`.
    fn project_symbol(fqn: &str, file: &str) -> Symbol {
        let mut sym = lib_symbol(fqn, file);
        sym.source = SymbolSource::Project;
        sym
    }

    fn project_meta(ns: &str, file: &str) -> NsMeta {
        ns_meta_in(ns, file)
    }

    /// One occurrence, so the file is a project path whatever it defines.
    fn some_occurrence() -> Vec<Occurrence> {
        vec![Occurrence {
            fqn: "clojure.core/inc".to_string(),
            name_range: Range::default(),
        }]
    }

    /// Inserts project `file` of namespace `ns` defining `names`.
    fn insert_project(index: &Index, ns: &str, file: &str, names: &[&str]) {
        let symbols = names
            .iter()
            .map(|n| project_symbol(&format!("{ns}/{n}"), file))
            .collect();
        index.insert_file(project_meta(ns, file), symbols, some_occurrence());
    }

    const TWIN_CLJ: &str = "/p/src/a/b.clj";
    const TWIN_CLJS: &str = "/p/src/a/b.cljs";

    fn insert_twin_clj(index: &Index) {
        insert_project(index, "a.b", TWIN_CLJ, &["foo", "clj-only"]);
    }

    fn insert_twin_cljs(index: &Index) {
        insert_project(index, "a.b", TWIN_CLJS, &["foo", "cljs-only"]);
    }

    fn sorted_ns_symbols(index: &Index, ns: &str) -> Vec<String> {
        let mut fqns = index.ns_symbols.get(ns).expect("ns symbols").clone();
        fqns.sort();
        fqns
    }

    #[test]
    fn project_twins_keep_both_definitions() {
        let forward = Index::new();
        insert_twin_clj(&forward);
        insert_twin_cljs(&forward);
        let reverse = Index::new();
        insert_twin_cljs(&reverse);
        insert_twin_clj(&reverse);
        for index in [forward, reverse] {
            assert_eq!(file_of(index.lookup("a.b/foo")), TWIN_CLJ);
            assert_eq!(
                file_of(index.lookup_for("a.b/foo", Dialect::Cljs)),
                TWIN_CLJS
            );
            assert_eq!(
                file_of(index.lookup_for("a.b/cljs-only", Dialect::Clj)),
                TWIN_CLJS
            );
            let files: Vec<PathBuf> = index
                .lookup_all("a.b/foo")
                .into_iter()
                .map(|s| s.file)
                .collect();
            assert_eq!(
                files,
                vec![PathBuf::from(TWIN_CLJ), PathBuf::from(TWIN_CLJS)]
            );
            assert_eq!(ns_file_of(index.ns_meta("a.b")), TWIN_CLJ);
            assert_eq!(
                ns_file_of(index.ns_meta_for("a.b", Dialect::Cljs)),
                TWIN_CLJS
            );
            assert_eq!(
                sorted_ns_symbols(&index, "a.b"),
                vec!["a.b/clj-only", "a.b/cljs-only", "a.b/foo"]
            );
        }
    }

    #[test]
    fn removing_one_twin_keeps_the_other() {
        let index = Index::new();
        insert_twin_clj(&index);
        insert_twin_cljs(&index);

        index.remove_file(Path::new(TWIN_CLJ));
        assert_eq!(file_of(index.lookup("a.b/foo")), TWIN_CLJS, "promoted");
        assert!(index.lookup("a.b/clj-only").is_none());
        assert_eq!(ns_file_of(index.ns_meta("a.b")), TWIN_CLJS);
        assert_eq!(
            sorted_ns_symbols(&index, "a.b"),
            vec!["a.b/cljs-only", "a.b/foo"]
        );

        insert_twin_clj(&index);
        assert_eq!(file_of(index.lookup("a.b/foo")), TWIN_CLJ);
        assert_eq!(
            file_of(index.lookup_for("a.b/foo", Dialect::Cljs)),
            TWIN_CLJS
        );
        assert_eq!(
            ns_file_of(index.ns_meta_for("a.b", Dialect::Cljs)),
            TWIN_CLJS
        );

        index.remove_file(Path::new(TWIN_CLJS));
        assert_eq!(
            file_of(index.lookup_for("a.b/foo", Dialect::Cljs)),
            TWIN_CLJ
        );
        assert!(index.lookup_for("a.b/cljs-only", Dialect::Cljs).is_none());
        assert_eq!(
            ns_file_of(index.ns_meta_for("a.b", Dialect::Cljs)),
            TWIN_CLJ
        );
        assert_eq!(
            sorted_ns_symbols(&index, "a.b"),
            vec!["a.b/clj-only", "a.b/foo"]
        );

        // The last file of the namespace takes the namespace with it.
        index.remove_file(Path::new(TWIN_CLJ));
        assert!(index.ns_meta("a.b").is_none());
        assert!(index.ns_symbols.get("a.b").is_none());
    }

    #[test]
    fn project_cljs_twin_beats_a_library_in_both_slots() {
        const LIB_CLJ: &str = "/m2/lib.jar!/a/b.clj";
        const LIB_CLJS: &str = "/m2/lib.jar!/a/b.cljs";
        let index = Index::new();
        index.insert_lib_file(
            ns_meta_in("a.b", LIB_CLJ),
            vec![lib_symbol("a.b/foo", LIB_CLJ)],
        );
        index.insert_lib_file(
            ns_meta_in("a.b", LIB_CLJS),
            vec![lib_symbol("a.b/foo", LIB_CLJS)],
        );

        insert_project(&index, "a.b", TWIN_CLJS, &["foo"]);
        assert_eq!(file_of(index.lookup("a.b/foo")), TWIN_CLJS);
        assert_eq!(
            file_of(index.lookup_for("a.b/foo", Dialect::Cljs)),
            TWIN_CLJS
        );
        assert_eq!(ns_file_of(index.ns_meta("a.b")), TWIN_CLJS);
        assert_eq!(
            ns_file_of(index.ns_meta_for("a.b", Dialect::Cljs)),
            TWIN_CLJS
        );

        insert_project(&index, "a.b", TWIN_CLJ, &["foo"]);
        assert_eq!(file_of(index.lookup("a.b/foo")), TWIN_CLJ);
        assert_eq!(
            file_of(index.lookup_for("a.b/foo", Dialect::Cljs)),
            TWIN_CLJS
        );

        // A library `.cljs` inserted last reaches neither slot.
        index.insert_lib_file(
            ns_meta_in("a.b", LIB_CLJS),
            vec![lib_symbol("a.b/foo", LIB_CLJS)],
        );
        assert_eq!(file_of(index.lookup("a.b/foo")), TWIN_CLJ);
        assert_eq!(
            file_of(index.lookup_for("a.b/foo", Dialect::Cljs)),
            TWIN_CLJS
        );
        assert_eq!(
            ns_file_of(index.ns_meta_for("a.b", Dialect::Cljs)),
            TWIN_CLJS
        );
    }

    #[test]
    fn merge_project_from_replaces_each_file_and_keeps_twins() {
        let index = Index::new();
        insert_twin_clj(&index);
        insert_twin_cljs(&index);

        let new_index = Index::new();
        insert_project(&new_index, "a.b", TWIN_CLJ, &["foo"]);
        insert_twin_cljs(&new_index);
        index.merge_project_from(new_index, &std::collections::HashSet::new());

        assert!(index.lookup("a.b/clj-only").is_none(), "dropped def");
        assert_eq!(
            file_of(index.lookup_for("a.b/cljs-only", Dialect::Cljs)),
            TWIN_CLJS
        );
        assert_eq!(file_of(index.lookup("a.b/foo")), TWIN_CLJ);
        assert_eq!(
            file_of(index.lookup_for("a.b/foo", Dialect::Cljs)),
            TWIN_CLJS
        );
        assert!(index.occurrences.contains_key(Path::new(TWIN_CLJ)));
        assert!(index.occurrences.contains_key(Path::new(TWIN_CLJS)));
        assert_eq!(
            sorted_ns_symbols(&index, "a.b"),
            vec!["a.b/cljs-only", "a.b/foo"]
        );
    }

    #[test]
    fn clear_libs_keeps_project_entries_in_the_shadow() {
        const LIB_CLJS: &str = "/m2/lib.jar!/a/b.cljs";
        let index = Index::new();
        index.insert_lib_file(
            ns_meta_in("a.b", LIB_CLJS),
            vec![
                lib_symbol("a.b/foo", LIB_CLJS),
                lib_symbol("a.b/lib-only", LIB_CLJS),
            ],
        );
        insert_twin_clj(&index);
        insert_twin_cljs(&index);
        assert!(index.lookup("a.b/lib-only").is_some());

        index.clear_libs();
        assert_eq!(
            file_of(index.lookup_for("a.b/foo", Dialect::Cljs)),
            TWIN_CLJS
        );
        assert_eq!(
            ns_file_of(index.ns_meta_for("a.b", Dialect::Cljs)),
            TWIN_CLJS
        );
        assert!(index.lookup_for("a.b/lib-only", Dialect::Cljs).is_none());
    }

    #[test]
    fn file_entries_round_trip() {
        let index = Index::new();
        insert_twin_clj(&index);
        insert_twin_cljs(&index);
        let edn = PathBuf::from("/p/resources/config.edn");
        let edn_occs = vec![Occurrence {
            fqn: ":a.b/db".to_string(),
            name_range: Range::default(),
        }];
        index.insert_edn_file(edn.clone(), edn_occs.clone());

        let mut sources = Vec::new();
        let mut edns = Vec::new();
        for entry in index.file_entries() {
            match entry {
                FileEntry::Source {
                    meta,
                    symbols,
                    occurrences,
                } => sources.push((meta, symbols, occurrences)),
                FileEntry::Edn { file, occurrences } => edns.push((file, occurrences)),
            }
        }
        sources.sort_by(|a, b| a.0.file.cmp(&b.0.file));
        assert_eq!(sources.len(), 2);
        for ((meta, symbols, occurrences), (file, names)) in sources.iter().zip([
            (TWIN_CLJ, ["foo", "clj-only"]),
            (TWIN_CLJS, ["foo", "cljs-only"]),
        ]) {
            assert_eq!(**meta, project_meta("a.b", file));
            let expected: Vec<Symbol> = names
                .iter()
                .map(|n| project_symbol(&format!("a.b/{n}"), file))
                .collect();
            assert_eq!(*symbols, expected);
            assert_eq!(*occurrences, some_occurrence());
        }
        assert_eq!(edns, vec![(edn, edn_occs)]);
    }

    #[test]
    fn same_dialect_collision_keeps_the_losers_record() {
        const A: &str = "/p/a/dev/user.clj";
        const B: &str = "/p/b/dev/user.clj";
        let index = Index::new();
        // `b` first: `a`, inserted last, takes both shared slots.
        insert_project(&index, "user", B, &["shared", "b-only"]);
        insert_project(&index, "user", A, &["shared"]);
        assert_eq!(file_of(index.lookup("user/shared")), A);
        assert_eq!(ns_file_of(index.ns_meta("user")), A);

        let entries = index.file_entries();
        let loser = entries
            .iter()
            .find_map(|e| match e {
                FileEntry::Source { meta, symbols, .. } if meta.file == Path::new(B) => {
                    Some((meta, symbols))
                }
                _ => None,
            })
            .expect("the loser's entry");
        assert_eq!(**loser.0, project_meta("user", B));
        let names: Vec<&str> = loser.1.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b-only"]);

        index.remove_file(Path::new(A));
        assert_eq!(file_of(index.lookup("user/b-only")), B);
        assert_eq!(ns_file_of(index.ns_meta("user")), B);
    }

    #[test]
    fn dialect_of_path() {
        use std::path::Path;
        assert_eq!(Dialect::of_path(Path::new("/p/src/a.cljs")), Dialect::Cljs);
        assert_eq!(Dialect::of_path(Path::new("/p/src/a.clj")), Dialect::Clj);
        assert_eq!(Dialect::of_path(Path::new("/p/src/a.cljc")), Dialect::Clj);
        assert_eq!(Dialect::of_path(Path::new("/p/src/a.lg")), Dialect::Clj);
        assert_eq!(
            Dialect::of_path(Path::new("/p/resources/config.edn")),
            Dialect::Clj
        );
        assert_eq!(
            Dialect::of_path(Path::new("/m2/x.jar!/clojure/string.cljs")),
            Dialect::Cljs
        );
        assert_eq!(
            Dialect::of_path(Path::new("/m2/x.jar!/clojure/string.clj")),
            Dialect::Clj
        );
    }
}
