# clj-kondo `:lint-as` config support Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make names defined by custom def-like macros (e.g. `(defcomponent interactions …)`) navigable, by reading a `:lint-as` map from `.clj-pulse/config.edn` (primary) merged over `.clj-kondo/config.edn`, and feeding it to the extractor.

**Tech Stack:** Rust, tree-sitter (`tree-sitter-clojure`), `edn_format`, tower-lsp.

---

## Design

### Problem

clj-pulse treats a list head it does not recognize as a plain call. So
`(defcomponent interactions [deps…])` records `interactions` as an argument
occurrence, never as a definition. Goto-def, hover, references, and the document
outline therefore miss every name a custom macro introduces. clj-kondo already
solves this with `:lint-as`, which projects declare in `.clj-kondo/config.edn`
(flockman maps `defcomponent/defcomponent` to `clojure.core/def`). We read that
map and act on it. We do not run clj-kondo, and we do not expand macros.

Scope is deliberately narrow (decided during brainstorming):

- Read `.clj-kondo/config.edn` only (no `config/` directory, no JAR-exported
  configs, no `~/.config` global). Those are future work.
- Consume `:lint-as` only. Other clj-kondo config (linter levels, etc.) is out.
- Load config once at project-index time. A config change needs a server
  restart. Live reload is future work.
- Apply `:lint-as` to project files only; library (JAR) indexing keeps the
  default empty config, so jar-cache output is unchanged.

### Data model

A single resolved struct is the only thing the extractor sees:

```rust
// src/index/mod.rs, next to DefKind
#[derive(Debug, Clone, Default)]
pub struct ExtractConfig {
    /// macro fqn ("defcomponent/defcomponent") -> the core def-form it acts like
    pub lint_as: HashMap<String, DefKind>,
}
```

`Default` (empty `lint_as`) reproduces today's behavior exactly. The extractor
never reads files or EDN; it only borrows an `&ExtractConfig`.

### Config layer

Two new flat modules, matching the `leiningen.rs` / `lgx.rs` / `classpath.rs`
style (a pure parse/merge function plus a thin file-reading wrapper):

- `src/kondo.rs` - the clj-kondo compatibility boundary. Reads
  `.clj-kondo/config.edn` and returns the raw `:lint-as` pairs.
  - `parse_lint_as(edn: &str) -> Vec<(String, String)>` (pure, unit-tested).
  - `lint_as(root: &Path) -> Vec<(String, String)>` (reads the file, returns
    empty on missing/unparseable input).
- `src/settings.rs` - clj-pulse's own settings and the merge into
  `ExtractConfig`.
  - `parse_lint_as(edn: &str) -> Vec<(String, String)>` for
    `.clj-pulse/config.edn` (same `{:lint-as {sym sym}}` shape, mirrored for
    familiarity).
  - `load(root: &Path) -> ExtractConfig`:
    1. `kondo::lint_as(root)` -> base pairs.
    2. `.clj-pulse/config.edn` pairs -> overlay, **clj-pulse wins per key**.
    3. Map each target fqn to a `DefKind` by its symbol name through the
       existing def-symbol mapping. Targets that are not def-like
       (`clojure.core/for`, `clojure.core/->`, …) map to `None` and are
       dropped, with a debug log. So flockman's config yields exactly
       `{"defcomponent/defcomponent": DefKind::Def}`.

Dependency direction: `settings` depends on `kondo` and on `index` types; the
extractor depends only on `index::ExtractConfig`.

### Extractor integration

A small shared helper resolves a list head to its fqn the way clj-kondo sees it:

```
resolve_head_fqn(head, ns_meta, source) -> Option<String>
  qualified -> alias-resolve  (comp/defcomponent -> defcomponent/defcomponent)
  bare      -> ns_meta.refers, else current-ns/name
```

This relies on the `ns` form being processed before the def form, which is
already true (the `ns` form is first) and is the same assumption the rest of the
extractor makes.

