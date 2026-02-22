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
