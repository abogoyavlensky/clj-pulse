# fast-clj-lsp — Implementation Plan

> **For AI agents**: Follow steps in order. Each step has explicit files to create/modify,
> exact code contracts, and a DONE criteria. Do not proceed to the next step until all
> tests in the current step pass. Never panic — use `anyhow::Result` everywhere.

---

## Project Goal

A minimal, fast Clojure LSP server in Rust. V1 scope:
- Jump to definition (project source only)
- Autocomplete (project symbols + clojure.core builtins)
- Hover / documentation

**Non-goals for V1**: dependency JARs, index cache, diagnostics, rename, references, nREPL.

---

## Final Project Structure

```
fast-clj-lsp/
├── Cargo.toml
├── ARCHITECTURE.md          ← data flow reference for agents
├── build.rs                 ← optional: codegen for core symbols
├── src/
│   ├── main.rs              ← stdio transport + tower-lsp init
│   ├── server.rs            ← LanguageServer trait impl (thin, delegates to handlers)
│   ├── config.rs            ← find project root, parse :paths from deps.edn
│   ├── document.rs          ← open file store (ropey ropes + incremental edits)
│   ├── index/
│   │   ├── mod.rs           ← Index struct + public API
│   │   ├── scanner.rs       ← parallel file walk → extractor → populate index
│   │   ├── extractor.rs     ← tree-sitter: &str → (NsMeta, Vec<Symbol>)
│   │   └── core.rs          ← pre-baked clojure.core symbols (~800 entries)
│   └── handlers/
│       ├── definition.rs    ← textDocument/definition logic
│       ├── completion.rs    ← textDocument/completion logic
│       └── hover.rs         ← textDocument/hover logic
└── tests/
    ├── fixtures/
    │   ├── simple_project/
    │   │   ├── deps.edn
    │   │   └── src/
    │   │       ├── core.clj
    │   │       └── utils.clj
    │   └── snippets/
    │       ├── basic_defn.clj
    │       ├── multi_arity.clj
    │       ├── reader_conditional.cljc
    │       └── ns_with_requires.clj
    ├── test_extractor.rs
    ├── test_index.rs
    ├── test_definition.rs
    ├── test_completion.rs
    └── test_hover.rs
```

---

## Cargo.toml

```toml
[package]
name = "fast-clj-lsp"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "fast-clj-lsp"
path = "src/main.rs"

[dependencies]
# LSP
tower-lsp = "0.20"
tokio = { version = "1", features = ["full"] }

# Parsing — tree-sitter-clojure 0.1 requires tree-sitter ^0.25, NOT 0.26+
tree-sitter = "0.25"
tree-sitter-clojure = "0.1"
tree-sitter-language = "0.1"   # needed for tree-sitter 0.25+ language() binding

# Concurrency / data structures
dashmap = "5"
rayon = "1"

# File system
ignore = "0.4"   # respects .gitignore, from ripgrep
ropey = "1.6"    # rope for open file text buffers
dirs = "5"       # platform cache/config dirs (used for log file path)

# Utilities
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender = "0.2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
tokio-test = "0.4"
```

> **Agent note**: Run `cargo build` after creating this file before proceeding. Fix any
> version resolution errors by checking crates.io for latest compatible versions.

---

## Core Data Types (establish these first, never change shapes without updating all dependents)

These live in `src/index/mod.rs`. All other modules import from here.

```rust
use std::path::PathBuf;
use tower_lsp::lsp_types::Range;

#[derive(Debug, Clone, PartialEq)]
pub enum DefKind {
    Def,
    Defn,
    DefnPrivate,
    Defmacro,
    Defmulti,
    Defmethod,
    Defprotocol,
    Defrecord,
    Deftype,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,       // unqualified: "hello"
    pub fqn: String,        // fully qualified: "my.core/hello"
    pub ns: String,         // "my.core"
    pub kind: DefKind,
    pub params: Vec<String>, // each arity as a string: ["[x y]", "[x y z]"]
    pub doc: Option<String>,
    pub file: PathBuf,
    pub range: Range,       // full def form range (for re-indexing)
    pub name_range: Range,  // just the name token (jump-to-def lands here)
}

#[derive(Debug, Clone)]
pub struct NsMeta {
    pub name: String,
    pub file: PathBuf,
    pub aliases: std::collections::HashMap<String, String>, // "str" → "clojure.string"
    pub refers: std::collections::HashMap<String, String>,  // "join" → "clojure.string/join"
}

#[derive(Debug, Clone)]
pub struct CoreSymbol {
    pub name: String,
    pub params: String,  // raw string: "([f coll] [f c1 c2])"
    pub doc: String,
}
```

---

## ARCHITECTURE.md Contents

Create this file so agents understand the data flow:

