# let-go / lgx Deps Resolver Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Index and navigate let-go projects — their `.lg`/`.cljc`/`.clj` files and their lgx-managed git/local dependencies — so go-to-definition, hover, completion, and references work across a let-go project and its libraries.

**Tech Stack:** Rust, tower-lsp, tree-sitter (tree-sitter-clojure parses `.lg`), `edn-rs` (new) for `lgx.edn` parsing, existing index/scanner infrastructure.

---

## Design

A resolved lgx dependency is just a source directory, so this reuses the
existing `SymbolSource::Dir` library path (the same one git/`:local/root`
Clojure deps use) rather than adding a new indexing mechanism. Dep files are
navigated via plain `file:` URIs — no `jar:` machinery — so this works in plain
vscode-languageclient too. The work is: recognize let-go projects, resolve
`lgx.edn` deps into source dirs, and teach the file layer about `.lg`.

### Dependency resolution (`src/lgx.rs`, new)

`resolve(project_root) -> Vec<PathBuf>` returns dependency source dirs via a
breadth-first, first-wins walk (mirroring lgx semantics):

1. Parse the project's `lgx.edn` `:deps` map (via `edn-rs`). Each coord is
   either a **git** coord (`:git/url` + `:git/sha` or `:git/tag`) or a
   **local** coord (`:local/root`), each optionally with `:deps/root`.
2. Map each coord to a directory:
   - Git: `$LGX_HOME/gitlibs/<url-sans-scheme-sans-.git>/<ref>` where
     `<ref>` is the sha verbatim, or the tag with `/` replaced by `_`.
     `$LGX_HOME` defaults to `~/.lgx`. Example:
     `https://github.com/nooga/let-go` + sha `46ce…` →
     `~/.lgx/gitlibs/github.com/nooga/let-go/46ce…`.
   - Local: `:local/root` resolved relative to the dep's own root (or
     absolute).
3. The source dir is `<dir>/<deps-root>`, where `deps-root` is `:deps/root`
   if given, else `src` when `<dir>/src` exists, else `<dir>` (repo root).
4. Read that dep's own `lgx.edn` (if present) and recurse into its `:deps`.
   Resolution is breadth-first; the first coord seen for a given lib name
   wins, later differing coords are skipped.

Missing `~/.lgx/gitlibs`, an unfetched dep dir, or malformed `lgx.edn` →
log a warning and skip that dep (same posture as a missing `.cpcache`).

### Project recognition (`src/config.rs`)

- `find_project_root` also stops at a directory containing `lgx.edn`.
- New `project_kind(root) -> ProjectKind` returns `LetGo` when `lgx.edn`
  exists (preferred), else `Clojure`.
- `source_paths` reads `:paths` from `lgx.edn` (via `edn-rs`) for let-go
  projects, else the existing `deps.edn` logic. Fallback `src`/`test`.

### Extensions (`src/index/scanner.rs`)

Add `lg` to the source-extension filter in `collect_clojure_files`, which
covers both the project scan and dep-dir indexing. Dep dirs flow through the
existing `index_classpath_libs` / `index_classpath_dir` unchanged.

### Server wiring (`src/server.rs`)

- The background library-indexing task (in `initialize`) and the
  classpath-changed branch of `did_change_watched_files` choose by
  `project_kind`: `LetGo` → `lgx::resolve(root)` fed into
  `index_classpath_libs(root, dirs, index)`; `Clojure` → existing
  `classpath::discover`.
- Add `.lg` to `did_open` indexing, the `workspace/didChangeWatchedFiles`
  glob patterns, and re-index on `lgx.edn` change (mirroring `deps.edn`).

### Deferred (out of scope here)

let-go's built-in `core`/stdlib `.lg` files live inside the runtime
(`../let-go/pkg/rt/core`), not in the project or `~/.lgx`, so navigating into
them needs a separate discovery mechanism. Deferred to a follow-up; this plan
covers project + git/local deps only.

## File Structure

- Create: `src/lgx.rs` — `lgx.edn` parsing + dep resolution (`resolve`,
  `lgx_home`, git-URL→path, ref encoding, transitive first-wins) + unit tests.
- Modify: `src/lib.rs`, `src/main.rs` — register `lgx` module.
- Modify: `Cargo.toml` — add `edn-rs`.
- Modify: `src/config.rs` — `lgx.edn` in `find_project_root`, `project_kind`,
  `source_paths` from `lgx.edn`.
- Modify: `src/index/scanner.rs` — add `lg` extension.
- Modify: `src/server.rs` — branch lib indexing on project kind; `.lg` in
  `did_open` / watched globs; `lgx.edn` re-index.
- Modify: `tests/test_e2e.rs` — let-go navigation e2e.
- Create: `tests/fixtures/letgo_project/` and a fixture gitlibs dir — e2e
  fixtures.
- Modify: `docs/ROADMAP.md` — mark the lgx resolver landed.

## Implementation Steps

### Task 1: lgx.edn deps parsing

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lgx.rs`
- Modify: `src/lib.rs`, `src/main.rs`

- [x] **Step 1: Add `edn-rs` and register the module**
  Add `edn-rs` to `[dependencies]`; add `pub mod lgx;` to `lib.rs` and
  `mod lgx;` to `main.rs`. Define `Coord` (Git{ url, reff } | Local{ root })
  and a `Dep { coord, deps_root: Option<String> }`, plus
  `parse_deps(edn: &str) -> Vec<(String, Dep)>` returning `(lib-name, dep)`
  pairs (lib name is the coord symbol). For git coords compute `reff` = sha if
  present, else the tag with `/` → `_`.

- [x] **Step 2: Write focused unit tests**
  In `lgx.rs` `#[cfg(test)]`: parse a `:deps` map covering a `:git/sha` coord,
  a `:git/tag` coord (assert `/`→`_` encoding), a `:local/root` coord, and a
  coord with explicit `:deps/root`; assert lib names and coord fields. Assert
  an absent/empty `:deps` yields an empty vec.

