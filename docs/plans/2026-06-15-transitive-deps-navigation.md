# Transitive Deps Navigation Implementation Plan

> **Status: COMPLETED (2026-06-15).** See the summary at the end.

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** When the current document is an external library file (a `jar:` entry,
or a `file:` source-dir dep), go-to-definition, find-references, hover, and
signature-help work from *inside* it — so you can navigate from one library into
its own transitive dependencies and see who uses a library symbol.

**Tech Stack:** Rust, tower-lsp, the existing `Index`/`Occurrence`/`DocumentStore`
infrastructure, the stdio e2e harness (`tests/test_e2e.rs`).

---

## Design

### The core finding

The entire transitive classpath is **already indexed**. `.cpcache` is the full
transitive classpath, and `scanner::index_classpath_libs` walks every JAR and
source dir on it. Symbols, namespaces, aliases, and `file_to_ns` entries for
transitive deps are all present in the index.

The one thing blocking navigation *from* a library file is mechanical: every
inspection handler begins with `uri.to_file_path()`, which **fails for `jar:`
URIs**. So the moment the current document is a JAR entry (you jumped into
`mylib/core.clj`, now open as `jar:file://…!/mylib/core.clj`), the handler bails
at line 1 — `current_ns` resolves to empty and `resolve_fqn_at` returns `None`.
The index would happily resolve `util/helper` → `mylib.util/helper` → its `jar:`
location; the handler just never gets there.

Dir-based libs (git deps, `:local/root`) use plain `file:` URIs, so
`to_file_path()` already works and navigation from them largely works today. The
substantive work is making `jar:` source files first-class for the read
handlers.

### Approach

Add one small `uri` module with a symmetric pair of helpers, then route the four
inspection handlers (and the references result-builder) through them:

- `uri::to_index_path(&Url) -> Option<PathBuf>` — `file:` → real path; a
  `jar:file://X!/entry` URI → the virtual path `X!/entry`, which is the exact key
  already stored in `file_to_ns` and `Symbol.file`. Reuses
  `jar_content::parse_jar_uri`.
- `uri::from_index_path(&Path) -> Result<Url>` — a path containing `!/` →
  a `jar:` URI; otherwise a `file:` URI. This **generalizes** the existing
  `location_for` in `definition.rs` (which branches on `SymbolSource`) and the
  `!/`-sniffing in `namespace_location`; both collapse onto this helper.

The canonical virtual path round-trips cleanly: index → `from_index_path` → the
editor opens that `jar:` URI → the editor sends it back → `to_index_path` → the
same canonical virtual path. So `file_ns`/`ns_meta`/`lookup` all hit, and
resolution proceeds exactly as it does for a project file.

### Why this is enough

For navigation *from* an open library file, resolution only needs three things,
all already true once the URI converts:

1. `index.file_ns(virtual_path)` → the lib file's namespace (`insert_lib_file`
   records it).
2. `documents.text(jar_uri)` → the live buffer (the editor opened the jar doc to
   display it, so it is in the `DocumentStore`).
3. `extractor::extract_full(text, virtual_path)` → the lib file's own ns form,
   aliases, and the occurrence under the cursor, resolved to its fqn.

The target symbol (the transitive dep) is already indexed, and `from_index_path`
turns its virtual path back into a `jar:` URI for the response.

### Key decisions

1. **References from a library symbol = project usages + usages in
   currently-open library files. Not a full cross-library reference search.** The
   `occurrences` index is project-only *by invariant* (`merge_project_from`,
   `clear_libs`, and "occurrences keys are exactly project files" all depend on
   it). `occurrences_for` already re-extracts *open* documents live — so once the
   open jar doc's URI converts correctly, its own usages are included for free,
   with zero index bloat and no invariant change. Indexing every library's
   occurrences (to find lib→lib refs across unopened files) would be a much
   larger, invariant-breaking effort and is out of scope.
2. **Include the jar/dir declaration in references results.** Today decls are
   pushed only for `Project`/`Dir` sources; jar decls are skipped. Now that
   `jar:` locations are navigable, render them via `from_index_path` so
   find-references on a library symbol also lists its declaration. Rename is
   unchanged — it still refuses non-project symbols separately.
3. **Scope = definition, references, hover, signature** (the navigate/inspect
   handlers). Completion and code_action are editing-oriented and jar docs are
   read-only; they keep their `to_file_path()` path (which already works for
   `file:` dir-libs) and are left out. Cheap to add later via the same helper.