```markdown
# Architecture

## Data Flow

editor keystroke
  → tower-lsp (server.rs) receives LSP request
  → delegates to handlers/ function with (&Index, &DocumentStore, params)
  → handler queries Index via public API only
  → returns LSP response type

## Index Population (startup)

config.rs: find project root (walk up dirs for deps.edn / project.clj)
config.rs: read :paths from deps.edn (default ["src" "test"])
scanner.rs: walk each path with `ignore::WalkBuilder`, collect .clj/.cljs/.cljc files
scanner.rs: rayon::par_iter → call extractor::extract(file_contents) per file
extractor.rs: tree-sitter parse → return (NsMeta, Vec<Symbol>)
scanner.rs: merge results into Arc<Index> via DashMap inserts

## Index Re-population (on file save)

server.rs didSave handler
  → index.remove_file(path)    ← removes all symbols from that file
  → extractor::extract(new_contents)
  → index.insert_file(ns_meta, symbols)

## Symbol Resolution (for definition + hover)

word under cursor (from DocumentStore / ropey)
  → if contains "/": split into (alias, name)
      → look up alias in current file's NsMeta.aliases → get full_ns
      → look up full_ns/name in Index.symbols
  → if no "/": 
      → check NsMeta.refers first
      → check current namespace symbols
      → check clojure.core builtins
      → return first match

## Key Invariants

- handlers/ never access DashMap directly — only Index public API methods
- extractor::extract() is pure: (source: &str) → Result<(NsMeta, Vec<Symbol>)>, no IO
- server.rs handlers are max ~15 lines each, all logic in handlers/
- All LSP Position/Range values come from ropey, never manual byte arithmetic
- On any parse failure: log warning, return Ok(empty) — never crash
```

---

## Step 1 — Scaffold + LSP Handshake

**Goal**: Server starts, editor connects, capabilities negotiated. No real functionality yet.

### Files to create:

**`src/main.rs`**
```rust
use tower_lsp::{LspService, Server};
mod server;
use server::Backend;

#[tokio::main]
async fn main() {
    // Log to file — LSP servers must not write to stdout
    let log_dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("fast-clj-lsp");
    std::fs::create_dir_all(&log_dir).ok();
    let file_appender = tracing_appender::rolling::daily(log_dir, "server.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt().with_writer(non_blocking).init();

    tracing::info!("fast-clj-lsp starting");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend::new(client));
    Server::new(stdin, stdout, socket).serve(service).await;
}
```

**`src/server.rs`**
- Implement `LanguageServer` trait for `Backend` struct
- `Backend` holds: `client: Client`, `index: Arc<Index>`, `documents: DocumentStore`
- **Note**: `Index` uses `DashMap` internally which provides interior mutability — no `RwLock` wrapper needed. Same for `DocumentStore` (also backed by `DashMap`).
- `initialize`: return `ServerCapabilities` with:
  - `text_document_sync`: `TextDocumentSyncKind::INCREMENTAL`
  - `completion_provider`: `Some(CompletionOptions::default())`
  - `hover_provider`: `Some(HoverProviderCapability::Simple(true))`
  - `definition_provider`: `Some(OneOf::Left(true))`
- `did_open`: call `documents.open(uri, text)` to populate DocumentStore
- `did_close`: call `documents.close(uri)` to free memory
- `did_change`: call `documents.apply_changes(uri, changes)` to keep rope in sync with editor
- All other handlers: stub returning `Ok(None)` or `Ok(vec![])`
- `initialized`: spawn background task to build index (just log "building index" for now)

**`src/index/mod.rs`**
- Define all data types from the Core Data Types section above
- Define `Index` struct with all DashMap fields
- Implement `Index::new() -> Self`
- Public API stubs (return empty/None, implement later):
  - `fn lookup(&self, fqn: &str) -> Option<Symbol>`
  - `fn complete(&self, prefix: &str, ns: &str) -> Vec<Symbol>`
  - `fn ns_meta(&self, ns: &str) -> Option<NsMeta>`
  - `fn remove_file(&self, path: &Path)`
  - `fn insert_file(&self, meta: NsMeta, symbols: Vec<Symbol>)`
  - `fn file_ns(&self, path: &Path) -> Option<String>`

**`src/document.rs`**
- `DocumentStore` struct with `DashMap<Url, ropey::Rope>`
- `fn open(&self, uri: Url, text: String)`
- `fn close(&self, uri: &Url)`
- `fn apply_changes(&self, uri: &Url, changes: Vec<TextDocumentContentChangeEvent>) -> Result<()>`
- `fn word_at(&self, uri: &Url, pos: Position) -> Option<String>` — extract identifier under cursor
- `fn line_text(&self, uri: &Url, line: u32) -> Option<String>`

**`src/config.rs`**
- `fn find_project_root(start: &Path) -> Option<PathBuf>` — walk up dirs, look for `deps.edn` or `project.clj`
- `fn source_paths(root: &Path) -> Vec<PathBuf>` — parse deps.edn `:paths`, default to `["src" "test"]`
- For now: parse `:paths` using simple string search (full EDN parse in Step 2)

### DONE when:
```
$ cargo build   # no errors
$ cargo run     # starts without crash
# In editor: connect to server, see "fast-clj-lsp" in LSP status, no errors in log
```

---

## Step 2 — Tree-sitter Extractor

