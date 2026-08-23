# Architecture

## Data Flow

editor keystroke
  → tower-lsp (server.rs) receives LSP request
  → delegates to handlers/ function with (&Index, &DocumentStore, params)
  → handler queries Index via public API only
  → returns LSP response type

## Index Population (startup)

projects.rs: detect subprojects (dirs holding deps.edn / project.clj / lgx.edn,
  gitignore-respecting, max depth 4); merge `.clj-pulse/config.edn` and editor
  `:projects` overrides into the resolved project list (workspace root first)
config.rs: read :paths from each project's manifest (default ["src" "test"])
scanner.rs: walk each project's paths with a project-scoped walker
  (`ScanRoot`: gitignore ancestry stops at the project dir), collect
  .clj/.cljs/.cljc files
scanner.rs: rayon::par_iter → call extractor::extract(file_contents) per file
extractor.rs: tree-sitter parse → return (NsMeta, Vec<Symbol>)
scanner.rs: merge results into the one shared Arc<Index> via DashMap inserts
  (a namespace defined by two projects: last writer wins, with a warning)
server.rs: per project, stage 2 indexes the cached classpath (.cpcache / lgx /
  lein) into the same index; stage 3 (opt-in per project, root-only by
  default) runs the project's `:cmd` and rebuilds the per-project library
  union when the entry set changed

## Keyword & Integrant Indexing

extractor.rs: qualified keywords (`:ns/name`, `::name`, `::alias/name`) are
  recorded as occurrences with colon-prefixed fqns (`:ns/name`), keyed disjointly
  from var fqns. Only qualified keywords are indexed.
extractor.rs: `(defmethod ig/init-key ::x …)` records `:ns/x` as an IntegrantKey
  *definition* (`DefKind::IntegrantKey`); the other lifecycle defmethods and all
  config uses are occurrences, so goto-definition lands on the constructor.
scanner.rs: EDN files under `:paths` containing `#ig/ref` are scanned for keyword
  occurrences (`extract_edn`) and inserted via `Index::insert_edn_file`
  (occurrences only) — this links a `config.edn` component key to its defmethod.

## Index Re-population (on file save)

server.rs didSave handler
  → index.remove_file(path)    ← removes all symbols from that file
  → extractor::extract(new_contents)
  → index.insert_file(ns_meta, symbols)

## Symbol Resolution (for definition + hover)

Definition first checks for a **local binding** in scope at the cursor
(`extractor::locals_in_scope_at`): a name bound by `let`/`fn`/`loop`/`for`/
destructuring/`letfn`/… resolves to its binding site in the same file, shadowing
any same-named var. Only when the cursor is not on an in-scope local does it fall
back to the var/alias/namespace/core resolution below. (Completion likewise
prepends in-scope locals; find-references resolves a local's usages
structurally within the file via `extractor::local_references_at`, since locals
are never recorded as occurrences.)

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
- Keyword fqns are colon-prefixed (`:ns/name`) so they never collide with var
  fqns; keyword occurrences span the whole keyword token (navigation-only — the
  rename path rejects keyword fqns).
- EDN config files contribute occurrences only (no namespace, no symbols),
  registered under a NUL sentinel ns in `file_to_ns` so re-scans keep them.