4. **Deliberate non-change: `did_open` keeps skipping `jar:` URIs.** Its
   `to_file_path()` guard already makes it ignore jar docs for indexing — and it
   must. Indexing a jar doc as a project file would create a project-owned ns and
   a virtual-path `occurrences` entry, violating the project-only invariant and
   corrupting `clear_libs`. The whole fix lives in the read handlers, never in
   indexing.

### Testing strategy

`bb e2e` is the done-gate (per CLAUDE.md: server behavior changes are not done
until `bb e2e` passes). The `uri` helpers get pure round-trip unit tests. The
behavior is locked by new e2e tests that open a JAR entry by its `jar:` URI and
drive definition/references/hover from inside it, using a JAR fixture with two
namespaces where one requires the other (the transitive shape).

## File Structure

- **Create `src/uri.rs`** — `to_index_path` / `from_index_path` + unit tests;
  one responsibility: URI ⇄ index-path translation across `file:`/`jar:`.
- **Modify `src/lib.rs`** — `pub mod uri;`.
- **Modify `src/handlers/definition.rs`** — `current_ns` path via
  `uri::to_index_path`; refactor `location_for` and `namespace_location` onto
  `uri::from_index_path` (drop the `SymbolSource`-based branching).
- **Modify `src/handlers/references.rs`** — `resolve_fqn_at` and the
  `occurrences_for` open-doc loop use `uri::to_index_path`; result locations and
  the declaration use `uri::from_index_path` (now including jar/dir decls).
- **Modify `src/handlers/hover.rs`, `src/handlers/signature.rs`** — `current_ns`
  path via `uri::to_index_path`.
- **Modify `tests/test_e2e.rs`** — `*_uri` client helpers, a `position_in_text`
  helper, a two-namespace JAR fixture, and the behavioral tests.
- **Modify `docs/ROADMAP.md`** — check the "Transitive Clojure deps navigation"
  item.

Reuse `jar_content::parse_jar_uri`, `references::resolve_fqn_at`,
`references::occurrences_for`. No new public index types.

---

### Task 1: `uri` module — URI ⇄ index-path round-trip

**Files:**
- Create: `src/uri.rs`
- Modify: `src/lib.rs`

- [x] **Step 1: Write the failing unit tests** (in `src/uri.rs` `#[cfg(test)]`):
  - `to_index_path` on a `file:///a/b.clj` URL returns `PathBuf("/a/b.clj")`.
  - `to_index_path` on `jar:file:///x.jar!/mylib/util.clj` returns the virtual
    path `PathBuf("/x.jar!/mylib/util.clj")`.
  - `from_index_path` on `/a/b.clj` returns a `file:` URL.
  - `from_index_path` on the virtual path `/x.jar!/mylib/util.clj` returns the
    URL string `jar:file:///x.jar!/mylib/util.clj`.
  - Round-trip: `to_index_path(from_index_path(p)) == p` for both shapes.

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib uri::`
  Expected: FAIL (module/functions don't exist yet).

- [x] **Step 3: Implement**
  - `to_index_path(uri: &Url) -> Option<PathBuf>`: if `uri.scheme() == "jar"`,
    call `jar_content::parse_jar_uri(uri.as_str())` and rebuild the virtual path
    as `format!("{}!/{}", jar_path.display(), entry)`; otherwise
    `uri.to_file_path().ok()`.
  - `from_index_path(path: &Path) -> anyhow::Result<Url>`: if the path string
    contains `!/`, split once on it, `Url::from_file_path(jar_part)`, then
    `Url::parse(&format!("jar:{}!/{}", jar_url, entry_part))`; otherwise
    `Url::from_file_path(path)`. (This is exactly the existing `location_for`
    logic, minus the `Range`/`SymbolSource`.)
  - `pub mod uri;` in `src/lib.rs`.

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib uri::`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Add uri module for file:/jar: index-path translation"`

### Task 2: Definition + hover + signature — accept library-file URIs

**Files:**
- Modify: `src/handlers/definition.rs`
- Modify: `src/handlers/hover.rs`
- Modify: `src/handlers/signature.rs`

- [x] **Step 1: Implement**
  - In all three handlers, replace the `uri.to_file_path()` step that computes
    the path for `index.file_ns(&path)` with `uri::to_index_path(&uri)` (return
    `Ok(None)` when it yields `None`). This makes `current_ns` correct when the
    open document is a `jar:` entry.
  - In `definition.rs`, refactor `location_for` to build its `Url` via
    `uri::from_index_path(file)` (keep the `Range` parameter, drop the
    `SymbolSource` parameter and its match), and update its two call sites.
  - In `definition.rs`, refactor `namespace_location` to build the target `Url`
    via `uri::from_index_path(&meta.file)`, removing the local `is_jar` /
    synthesized-`SymbolSource` dance.