**Goal**: Given Clojure source as `&str`, extract all symbols and namespace metadata. Pure function, fully tested before wiring into the rest.

### Files to create:

**`tests/fixtures/snippets/basic_defn.clj`**
```clojure
(ns my.core)

(def PI 3.14159)

(defn hello
  "Says hello to someone."
  [name]
  (str "Hello, " name))

(defn- private-thing [x] x)

(defmacro when-pos [n & body]
  `(when (pos? ~n) ~@body))
```

**`tests/fixtures/snippets/multi_arity.clj`**
```clojure
(ns my.arity)

(defn greet
  "Greets with optional title."
  ([name] (greet nil name))
  ([title name] (str title " " name)))
```

**`tests/fixtures/snippets/ns_with_requires.clj`**
```clojure
(ns my.service
  (:require [clojure.string :as str]
            [my.core :as core]
            [my.utils :refer [format-date parse-id]]))

(defn process [input]
  (str/upper-case input))
```

**`tests/fixtures/snippets/reader_conditional.cljc`**
```clojure
(ns my.platform)

#?(:clj  (defn read-file [path] (slurp path))
   :cljs (defn read-file [path] (js/fetch path)))

(defn shared-fn [x] (* x 2))
```

**`src/index/extractor.rs`**

Public API (this is the contract — implement to satisfy it):
```rust
pub fn extract(source: &str, file: &Path) -> anyhow::Result<(NsMeta, Vec<Symbol>)>
```

Implementation requirements:
- Initialize tree-sitter parser with `tree-sitter-clojure` language once per call (later: pass in pre-built parser)
- Use tree-sitter queries — compile queries once as `static` using `std::sync::OnceLock`
- Extract `(ns ...)` form: get namespace name, parse `:require` for aliases and refers
- Extract all `def*` forms: name, kind (map first symbol to `DefKind`), docstring (next sibling string literal after name), params (all vector children for each arity)
- **Multi-arity detection**: multi-arity defns wrap each arity in a `list_lit`, not a top-level `vec_lit`:
  ```clojure
  (defn greet          ;; list_lit
    ([name] ...)       ;;   list_lit → contains vec_lit [name]
    ([title name] ...) ;;   list_lit → contains vec_lit [title name]
  )
  ```
  So: if the def form has NO direct `vec_lit` child but has `list_lit` children (after name/doc),
  treat each child `list_lit`'s first `vec_lit` as an arity's params.
- For `name_range`: use the range of the name node specifically, not the whole form
- Reader conditionals (`#?`): include both `:clj` and `:cljs` branches (collect all defs from both)
- On any node parse error: log warning, skip that form, continue

**Grammar node types reference** (tree-sitter-clojure uses `_lit` suffix convention):
- Lists `(...)` → `list_lit`
- Symbols → `sym_lit` (children: `sym_ns`, `sym_name`)
- Vectors `[...]` → `vec_lit`
- Strings `"..."` → `str_lit`
- Keywords `:foo` → `kwd_lit` (children: `kwd_ns`, `kwd_name`)
- Reader conditionals `#?(...)` → `read_cond_lit`
- There is **NO** dedicated `ns_form` node — `(ns ...)` is just a `list_lit` whose first child `sym_lit` has text `"ns"`

> **Agent note**: Before writing extractor queries, run a quick smoke test: parse a simple
> `(ns my.core) (defn foo [x] x)` string, dump the tree with `root.to_sexp()`, and verify
> actual node types match the above. If they don't, update queries accordingly.

Tree-sitter query strings to use (compile with `Query::new`):

```scheme
; NS query — no ns_form node, match list whose first symbol is "ns"
(list_lit . (sym_lit) @ns-kw . (sym_lit) @ns-name)
;; Then filter in code: only process when node_text(@ns-kw) == "ns"

; DEF query
(list_lit . (sym_lit) @def-kw . (sym_lit) @def-name)
;; Filter in code: only process when str_to_defkind(node_text(@def-kw)).is_some()

; DOCSTRING query (sibling string after name in def form)
(list_lit . (sym_lit) @_kw . (sym_lit) @_name . (str_lit) @doc)

; PARAMS query — top-level vec_lit for single-arity defs
(list_lit . (sym_lit) @_kw . (sym_lit) @_name (vec_lit) @params)
```

Helper: `fn node_text<'a>(node: Node, source: &'a str) -> &'a str` — extract text from a node using its byte range.

Helper: `fn node_to_lsp_range(node: Node) -> Range` — convert tree-sitter `Point` (row, col) to LSP `Range`.

Helper: `fn str_to_defkind(s: &str) -> Option<DefKind>` — map "defn" → `DefKind::Defn` etc. Return `None` for non-def symbols so they're skipped.

**`tests/test_extractor.rs`**