**Symbol pass** (`process_top_level_list`): when `str_to_defkind(head_text)` is
`None`, fall back to `cfg.lint_as.get(resolve_head_fqn(...))`. If it yields a
`DefKind`, call the existing `extract_def(...)` with that kind. `interactions`
becomes a normal `Symbol { kind: Def, fqn: "flockman.interactions/interactions" }`,
reusing all current def logic.

**Occurrence pass** (`walk_list`): before the core-form match, look up the same
`cfg.lint_as` on the resolved head. If def-like:

```
record_occurrence(head)        // keep `defcomponent` navigable when its lib is indexed
walk_def_form(kind, children)  // name is a def (not an occurrence); body args are usages
return
```

Net effect on that form: `interactions` flips from occurrence to definition; the
head stays an occurrence; the dependency args stay usages. `OccurrenceCtx` gains
a `lint_as: &'a HashMap<String, DefKind>` field alongside `source` / `ns_meta` /
`def_names`.

### Wiring (config reaches the extractor by parameter)

- `extract_full(source, file, cfg: &ExtractConfig)` and
  `file_occurrences(source, path, cfg)` gain the parameter. `extract(source,
  file)` stays as a two-argument wrapper that passes `&ExtractConfig::default()`,
  so its callers (diagnostics, code_action, JAR indexing, library dir indexing,
  tests) are untouched.
- `scanner::build_index(root, source_paths, cfg: &ExtractConfig)` gains the
  parameter and forwards `cfg` to each `extract_full`. It builds a *fresh*
  `Index`, which is then merged into the persistent index via
  `Index::merge_project_from` (server.rs:200, 519) - so `cfg` is passed in, not
  read from the throwaway index.
- The *persistent* `Index` (defined in `src/index/mod.rs:108`) holds the config
  and exposes `extract_config(&self) -> &ExtractConfig` (the value set at
  startup, or a `LazyLock` default). `server.rs` `initialize` calls
  `settings::load(root)`, sets it on the index once, then passes
  `index.extract_config()` into `build_index`.
- Sites that pass `index.extract_config()`:
  - `build_index` - called from `initialize` (server.rs:195) and the
    `did_change_watched_files` project rebuild (server.rs:517).
  - the per-file re-extraction handlers `did_open` (server.rs:361), `did_save`
    (server.rs:403), and the single changed-file branch of
    `did_change_watched_files` (server.rs:492). (`did_change`, server.rs:538,
    only re-lints and does not extract symbols - untouched.)
  - `references` (re-extraction and `file_occurrences` for goto-def /
    references / rename).
  - `symbols` (document outline) - switches from `extract` to
    `extract_full(.., cfg)`, discarding occurrences.