- [x] **Step 2: Verify build + existing tests (no regressions)**
  Run: `bb check`
  Expected: PASS.

- [x] **Step 3: Commit**
  `git commit -m "Resolve definition/hover/signature from inside library files"`

### Task 3: References — resolve, search, and render library-file locations

**Files:**
- Modify: `src/handlers/references.rs`

- [x] **Step 1: Implement**
  - `resolve_fqn_at`: replace `uri.to_file_path().ok()?` with
    `uri::to_index_path(uri)?` so the symbol under the cursor resolves when the
    current document is a `jar:` entry.
  - `occurrences_for`: in the open-document loop, convert each open URI with
    `uri::to_index_path` instead of `uri.to_file_path()`, so the currently-open
    jar doc's live occurrences are included in the search.
  - `references`: build each result `Location` URI with `uri::from_index_path`
    (instead of `Url::from_file_path`), so occurrences located in a jar doc come
    back as `jar:` URIs. Skip entries where conversion fails.
  - `references` declaration block: drop the
    `matches!(sym.source, Project | Dir)` gate and build the declaration
    `Location` via `uri::from_index_path(&sym.file)`, so a library symbol's own
    declaration is listed too.
  - Leave `rename` untouched (it independently refuses non-`Project` symbols).

- [x] **Step 2: Verify build + existing tests (no regressions)**
  Run: `bb check`
  Expected: PASS — existing references/rename e2e and unit tests still pass.

- [x] **Step 3: Commit**
  `git commit -m "References: resolve and render usages from library files"`

### Task 4: End-to-end — navigate and find references from a JAR file

**Files:**
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Add the e2e scaffolding**
  - `LspClient` helpers that take a raw URI string rather than a `&Path`:
    `did_open_uri(uri, text)`, `goto_definition_uri(uri, line, ch)`,
    `references_uri(uri, line, ch, include_decl)`, `hover_uri(uri, line, ch)`.
    Model them on the existing path-based methods (same JSON, URI passed through).
  - A free helper `position_in_text(text: &str, needle: &str) -> (u32, u32)`
    mirroring `position_of` but scanning an in-memory string (jar content is not
    on disk).
  - A fixture helper that builds a two-namespace JAR and puts it on the
    classpath of a `setup_project()` root:
    - `mylib/util.clj`: `(ns mylib.util)\n(defn helper [x] x)\n`
    - `mylib/core.clj`:
      `(ns mylib.core\n  (:require [mylib.util :as util]))\n(defn run [x] (util/helper x))\n`
    - Write the JAR's absolute path into `.cpcache/1.cp` (same mechanism as
      `test_e2e_completion_from_directory_library`).

- [x] **Step 2: Write the failing behavioral tests**
  - **Transitive jar→jar definition.** Project consumer
    `src/uses_lib.clj`:
    `(ns uses-lib\n  (:require [mylib.core :as core]\n            [mylib.util :as util]))\n\n(core/run 1)\n(util/helper 2)\n`.
    Initialize, `wait_for_log("library indexing complete")`, `did_open` the
    consumer. `goto_definition` on `core/run` → a `jar:` URI ending
    `!/mylib/core.clj`. Fetch its source via `text_document_content`,
    `did_open_uri` it, then `goto_definition_uri` on the `helper` of
    `util/helper` *inside core.clj* → a `jar:` URI ending `!/mylib/util.clj`.
  - **References from inside a library file.** With the same fixture: open the
    consumer (project usage of `util/helper`), open `core.clj` as a jar doc
    (a second usage), then open `util.clj` as a jar doc. `references_uri` on the
    `helper` declaration inside `util.clj` with `includeDeclaration: true`
    returns locations that include: the consumer usage (a `file:` URI), the
    `core.clj` usage (a `jar:` URI ending `!/mylib/core.clj`), and the
    declaration (a `jar:` URI ending `!/mylib/util.clj`).
  - **Hover from inside a library file.** `hover_uri` on `util/helper` inside the
    open `core.clj` jar doc returns markdown mentioning `helper` and
    `mylib.util`.

- [x] **Step 3: Run to verify they fail, then pass after Tasks 1–3**
  Run: `cargo test --test test_e2e transitive` (and the references/hover names)
  Expected: PASS (they exercise the Task 1–3 changes).