```rust
use fast_clj_lsp::index::extractor::extract;
use fast_clj_lsp::index::{DefKind};
use std::path::Path;

#[test]
fn extracts_namespace_name() {
    let (meta, _) = extract(include_str!("fixtures/snippets/basic_defn.clj"),
                             Path::new("basic_defn.clj")).unwrap();
    assert_eq!(meta.name, "my.core");
}

#[test]
fn extracts_defn_with_doc_and_params() {
    let (_, syms) = extract(include_str!("fixtures/snippets/basic_defn.clj"),
                             Path::new("basic_defn.clj")).unwrap();
    let hello = syms.iter().find(|s| s.name == "hello").expect("hello not found");
    assert_eq!(hello.kind, DefKind::Defn);
    assert_eq!(hello.fqn, "my.core/hello");
    assert_eq!(hello.doc.as_deref(), Some("Says hello to someone."));
    assert_eq!(hello.params, vec!["[name]"]);
}

#[test]
fn extracts_def_and_defmacro() {
    let (_, syms) = extract(include_str!("fixtures/snippets/basic_defn.clj"),
                             Path::new("basic_defn.clj")).unwrap();
    assert!(syms.iter().any(|s| s.name == "PI" && s.kind == DefKind::Def));
    assert!(syms.iter().any(|s| s.name == "when-pos" && s.kind == DefKind::Defmacro));
}

#[test]
fn extracts_defn_private() {
    let (_, syms) = extract(include_str!("fixtures/snippets/basic_defn.clj"),
                             Path::new("basic_defn.clj")).unwrap();
    let p = syms.iter().find(|s| s.name == "private-thing").unwrap();
    assert_eq!(p.kind, DefKind::DefnPrivate);
}

#[test]
fn extracts_multi_arity_params() {
    let (_, syms) = extract(include_str!("fixtures/snippets/multi_arity.clj"),
                             Path::new("multi_arity.clj")).unwrap();
    let greet = syms.iter().find(|s| s.name == "greet").unwrap();
    assert_eq!(greet.params.len(), 2);
    assert!(greet.params.contains(&"[name]".to_string()));
    assert!(greet.params.contains(&"[title name]".to_string()));
}

#[test]
fn extracts_ns_aliases_and_refers() {
    let (meta, _) = extract(include_str!("fixtures/snippets/ns_with_requires.clj"),
                             Path::new("ns_with_requires.clj")).unwrap();
    assert_eq!(meta.aliases.get("str").map(|s| s.as_str()), Some("clojure.string"));
    assert_eq!(meta.aliases.get("core").map(|s| s.as_str()), Some("my.core"));
    assert_eq!(meta.refers.get("format-date").map(|s| s.as_str()), Some("my.utils/format-date"));
    assert_eq!(meta.refers.get("parse-id").map(|s| s.as_str()), Some("my.utils/parse-id"));
}

#[test]
fn handles_reader_conditionals() {
    let (_, syms) = extract(include_str!("fixtures/snippets/reader_conditional.cljc"),
                             Path::new("reader_conditional.cljc")).unwrap();
    // Both platform-specific and shared defs extracted
    assert!(syms.iter().any(|s| s.name == "read-file"));
    assert!(syms.iter().any(|s| s.name == "shared-fn"));
}

#[test]
fn name_range_is_just_name_not_full_form() {
    let (_, syms) = extract(include_str!("fixtures/snippets/basic_defn.clj"),
                             Path::new("basic_defn.clj")).unwrap();
    let hello = syms.iter().find(|s| s.name == "hello").unwrap();
    // name_range should be narrower than range (which covers the whole defn)
    assert!(hello.name_range.start.line == hello.range.start.line
            || hello.name_range.start.character > hello.range.start.character);
    assert!(hello.name_range.end.character > hello.name_range.start.character);
}
```

### DONE when:
```
$ cargo test test_extractor   # all tests pass
# Manually: run extractor on a real project file, inspect output looks correct
```

---

## Step 3 — Scanner + Index Population

**Goal**: Walk project directories in parallel, call extractor, populate the in-memory Index.

### Files to create/modify:

**`tests/fixtures/simple_project/deps.edn`**
```clojure
{:paths ["src"]
 :deps {org.clojure/clojure {:mvn/version "1.11.1"}}}
```

**`tests/fixtures/simple_project/src/core.clj`**
```clojure
(ns simple.core)

(def VERSION "1.0.0")

(defn add
  "Adds two numbers."
  [a b]
  (+ a b))

(defn multiply
  "Multiplies two numbers."
  [a b]
  (* a b))
```

**`tests/fixtures/simple_project/src/utils.clj`**
```clojure
(ns simple.utils
  (:require [simple.core :as core]))

(defn add-and-double
  "Adds two numbers then doubles the result."
  [x y]
  (* 2 (core/add x y)))

(defn greet [name]
  (str "Hello, " name))
```

**`src/index/scanner.rs`**

```rust
pub fn build_index(root: &Path, source_paths: &[PathBuf]) -> anyhow::Result<Index>
```