- [x] **Step 3: Run the focused test**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib lgx`
  Expected: parsing tests pass.

- [x] **Step 4: Commit**
  Run: `git add -A && git commit -m "Parse lgx.edn :deps coords"`

### Task 2: Dependency resolution

**Files:**
- Modify: `src/lgx.rs`

- [x] **Step 1: Implement resolution**
  Add `lgx_home()` (`$LGX_HOME` else `~/.lgx`), `gitlib_dir(url, reff)`
  (`lgx_home/gitlibs/<url sans scheme sans .git>/<reff>`), `source_dir(root,
  deps_root)` (explicit `:deps/root`, else `src` if present, else root), and
  `resolve(project_root) -> Vec<PathBuf>` doing the breadth-first, first-wins
  walk that reads each dep's own `lgx.edn` for transitive deps. Warn+skip
  missing dirs.

- [x] **Step 2: Write focused unit tests**
  Build a temp `$LGX_HOME` (override the env) with a fake gitlib checkout
  (`gitlibs/github.com/x/lib/<sha>/src/...`) and a `:local/root` dep dir, plus
  a project `lgx.edn` referencing both; assert `resolve` returns both source
  dirs. Add a transitive case (dep ships its own `lgx.edn` with a further dep)
  and a first-wins conflict case. Assert `deps_root` default picks `src` vs
  repo root.

- [x] **Step 3: Run the focused test**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib lgx`
  Expected: resolution tests pass.

- [x] **Step 4: Commit**
  Run: `git add -A && git commit -m "Resolve lgx git/local deps to source dirs (transitive, first-wins)"`

### Task 3: Project recognition + `.lg` extension

**Files:**
- Modify: `src/config.rs`
- Modify: `src/index/scanner.rs`

- [ ] **Step 1: Write focused unit tests**
  In `config.rs`: `find_project_root` stops at a dir with only `lgx.edn`;
  `source_paths` returns `:paths` parsed from an `lgx.edn`; `project_kind`
  returns `LetGo` for `lgx.edn` and `Clojure` for `deps.edn`. (Scanner `.lg`
  pickup is covered by the Task 5 e2e.)

- [ ] **Step 2: Run the focused test (expect failure)**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib config`
  Expected: new tests fail (not implemented).

- [ ] **Step 3: Implement**
  Add `ProjectKind` + `project_kind`; teach `find_project_root` and
  `source_paths` about `lgx.edn` (parse `:paths` via `edn-rs`). Add `"lg"` to
  the extension check in `scanner::collect_clojure_files`.

- [ ] **Step 4: Run verification**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib config`
  Expected: config tests pass.

- [ ] **Step 5: Commit**
  Run: `git add -A && git commit -m "Recognize let-go projects (lgx.edn) and index .lg files"`

### Task 4: Server wiring

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Implement**
  In the `initialize` library-indexing task, branch on
  `config::project_kind(root)`: `LetGo` → `lgx::resolve(root)` →
  `scanner::index_classpath_libs(root, dirs, index)`; `Clojure` → existing
  `classpath::discover` path. Mirror the branch in the classpath-changed arm
  of `did_change_watched_files`, treating an `lgx.edn` change like `deps.edn`
  (re-resolve project paths + deps). Add `.lg` to `did_open` indexing and to
  the `didChangeWatchedFiles` glob patterns (and an `lgx.edn` watcher).

- [ ] **Step 2: Run full check**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo build && bb check`
  Expected: builds; fmt/clippy clean; all existing tests pass.

- [ ] **Step 3: Commit**
  Run: `git add -A && git commit -m "Index lgx deps for let-go projects on startup and lgx.edn change"`

### Task 5: e2e navigation test

**Files:**
- Create: `tests/fixtures/letgo_project/` (project with `lgx.edn`, a `.lg`
  source using a `:local/root` dep and a gitlib dep), a sibling local dep dir,
  and a fixture gitlibs tree for `LGX_HOME`.
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Add fixtures + test**
  Create a let-go project whose `lgx.edn` has `:paths ["src"]` and two deps: a
  `:local/root` dep and a `:git/sha` dep resolvable under a fixture
  `LGX_HOME`. The project `.lg` requires both and uses a symbol from each.
  Add an e2e test that starts the server with `LGX_HOME` pointed at the
  fixture gitlibs dir, opens the project `.lg`, waits for
  `library indexing complete`, and asserts go-to-definition jumps into the
  dep `.lg` files (plain `file:` URIs). The `LspClient` may need to set an env
  var on spawn — extend `start` if required.

- [ ] **Step 2: Run e2e**
  Run: `bb e2e`
  Expected: the new test passes with the suite.

- [ ] **Step 3: Run the editor-client e2e**
  Run: `bb e2e-nvim`
  Expected: passes.

- [ ] **Step 4: Commit**
  Run: `git add -A && git commit -m "e2e: navigate a let-go project into its lgx deps"`

### Task 6: Roadmap + final verification

**Files:**
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Mark the item landed**
  Update the Phase 5 "let-go support with lgx deps resolver" entry (note
  let-go core navigation remains deferred).

- [ ] **Step 2: Final verification**
  Run: `bb check && bb e2e`
  Expected: green.

- [ ] **Step 3: Commit**
  Run: `git add -A && git commit -m "Mark lgx deps resolver landed in roadmap"`