- [x] **Step 4: Full suite**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "e2e: navigate and find references from inside library files"`

### Task 5: Roadmap note

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Check the item**
  Mark the Phase 5 "Transitive Clojure deps navigation" item done, noting that
  definition/references/hover/signature now work from inside `jar:` (and
  `file:` dir-dep) library files, reaching transitive dependencies.

- [x] **Step 2: Commit**
  `git commit -m "Roadmap: note transitive deps navigation"`

---

## Notes & limitations

- **References scope is project + open library files**, by design (key decision
  1). Usages in *unopened* library files are not found — the `occurrences` index
  stays project-only to preserve the index invariants.
- **The target namespace must be indexed.** Transitive deps on the classpath are,
  but namespaces filtered at index time (`.impl` / `.internal`) are not reachable.
- **`jar:` navigation still needs client wiring** in the maintainer's Calva setup
  (pre-existing, noted in CLAUDE.md). This change is server-side and verified by
  the stdio `bb e2e` harness, which drives `jar:` URIs through `didOpen`.
- **Completion / code_action are out of scope** for `jar:` docs (read-only);
  they keep working for `file:` dir-deps via their existing path handling.
- **A real directory named literally `<name>.jar!`** would have its files
  mistaken for JAR entries by the `.jar!/` path heuristic (see the
  `split_jar_virtual_path` note). Pathological and accepted; string-only
  detection cannot resolve it without disk probing.

---

## Implementation summary (2026-06-15)

Implemented on branch `transitive-deps-navigation`. All `bb check` and `bb e2e`
(46 tests) pass.

- **`src/uri.rs` (new)** — `to_index_path` / `from_index_path` translate between
  editor URIs and index paths across `file:`/`jar:` schemes; 5 round-trip unit
  tests. Registered in both `src/lib.rs` and `src/main.rs` (the binary
  re-declares modules, so it needed its own `mod uri;`).
- **`src/handlers/definition.rs`, `hover.rs`, `signature.rs`** — current-ns path
  resolved via `uri::to_index_path`, so a JAR buffer is a valid "current file".
  `location_for`/`namespace_location` collapsed onto `uri::from_index_path`,
  dropping the `SymbolSource`-based URI branching.
- **`src/handlers/references.rs`** — `resolve_fqn_at` and the `occurrences_for`
  open-doc loop convert via `to_index_path`; result and declaration locations
  render via `from_index_path` (jar/dir decls now included).
- **`tests/test_e2e.rs`** — `*_uri` client helpers, `position_in_text`, a
  two-namespace JAR fixture, and tests for transitive jar→jar definition,
  references from inside a library file (project + lib→lib + declaration), and
  hover from inside a library file.
- **`docs/ROADMAP.md`** — Phase 5 transitive-deps item checked.

### Codex review follow-ups (both fixed)

Two `review-with-codex` passes each surfaced a real P2 regression introduced by
this change:

1. **Rename from a read-only library buffer.** Routing `rename` through the
   now-jar-aware `resolve_fqn_at` meant a rename initiated from a JAR buffer
   could, when a project symbol shadows the library symbol's fqn, resolve to the
   project symbol and edit project files (before this change, the `to_file_path`
   failure stopped it). Fixed by gating `rename` on the originating document
   being an editable project file: added `Index::is_project_path` (a path with an
   occurrences entry — project files always have one, JAR/dir-lib paths never do)
   and a guard that bails with "cannot rename from a library file".
   Definition/references/hover still work from library buffers (project-wins
   navigation is intended there); only the mutating `rename` is restricted.
   Regression test: `test_e2e_rename_rejected_from_library_file`.
2. **Project paths containing `!/` misrouted as JAR URIs.** The refactor replaced
   `location_for`'s `SymbolSource`-based dispatch with bare `!/` sniffing, so a
   real path under a directory named e.g. `work!` would be emitted as a bogus
   `jar:` URI, breaking navigation. Fixed by detecting JAR virtual paths on their
   actual construction shape `.jar!/` rather than a bare `!/`
   (`split_jar_virtual_path`). Source metadata can't be used uniformly here — the
   references occurrences path arrives without a `SymbolSource`. Regression tests:
   `real_path_with_bang_slash_is_not_a_jar`, `jar_under_bang_dir_still_splits_at_archive`.

A third pass downgraded the residual edge to P3 and confirmed the behavior +
tests are correct: a real path under a directory named literally `<name>.jar!`
is still string-indistinguishable from a JAR entry. Accepted as a documented
limitation (see `split_jar_virtual_path`) rather than fixed — the alternatives
are strictly worse: filesystem probing would break round-tripping for archives
not currently on disk (and the unit tests' synthetic paths), and source-metadata
threading can't reach the path-only references-occurrences call site.