Implementation:
- Create `Index::new()`
- For each source path: use `ignore::WalkBuilder::new(path).build()` to get file iterator
- Filter for extensions: `.clj`, `.cljs`, `.cljc`
- Collect all file paths into a `Vec<PathBuf>`
- Use `rayon::prelude::*` → `par_iter()` → read file, call `extractor::extract()`, return `(NsMeta, Vec<Symbol>)`
- Collect results (handle errors: log + skip, don't propagate)
- Sequential pass: call `index.insert_file(meta, symbols)` for each

**`src/index/mod.rs`** — implement the public API:

```rust
impl Index {
    pub fn insert_file(&self, meta: NsMeta, symbols: Vec<Symbol>) {
        let ns_name = meta.name.clone();
        // Insert all symbols into self.symbols (keyed by fqn)
        // Insert symbol names into self.ns_symbols (keyed by ns)
        // Insert meta into self.namespaces
        // Insert file→ns mapping into self.file_to_ns
    }

    pub fn remove_file(&self, path: &Path) {
        // Look up ns name from file_to_ns
        // Remove all symbols where symbol.ns == ns_name
        // Remove from ns_symbols, namespaces, file_to_ns
    }

    pub fn lookup(&self, fqn: &str) -> Option<Symbol> {
        self.symbols.get(fqn).map(|r| r.clone())
    }

    pub fn lookup_in_ns(&self, ns: &str, name: &str) -> Option<Symbol> {
        let fqn = format!("{}/{}", ns, name);
        self.lookup(&fqn)
    }

    pub fn ns_meta(&self, ns: &str) -> Option<NsMeta> {
        self.namespaces.get(ns).map(|r| r.clone())
    }

    pub fn file_ns(&self, path: &Path) -> Option<String> {
        self.file_to_ns.get(path).map(|r| r.clone())
    }

    // prefix: bare prefix string, current_ns: for filtering visible symbols
    pub fn complete(&self, prefix: &str, current_ns: &str) -> Vec<Symbol> {
        // implement in Step 5
        vec![]
    }
}
```

**`tests/test_index.rs`**

```rust
#[test]
fn indexes_all_files_in_project() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index = scanner::build_index(root, &paths).unwrap();

    assert!(index.lookup("simple.core/add").is_some());
    assert!(index.lookup("simple.core/multiply").is_some());
    assert!(index.lookup("simple.core/VERSION").is_some());
    assert!(index.lookup("simple.utils/greet").is_some());
    assert!(index.lookup("simple.utils/add-and-double").is_some());
}

#[test]
fn index_contains_ns_metadata() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index = scanner::build_index(root, &paths).unwrap();

    let meta = index.ns_meta("simple.utils").unwrap();
    assert_eq!(meta.aliases.get("core").map(|s| s.as_str()), Some("simple.core"));
}

#[test]
fn remove_file_cleans_up_all_symbols() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index = scanner::build_index(root, &paths).unwrap();

    assert!(index.lookup("simple.utils/greet").is_some());

    let utils_path = root.join("src/utils.clj");
    index.remove_file(&utils_path);

    assert!(index.lookup("simple.utils/greet").is_none());
    assert!(index.ns_meta("simple.utils").is_none());
}

#[test]
fn insert_file_updates_index() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index = scanner::build_index(root, &paths).unwrap();

    // Simulate re-indexing with a new symbol added
    let new_source = r#"
        (ns simple.utils (:require [simple.core :as core]))
        (defn new-fn [x] x)
    "#;
    let fake_path = root.join("src/utils.clj");
    index.remove_file(&fake_path);
    let (meta, syms) = extractor::extract(new_source, &fake_path).unwrap();
    index.insert_file(meta, syms);

    assert!(index.lookup("simple.utils/new-fn").is_some());
    // Old symbol gone
    assert!(index.lookup("simple.utils/greet").is_none());
}
```

Wire into `server.rs` `initialized` handler — spawn a tokio task that calls `scanner::build_index()` and populates `Backend`'s `Arc<Index>`. Since `Index` uses `DashMap` internally, no `RwLock` is needed — just call `index.insert_file()` directly through the `Arc`.

### DONE when:
```
$ cargo test test_index   # all tests pass
# Log shows: "Index built: 5 symbols in 2 namespaces in 45ms" (or similar)
```

---

## Step 4 — Jump to Definition

**Goal**: `textDocument/definition` returns correct file + position.

### Files to create/modify:

**`src/handlers/definition.rs`**

```rust
pub async fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: GotoDefinitionParams,
) -> anyhow::Result<Option<GotoDefinitionResponse>>
```

Resolution algorithm (implement exactly in this order):
1. Get URI + position from params
2. Call `documents.word_at(uri, position)` → `Option<String>`
3. If no word: return `Ok(None)`
4. Get current file's namespace: `index.file_ns(path)`
5. Get `NsMeta` for current namespace
6. Resolve the word:
   - If word contains `/`: split on first `/` → `(alias, name)`
     - Look up `alias` in `NsMeta.aliases` → `full_ns`
     - Construct `fqn = full_ns/name`
     - `index.lookup(fqn)`
   - If word contains no `/`:
     - Check `NsMeta.refers.get(word)` → if found, use that fqn → `index.lookup(fqn)`
     - Check `index.lookup_in_ns(current_ns, word)`
     - Check `index.core_symbols` (see Step 5 — core has no file, return None for core)
     - Return first match
7. If symbol found: return `GotoDefinitionResponse::Scalar(Location { uri: file_uri, range: symbol.name_range })`
8. If not found: return `Ok(None)`

**`src/document.rs`** — implement `word_at`:
- Get the rope for the URI
- Get the line at `position.line`
- Find character boundaries around `position.character` by scanning for non-identifier chars
- Clojure identifier chars: alphanumeric + `- _ / . ? ! * + > < = # ' & %`
- Return the word as `String`

**`tests/test_definition.rs`**

```rust
// Helper: build index from simple_project and a mock DocumentStore
fn test_setup() -> (Index, DocumentStore) { ... }

#[test]
fn resolves_definition_in_same_namespace() {
    // In core.clj, "add" at some position should resolve to the defn add location
}

#[test]
fn resolves_definition_via_alias() {
    // In utils.clj, "core/add" resolves to simple.core/add in core.clj
    let (index, docs) = test_setup();
    // Simulate cursor on "core" in "core/add"
    let word = "core/add";
    // Call resolution logic directly (extract resolution fn as pub for testing)
    let sym = resolve_symbol(&index, word, "simple.utils").unwrap();
    assert_eq!(sym.fqn, "simple.core/add");
    assert!(sym.file.ends_with("core.clj"));
}

#[test]
fn resolves_definition_via_refer() {
    // Setup: ns has (:refer [some-fn]) from another ns
    // bare "some-fn" resolves correctly
}

#[test]
fn returns_none_for_unknown_symbol() {
    let (index, _) = test_setup();
    let result = resolve_symbol(&index, "nonexistent/thing", "simple.core");
    assert!(result.is_none());
}
```

Wire into `server.rs`:
```rust
async fn goto_definition(&self, params: GotoDefinitionParams) -> Result<Option<GotoDefinitionResponse>> {
    handlers::definition::handle(&self.index, &self.documents, params)
        .await
        .map_err(|e| { tracing::error!("definition error: {}", e); tower_lsp::jsonrpc::Error::internal_error() })
}
```

### DONE when:
```
$ cargo test test_definition   # all tests pass
# Manual: open fixture project in editor, put cursor on "core/add" in utils.clj, trigger go-to-def → lands on defn add in core.clj
```

---

## Step 5 — Completion + Clojure.core Builtins

**Goal**: Autocomplete returns project symbols and clojure.core builtins filtered by prefix.

### Files to create/modify:

**`src/index/core.rs`**

Generate this file using a babashka script (see below) or write manually for ~50 key functions as a start. Format:

```rust
use super::CoreSymbol;

pub fn core_symbols() -> Vec<CoreSymbol> {
    vec![
        CoreSymbol {
            name: "map".to_string(),
            params: "([f coll] [f c1 c2] [f c1 c2 c3] [f c1 c2 c3 & colls])".to_string(),
            doc: "Returns a lazy sequence consisting of the result of applying f to\nthe set of first items of each coll...".to_string(),
        },
        CoreSymbol { name: "mapv".to_string(), params: "([f coll] [f c1 c2] [f c1 c2 c3] [f c1 c2 c3 & colls])".to_string(), doc: "Returns a vector...".to_string() },
        CoreSymbol { name: "filter".to_string(), params: "([pred] [pred coll])".to_string(), doc: "Returns a lazy sequence of items in coll for which pred returns true...".to_string() },
        CoreSymbol { name: "reduce".to_string(), params: "([f coll] [f val coll])".to_string(), doc: "f should be a function of 2 arguments...".to_string() },
        CoreSymbol { name: "into".to_string(), params: "([to from] [to xform from])".to_string(), doc: "Returns a new coll consisting of to-coll with all of the items of from-coll conjoined.".to_string() },
        // ... continue for all clojure.core public functions
    ]
}
```

**Babashka script to generate `core.rs`** — create as `scripts/gen_core.clj`:
```clojure
#!/usr/bin/env bb
;; Run: bb scripts/gen_core.clj > src/index/core.rs
;; Requires: clojure.core loaded in bb

(println "use super::CoreSymbol;")
(println)
(println "pub fn core_symbols() -> Vec<CoreSymbol> {")
(println "    vec![")

(doseq [[sym-name var] (sort-by key (ns-publics 'clojure.core))
        :let [m (meta var)
              params (str (:arglists m))
              doc (or (:doc m) "")
              ;; escape for Rust string
              doc-escaped (-> doc (clojure.string/replace "\\" "\\\\") (clojure.string/replace "\"" "\\\"") (clojure.string/replace "\n" "\\n"))
              params-escaped (-> params (clojure.string/replace "\"" "\\\""))]]
  (println (str "        CoreSymbol { name: \"" sym-name "\".to_string(), params: \""
                params-escaped "\".to_string(), doc: \""
                doc-escaped "\".to_string() },")))

(println "    ]")
(println "}")
```

**`src/handlers/completion.rs`**

```rust
pub async fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: CompletionParams,
) -> anyhow::Result<Option<CompletionResponse>>
```

Implementation:
1. Get cursor position + URI
2. Get current word prefix from `documents.word_at()` (may be partial)
3. Get current ns from `index.file_ns(path)`
4. Get `NsMeta` for current ns
5. Build completion candidates from three pools:

   **Pool A — current ns symbols**:
   - All symbols where `symbol.ns == current_ns`
   - Filter: `symbol.name.starts_with(prefix)`

   **Pool B — required ns symbols** (via aliases):
   - For each `(alias, full_ns)` in `NsMeta.aliases`:
     - If prefix starts with `alias + "/"`: filter symbols in `full_ns` by `name` part of prefix
     - If prefix is bare: include all refers (`:refer` imports) matching prefix
   - For `:refer` symbols: include bare names

   **Pool C — clojure.core**:
   - Filter `index.core_symbols` by `name.starts_with(prefix)`

6. Deduplicate by label, convert to `CompletionItem`:
```rust
CompletionItem {
    label: symbol.name.clone(),
    detail: Some(format!("{} ({})", symbol.ns, params_display(&symbol.params))),
    documentation: symbol.doc.as_ref().map(|d| {
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: d.clone(),
        })
    }),
    kind: Some(match symbol.kind {
        DefKind::Defn | DefKind::DefnPrivate | DefKind::Defmacro => CompletionItemKind::FUNCTION,
        DefKind::Def => CompletionItemKind::VARIABLE,
        DefKind::Defprotocol => CompletionItemKind::INTERFACE,
        DefKind::Defrecord | DefKind::Deftype => CompletionItemKind::CLASS,
        _ => CompletionItemKind::VALUE,
    }),
    ..Default::default()
}
```

**`tests/test_completion.rs`**

```rust
#[test]
fn completes_symbols_in_current_ns() {
    let index = build_test_index();
    let completions = handlers::completion::complete_symbols(&index, "add", "simple.core");
    assert!(completions.iter().any(|c| c.label == "add"));
    assert!(completions.iter().any(|c| c.label == "add-and-double" == false)); // wrong ns
}

#[test]
fn completes_with_alias_prefix() {
    let index = build_test_index();
    // utils.clj has :require [simple.core :as core]
    let completions = handlers::completion::complete_symbols(&index, "core/ad", "simple.utils");
    assert!(completions.iter().any(|c| c.label == "add"));
}

#[test]
fn completes_clojure_core_builtins() {
    let index = Index::new_with_core();
    let completions = handlers::completion::complete_symbols(&index, "map", "any.ns");
    assert!(completions.iter().any(|c| c.label == "map"));
    assert!(completions.iter().any(|c| c.label == "mapv"));
    assert!(completions.iter().any(|c| c.label == "map-indexed"));
}

#[test]
fn completion_item_has_doc_and_detail() {
    let index = build_test_index();
    let completions = handlers::completion::complete_symbols(&index, "add", "simple.core");
    let item = completions.iter().find(|c| c.label == "add").unwrap();
    assert!(item.detail.is_some());
    assert!(item.documentation.is_some());
}

#[test]
fn empty_prefix_returns_all_visible_symbols() {
    let index = build_test_index();
    let completions = handlers::completion::complete_symbols(&index, "", "simple.core");
    assert!(completions.len() >= 3); // at least: add, multiply, VERSION + core builtins
}
```

### DONE when:
```
$ cargo test test_completion   # all tests pass
# Manual: type "map" in editor → see map, mapv, map-indexed etc.
# Manual: type "core/" in utils.clj → see add, multiply from simple.core
```

---

## Step 6 — Hover

**Goal**: `textDocument/hover` returns formatted docstring + signature.

### Files to create/modify:

**`src/handlers/hover.rs`**

```rust
pub async fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: HoverParams,
) -> anyhow::Result<Option<Hover>>
```

Implementation:
1. Resolve symbol using same logic as `definition.rs` (extract `resolve_symbol()` as shared fn in `handlers/mod.rs`)
2. If not found: return `Ok(None)`
3. Format markdown:

```rust
fn format_hover(sym: &Symbol) -> String {
    let mut md = String::new();
    // Signature block
    let params = if sym.params.is_empty() {
        String::new()
    } else {
        sym.params.join(" ")
    };
    md.push_str(&format!("```clojure\n({} {}{})\n```\n", 
        defkind_str(&sym.kind), sym.name,
        if params.is_empty() { String::new() } else { format!(" {}", params) }
    ));
    // Namespace
    md.push_str(&format!("*{}*\n\n", sym.ns));
    // Docstring
    if let Some(doc) = &sym.doc {
        md.push_str(doc);
    }
    md
}
```

For `CoreSymbol`, format similarly without file location.

**`tests/test_hover.rs`**

```rust
#[test]
fn hover_returns_doc_for_known_symbol() {
    let index = build_test_index();
    let result = handlers::hover::format_for_symbol(
        index.lookup("simple.core/add").as_ref().unwrap()
    );
    assert!(result.contains("add"));
    assert!(result.contains("[a b]"));
    assert!(result.contains("Adds two numbers"));
    assert!(result.contains("simple.core"));
}

#[test]
fn hover_formats_as_clojure_code_block() {
    let index = build_test_index();
    let sym = index.lookup("simple.core/add").unwrap();
    let md = handlers::hover::format_for_symbol(&sym);
    assert!(md.contains("```clojure"));
    assert!(md.contains("```"));
}