- Sites that stay on the default two-argument `extract` (libraries are **not**
  lint-as'd in v1): JAR indexing (`jar.rs:47`) and library source-directory
  indexing (`scanner::index_classpath_dir`, scanner.rs:135). Because their
  output is unchanged, **`JarCacheEntry::format_version` needs no bump.**
  `diagnostics`, `code_action`, and tests also stay on the wrapper.

### Error handling

Missing or malformed config files yield an empty `ExtractConfig`; the server
behaves exactly as today. This matches the extractor's existing rule: on parse
failure, log and continue, never crash.

### Testing strategy

- Unit tests for `kondo::parse_lint_as` and `settings` (merge precedence,
  def-like filtering, empty input).
- Extractor unit tests proving a lint-as'd form defines its name, keeps the head
  as an occurrence, and does not record the def-site name as an occurrence; plus
  a negative test that an empty config preserves today's behavior.
- An end-to-end test (`bb e2e`) driving real `textDocument/definition` over a
  fixture project whose `.clj-kondo/config.edn` declares a `:lint-as` mapping.
  Per CLAUDE.md, server behavior changes are not done until `bb e2e` passes.

## File Structure

**Created:**
- `src/kondo.rs` - read `.clj-kondo/config.edn` `:lint-as` (compat boundary).
- `src/settings.rs` - read `.clj-pulse/config.edn`, merge, build `ExtractConfig`.
- `tests/fixtures/lint_as_project/` - e2e fixture (`.clj-kondo/config.edn` + a
  source file using a lint-as'd macro).

**Modified:**
- `src/index/mod.rs` - add `ExtractConfig`; add a `pub(crate)` def-symbol →
  `DefKind` mapping reused by the extractor and `settings`; add the persistent
  `Index` (defined here, line 108) `extract_config` field, accessor, and
  one-time setter.
- `src/index/extractor.rs` - thread `&ExtractConfig`; add `resolve_head_fqn`;
  lint-as branches in `process_top_level_list` and `walk_list`; `OccurrenceCtx`
  field; keep the two-argument `extract` wrapper.
- `src/index/scanner.rs` - `build_index` gains a `cfg: &ExtractConfig` parameter
  forwarded to `extract_full`. `index_classpath_dir` (library dirs) is unchanged.
- `src/server.rs` - load + set config in `initialize`; pass `cfg` to
  `build_index` (lines 195, 517) and the three per-file `extract_full` handlers
  (361, 403, 492).
- `src/handlers/references.rs`, `src/handlers/symbols.rs` - pass
  `index.extract_config()`.
- crate root - declare the modules in **both** `src/main.rs` (`mod kondo;`,
  `mod settings;`) and `src/lib.rs` (`pub mod kondo;`, `pub mod settings;`),
  mirroring how `leiningen`/`lgx` are declared in both.
- `tests/test_e2e.rs` - new lint-as definition test.
- `README.md`, `docs/ROADMAP.md` - document the feature.

`src/index/jar.rs` needs **no** change: its `extract` call (jar.rs:47) already
uses the two-argument wrapper, which keeps passing the default.

---

## Task 1: clj-kondo config reader (`src/kondo.rs`)

**Files:**
- Create: `src/kondo.rs`
- Modify: crate root module list (`src/lib.rs` or `src/main.rs`)

- [ ] **Step 1: Write the failing test**
  In `src/kondo.rs` `#[cfg(test)]`, test `parse_lint_as`: given
  `{:lint-as {defcomponent/defcomponent clojure.core/def
   plumbing.core/for-map clojure.core/for}}`, it returns both pairs as
  `(String, String)` (fully-qualified macro symbol, fully-qualified target).
  Add cases: empty/missing `:lint-as` returns `[]`; non-map input returns `[]`.

- [ ] **Step 2: Run test to verify it fails**
  Run: `cargo test kondo`
  Expected: FAIL (module/function does not exist).

- [ ] **Step 3: Write minimal implementation**
  Implement `parse_lint_as(edn: &str) -> Vec<(String, String)>` using
  `edn_format::parse_str` and the `edn.rs` helpers (`get`, `kw`), reading the
  top-level `:lint-as` map and stringifying each symbol key/value (preserve
  `ns/name`). Implement `lint_as(root: &Path) -> Vec<(String, String)>` that
  reads `root/.clj-kondo/config.edn` and calls `parse_lint_as`, returning `[]`
  on any read/parse error. Declare `mod kondo;` in the crate root.

- [ ] **Step 4: Run test to verify it passes**
  Run: `cargo test kondo`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: read :lint-as from .clj-kondo/config.edn"`

## Task 2: `ExtractConfig` type and settings loader (`src/settings.rs`)

**Files:**
- Modify: `src/index/mod.rs`
- Create: `src/settings.rs`
- Modify: crate root module list

- [ ] **Step 1: Add the `ExtractConfig` type and the def-symbol mapping**
  In `src/index/mod.rs`, add `ExtractConfig { lint_as: HashMap<String, DefKind> }`
  with `derive(Debug, Clone, Default)`. Add a `pub(crate)` mapping from a
  def-symbol name to `DefKind` (extract the match arms currently in
  `extractor::str_to_defkind` into a shared function, or expose `str_to_defkind`
  as `pub(crate)`), so both the extractor and `settings` use one source of truth.

- [ ] **Step 2: Write the failing test**
  In `src/settings.rs` `#[cfg(test)]`, test the pure merge function: given
  kondo pairs `[("defcomponent/defcomponent","clojure.core/def"),
  ("p/for-map","clojure.core/for")]` and clj-pulse pairs
  `[("defcomponent/defcomponent","clojure.core/defn")]`, the result
  `lint_as` is `{"defcomponent/defcomponent": Defn}` (clj-pulse overlay wins;
  the `for` target is dropped as not def-like). Add an empty-input case
  yielding an empty map.

- [ ] **Step 3: Run test to verify it fails**
  Run: `cargo test settings`
  Expected: FAIL.

- [ ] **Step 4: Write minimal implementation**
  Implement `parse_lint_as(edn: &str)` for the clj-pulse config (same shape),
  a pure `merge(kondo, clj_pulse) -> ExtractConfig` that overlays clj-pulse over
  kondo per key and maps targets to `DefKind` via the shared mapping (dropping
  `None`, debug-logging drops), and `load(root: &Path) -> ExtractConfig` that
  reads both files and calls `merge`. Declare `mod settings;` in the crate root.

- [ ] **Step 5: Run test to verify it passes**
  Run: `cargo test settings`
  Expected: PASS.

- [ ] **Step 6: Commit**
  `git commit -m "feat: merge .clj-pulse and .clj-kondo :lint-as into ExtractConfig"`

## Task 3: Thread `&ExtractConfig` through the extractor (behavior-preserving)

**Files:**
- Modify: `src/index/extractor.rs`
- Modify: `src/index/scanner.rs`, `src/handlers/references.rs`, `src/server.rs`

- [ ] **Step 1: Add the parameter, default everywhere**
  Change `extract_full(source, file, cfg: &ExtractConfig)` and
  `file_occurrences(source, path, cfg: &ExtractConfig)`. Add the
  `lint_as: &HashMap<String, DefKind>` field to `OccurrenceCtx` and populate it
  from `cfg` (do not consult it yet). Keep `extract(source, file)` as a wrapper
  calling `extract_full(source, file, &ExtractConfig::default())`. Update only
  the callers of `extract_full`/`file_occurrences` to pass
  `&ExtractConfig::default()` for now: inside `scanner::build_index`, the three
  `server.rs` handlers (361, 403, 492), and `references.rs` (102, 170, 176, 236).
  The two-argument `extract` callers (`jar`, `diagnostics`, `code_action`,
  `symbols`, `index_classpath_dir`, tests) are untouched.

- [ ] **Step 2: Verify no behavior change**
  Run: `bb check`
  Expected: PASS (all existing tests green; this is a pure refactor).

- [ ] **Step 3: Commit**
  `git commit -m "refactor: thread &ExtractConfig through the extractor"`

## Task 4: Implement `:lint-as` in the extractor passes

**Files:**
- Modify: `src/index/extractor.rs`

- [ ] **Step 1: Write the failing tests**
  In `src/index/extractor.rs` `#[cfg(test)]` (or `tests/test_extractor.rs`,
  following its style), with `cfg.lint_as = {"my/defthing": Def}` extract:
  `(ns x (:require [my :refer [defthing]])) (defthing foo 1) (inc foo)`.
  Assert: a `Symbol` named `foo` with fqn `x/foo` exists; occurrences include the
  head `my/defthing` and the trailing `foo` as `x/foo`; the def-site `foo` is not
  an occurrence. Add a negative test: with `ExtractConfig::default()`, no `foo`
  symbol is produced.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test extractor`
  Expected: FAIL.

- [ ] **Step 3: Implement the lint-as branches**
  Add `resolve_head_fqn(head, ns_meta, source) -> Option<String>`. In
  `process_top_level_list`, when `str_to_defkind` is `None`, fall back to
  `cfg.lint_as.get(&resolve_head_fqn(...))` and call `extract_def` with that
  kind. Thread `cfg` into `process_top_level_list` and
  `process_reader_conditional`. In `walk_list`, before the core-form match, if
  the resolved head is in `ctx.lint_as`, `record_occurrence(head)` then
  `walk_def_form(kind, …)` and return.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test extractor`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: extract names defined by :lint-as def-like macros"`

## Task 5: Load config at startup and wire it into `Index`

**Files:**
- Modify: `src/index/mod.rs` (the persistent `Index`, line 108)
- Modify: `src/index/scanner.rs` (`build_index` signature)
- Modify: `src/server.rs`
- Modify: `src/handlers/references.rs`, `src/handlers/symbols.rs`

- [ ] **Step 1: Add config storage to `Index`**
  In `src/index/mod.rs`, add an `extract_config` field to `Index` (an
  `OnceLock<ExtractConfig>` or equivalent), a one-time setter, and
  `pub fn extract_config(&self) -> &ExtractConfig` that returns the set value or
  a `LazyLock` default. The throwaway index from `build_index` does not use this
  field; only the persistent index (the one merged into) carries it.

- [ ] **Step 2: Give `build_index` a `cfg` parameter**
  Change `scanner::build_index(root, source_paths, cfg: &ExtractConfig)` to
  forward `cfg` to each `extract_full`.

- [ ] **Step 3: Load + set config, then pass it everywhere**
  In `server.rs` `initialize`, call `settings::load(&root_path)` and set it on
  the (persistent, `Arc`-shared) `index` before `build_index`. Pass
  `index.extract_config()` to `build_index` at both sites (lines 195 and 517 -
  the second already has the loaded config from initialize). Switch the per-file
  `extract_full` handlers `did_open` (361), `did_save` (403), and the changed-
  file branch (492) to `self.index.extract_config()`. Switch `references`
  (re-extraction + `file_occurrences`) and `symbols` (document outline, now
  calling `extract_full(.., cfg)`) to `index.extract_config()`. Leave `jar.rs`
  and `index_classpath_dir` on the default wrapper.

- [ ] **Step 4: Verify**
  Run: `bb check`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: load :lint-as config at startup and apply to project files"`

## Task 6: End-to-end definition test

**Files:**
- Create: `tests/fixtures/lint_as_project/.clj-kondo/config.edn`
- Create: `tests/fixtures/lint_as_project/deps.edn` (minimal, so it is detected
  as a Clojure project)
- Create: `tests/fixtures/lint_as_project/src/app/core.clj`
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Write the fixture and the failing test**
  Fixture `.clj-kondo/config.edn`: `{:lint-as {app.macros/defthing clojure.core/def}}`.
  `src/app/core.clj`: requires `[app.macros :refer [defthing]]`, defines
  `(defthing widget 1)`, and later references `widget`. Add an e2e test that
  copies the fixture via `setup_named("lint_as_project")` (note: `setup_project()`
  hardcodes `simple_project`; `copy_dir` uses `read_dir`, so dotfiles like
  `.clj-kondo/config.edn` are copied into the temp root), then `initialize`,
  `wait_for_log("Indexed")`, `did_open`, sends `textDocument/definition` on the
  `widget` usage, and asserts the response location is the `defthing widget`
  line.

- [ ] **Step 2: Run to verify it fails (if run before Task 5 landed) or passes**
  Run: `bb e2e`
  Expected: PASS (Tasks 4-5 implement the behavior; if iterating, a pre-Task-5
  run FAILs by resolving nothing).

- [ ] **Step 3: Commit**
  `git commit -m "test: e2e goto-def on a :lint-as defined name"`

## Task 7: Documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Document configuration**
  Add a short README subsection (use `/writing-clearly`): clj-pulse reads
  `:lint-as` from `.clj-pulse/config.edn` (primary) and `.clj-kondo/config.edn`
  (merged, clj-pulse wins), so names defined by custom def-like macros become
  navigable; note the recommended gitignore split for `.clj-pulse/` (commit
  `config.edn`, ignore `jar-cache/` and `*.log`). Add a `docs/ROADMAP.md` entry
  recording the clj-kondo config-compat layer and what remains (config/ dir,
  JAR-exported configs, live reload, linter-level compat).

- [ ] **Step 2: Commit**
  `git commit -m "docs: document :lint-as config support"`

---

## Conventions

- Run `bb check` (fmt + clippy `-D warnings` + tests) before each commit; `bb e2e`
  after Tasks 5-6.
- Exact file paths; no full code inlined - implement from the descriptions.
- DRY (one def-symbol → `DefKind` mapping), YAGNI (single config file, startup
  load, project-only), TDD, frequent commits.
- Use `/writing-clearly` for all prose.