#[test]
fn hover_returns_none_for_unknown() {
    let index = Index::new_with_core();
    let result = handlers::hover::resolve_and_format(&index, "nonexistent/fn", "any.ns");
    assert!(result.is_none());
}
```

### DONE when:
```
$ cargo test test_hover   # all tests pass
# Manual: hover over "add" in utils.clj → popup shows signature and docstring
```

---

## Step 7 — File Watching + Re-index

**Goal**: When a file is saved, re-index it so definition/completion/hover are immediately up to date.

### Modify:

**`server.rs`** — implement `did_save` (DashMap-backed Index needs no lock):
```rust
async fn did_save(&self, params: DidSaveTextDocumentParams) {
    let Some(path) = params.text_document.uri.to_file_path().ok() else { return };
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => { tracing::warn!("failed to read {}: {}", path.display(), e); return; }
    };
    self.index.remove_file(&path);
    match extractor::extract(&source, &path) {
        Ok((meta, symbols)) => self.index.insert_file(meta, symbols),
        Err(e) => tracing::warn!("failed to re-index {}: {}", path.display(), e),
    }
    tracing::info!("re-indexed {}", path.display());
}
```

`did_change` should already be wired from Step 1 (updating DocumentStore). No additional lock needed — `DocumentStore` is also backed by `DashMap`.

No new tests needed — verify manually by:
1. Open utils.clj, hover over a symbol → see its doc
2. Edit core.clj, change the docstring, save
3. Hover over same symbol → see updated doc

---

## Step 8 — Polish + Distribution

**Goal**: Usable by others, logs are useful, packaging works.

### Tasks (no new tests, verification is manual):

**Error handling audit**:
- Search codebase for `unwrap()` calls — replace all with `?` or explicit logging
- Every LSP handler must catch errors and return `tower_lsp::jsonrpc::Error::internal_error()` rather than panicking

**Logging**:
- At startup: log project root, source paths, count of files found
- After indexing: log `"Indexed N symbols in M namespaces in Xms"`
- On each definition request: log the word being resolved and whether it was found
- Log file: `~/.cache/fast-clj-lsp/server.log`

**VS Code config** — create `editors/vscode/`:
```json
// .vscode/settings.json for users
{
  "clojure.lsp.server.path": "/path/to/fast-clj-lsp"
}
```
Or minimal `package.json` extension activating the server for `clojure` language ID.

**Zed config** — `~/.config/zed/settings.json`:
```json
{
  "lsp": {
    "fast-clj-lsp": {
      "binary": { "path": "/path/to/fast-clj-lsp" }
    }
  },
  "languages": {
    "Clojure": { "language_servers": ["fast-clj-lsp"] }
  }
}
```

**Binary size / release build**:
```toml
# Cargo.toml
[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
```

**`--version` flag** — add to `main.rs` before LSP startup:
```rust
if std::env::args().any(|a| a == "--version") {
    println!("fast-clj-lsp {}", env!("CARGO_PKG_VERSION"));
    return;
}
```
No need for `clap` — this is the only CLI flag.

**README.md** — include:
- Install: `cargo install --path .`
- Verify: `fast-clj-lsp --version`
- Editor setup sections
- Limitations (v1 scope)

---

## Shared Utilities — `handlers/mod.rs`

Extract this function so both `definition.rs` and `hover.rs` share it:

```rust
pub fn resolve_symbol<'a>(
    index: &'a Index,
    word: &str,
    current_ns: &str,
) -> Option<ResolvedSymbol<'a>> {
    // Returns either a project Symbol or a CoreSymbol
    // Algorithm: see Step 4 definition section
}

pub enum ResolvedSymbol {
    Project(Symbol),
    Core(CoreSymbol),
}
```

---

## Agent Rules (always follow)

1. **Run `cargo test` after every file change** — never leave tests failing
2. **Never use `.unwrap()` in `src/`** — use `?`, `.ok()`, or explicit match with logging
3. **Never write to stdout** in the server — use `tracing::info!()` etc.
4. **Extractor is pure** — `extractor::extract()` takes `&str`, returns data, no IO ever
5. **Handlers are thin** — max ~20 lines in `server.rs` per handler, all logic in `handlers/`
6. **When a test fails**, fix the implementation, not the test (unless the test is demonstrably wrong)
7. **When adding a new def form** (e.g. `defstate` from mount), add it to `DefKind` enum and update `str_to_defkind()` and all match arms — the compiler will tell you every place to update
8. **Index API is the only way** for handlers to read data — no direct DashMap access outside `index/mod.rs`
