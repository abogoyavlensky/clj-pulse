# let-go Core Navigation (clj-pulse side) Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In a let-go (lgx) project that pins `:lg-version`, give definition /
hover / completion / references for let-go's built-in `core` and stdlib by
indexing the let-go source that `lgx install` fetched, navigating into the actual
`.lg` source.

**Tech Stack:** Rust, the existing `Index`/`scanner`/`lgx`/`handlers`
infrastructure, the stdio e2e harness.

---

## Design

This is the editor half of the let-go-version feature; the lgx side (which
fetches the source) is done. lgx places the source at
`$LGX_HOME/let-go/source/<version>/`, with let-go's core/stdlib under
`pkg/rt/core/*.lg` (`core.lg` → ns `core`, `string.lg` → ns `string`, etc.).

### Trigger & sourcing

- Only for `ProjectKind::LetGo`. Read `:lg-version` from `lgx.edn`
  (`lgx::lg_version(edn) -> Option<String>`, parsed like `lgx::paths`).
- Core dir = `<lgx_home>/let-go/source/<version>/pkg/rt/core` (reuse
  `lgx::lgx_home()`). If `:lg-version` is absent or the dir doesn't exist, do
  nothing — clj-pulse degrades to today's behavior.
- Index the dir as a dir-lib via the existing `scanner::index_dir_libs`, which
  extracts `.lg`, tags `SymbolSource::Dir`, and navigates via `file:` URIs. So
  `core.lg`/`string.lg`/etc. land in the index with no new extraction code, and
  navigation lands on the real `.lg` source.

### The two let-go-specific resolution pieces (the actual work)

let-go aliases Clojure namespace names to its own (`lang.go` `nsAliases`):

```
clojure.core→core   clojure.string→string  clojure.set→set    clojure.walk→walk
clojure.edn→edn     clojure.zip→zip         clojure.data→data  clojure.pprint→pprint
clojure.test→test
```

1. **`clojure.*` alias registration.** Projects write `[clojure.string :as str]`
   but the source file is ns `string`. After indexing core, for each pair above
   whose let-go ns is indexed, register a **second copy** of that namespace under
   the `clojure.*` name: clone the `NsMeta` (name = `clojure.string`, same
   `file`) and the symbols (ns/fqn rewritten to `clojure.string/...`, same
   `file`/ranges), insert via `Index::insert_lib_file`. Then every existing
   resolver — `lookup`, `ns_meta`, completion, definition — resolves both names
   with **zero handler changes**. Duplication is a few hundred symbols (cheap).

2. **`core` as the auto-referred builtin.** Bare `map`/`when` in a let-go file
   must resolve to `core/map` in `core.lg`, **not** the static `clojure.core`
   list (which would mis-navigate to a clojure jar absent from a let-go
   classpath). Add a marker on `Index` (`letgo_core: AtomicBool`, set when let-go
   core is indexed — interior mutability because the `Arc<Index>` is already
   shared when background indexing runs). In `resolve_symbol`'s bare-word
   fallback: when the marker is set, resolve via `lookup_in_ns("core", word)` and
   **do not** fall through to the static clojure.core list (let-go projects don't
   use clojure.core). Go-only primitives (`+`, `apply*`) aren't in `core.lg`, so
   they simply don't navigate — as decided.

### Navigation target

The `.lg` source, handled by the existing `Dir` indexing + `file:` URIs. No Go.

### Error handling / degradation

- `:lg-version` absent, dir missing, or extraction failure → skip; existing
  let-go (lgx dep) navigation is unaffected.
- Concurrent with project + lgx-dep indexing; uses `insert_lib_file` (never
  shadows project symbols), consistent with the invariants.

### Testing

- Unit: `lgx::lg_version` parsing; the alias-duplication + marker over a
  hand-built index.
- e2e: a let-go fixture pinning `:lg-version`, a hand-built
  `$LGX_HOME/let-go/source/<v>/pkg/rt/core/{core.lg,string.lg}` (via
  `start_with_env` `LGX_HOME`), asserting definition on bare `map` → `core.lg`
  and `str/join` (through the `clojure.string` alias) → `string.lg`.

## File Structure

- **Modify `src/lgx.rs`** — `pub fn lg_version(edn: &str) -> Option<String>`;
  `index_letgo_core(root, index)` (resolve dir, index it, alias-duplicate, set
  marker). Reuse `lgx_home`, `scanner::index_dir_libs`, the `edn` helpers.
- **Modify `src/index/mod.rs`** — `letgo_core: AtomicBool` on `Index` (default
  false); keep `insert_lib_file` for the duplicate registration.
- **Modify `src/handlers/mod.rs`** — `resolve_symbol` bare-word fallback prefers
  the let-go `core` ns when the marker is set (and skips the static core list).
- **Modify `src/server.rs`** — call `lgx::index_letgo_core(root, index)` in the
  `ProjectKind::LetGo` branch of `resolve_and_index_libs`, after lgx deps.
- **Modify `tests/test_e2e.rs`** — let-go core-navigation e2e.

Reuse `scanner::index_dir_libs`, `Index::{insert_lib_file, lookup_in_ns,
ns_symbols, lookup}`, `lgx::{lgx_home, paths}` patterns, the `edn` module, and
`start_with_env`. No new dependencies.

---

## Tasks

### Task 1: `lgx::lg_version` parser

**Files:** Modify `src/lgx.rs`

- [ ] **Step 1: Failing unit test** in `lgx.rs` `#[cfg(test)]`: `lg_version` of
  `{:lg-version "1.10.0" :paths ["src"]}` → `Some("1.10.0")`; of `{:paths …}` →
  `None`; of a blank/non-string value → `None`.
- [ ] **Step 2:** `cargo test --lib lgx::` → FAIL.
- [ ] **Step 3:** Implement `pub fn lg_version(edn: &str) -> Option<String>`
  using the `edn` helpers (mirror `paths`): top-level map → `:lg-version` →
  non-blank string.
- [ ] **Step 4:** `cargo test --lib lgx::` → PASS.
- [ ] **Step 5:** `git commit -m "lgx: parse :lg-version from lgx.edn"`

### Task 2: index let-go core + clojure.* aliases + marker

**Files:** Modify `src/lgx.rs`, `src/index/mod.rs`

- [ ] **Step 1: Failing unit test** (in `lgx.rs` or `index/mod.rs` tests): build
  an `Index`, point it at a temp `…/pkg/rt/core/{core.lg,string.lg}` (ns `core`
  with `(defn map …)`, ns `string` with `(defn join …)`), run the indexing
  helper, then assert: `lookup_in_ns("string","join")` and
  `lookup_in_ns("clojure.string","join")` both resolve to the same file;
  `ns_meta("clojure.core")` exists; `index.letgo_core` is true.
- [ ] **Step 2:** `cargo test --lib` → FAIL.
- [ ] **Step 3:** Implement
  - `Index.letgo_core: AtomicBool` (default false); a `mark_letgo_core(&self)`
    setter and a reader used by `resolve_symbol`.
  - `lgx::index_letgo_core(root, index)`: return early unless `ProjectKind::LetGo`
    and `lg_version` present and the core dir exists; `index_dir_libs(&[core_dir])`;
    then for each `NS_ALIASES` pair whose let-go ns is in `ns_symbols`, clone its
    `NsMeta` (rename) + symbols (rewrite ns/fqn, keep file/ranges) and
    `insert_lib_file` under the `clojure.*` name; `index.mark_letgo_core()`.
  - A `const NS_ALIASES: &[(&str,&str)]` (clojure.* ↔ let-go) in `lgx.rs`.
- [ ] **Step 4:** `cargo test --lib` → PASS.
- [ ] **Step 5:** `git commit -m "Index let-go core source under both let-go and clojure.* names"`

### Task 3: `core` auto-refer in resolve_symbol

**Files:** Modify `src/handlers/mod.rs`

- [ ] **Step 1: Failing unit test** (handlers `#[cfg(test)]`): an `Index` with a
  `core/map` symbol and `letgo_core` set → `resolve_symbol(index,"map","app")`
  resolves to the `core/map` Project symbol (not a Core builtin). With the marker
  unset, behavior is unchanged (still the static core list).
- [ ] **Step 2:** `cargo test --lib handlers` → FAIL.
- [ ] **Step 3:** In the bare-word fallback of `resolve_symbol`, before the
  static clojure.core list: if `index.letgo_core` is set, return
  `lookup_in_ns("core", word)` as a `Project` match (and skip the static list).
- [ ] **Step 4:** `cargo test --lib handlers` → PASS.
- [ ] **Step 5:** `git commit -m "resolve_symbol: let-go core is the bare-word builtin"`

### Task 4: wire into indexing + e2e

**Files:** Modify `src/server.rs`, `tests/test_e2e.rs`

- [ ] **Step 1: Failing e2e** (`tests/test_e2e.rs`): a let-go fixture whose
  `lgx.edn` has `:paths ["src"]` and `:lg-version "0.0.1"`; build
  `<tmp_home>/let-go/source/0.0.1/pkg/rt/core/core.lg` (`(ns core)\n(defn map [f c] …)`)
  and `string.lg` (`(ns string)\n(defn join [sep c] …)`); a project `.lg` file
  `(ns app (:require [clojure.string :as str]))\n(map identity [])\n(str/join "," [])`.
  `start_with_env(root, &[("LGX_HOME", tmp_home)])`, initialize,
  `wait_for_log("library indexing complete")`, `did_open` the app file.
  goto-def on `map` → URI ends `…/pkg/rt/core/core.lg`; goto-def on the `join` of
  `str/join` → `…/pkg/rt/core/string.lg`.
- [ ] **Step 2:** `cargo test --test test_e2e letgo_core` → FAIL.
- [ ] **Step 3:** In `server.rs` `resolve_and_index_libs`, `ProjectKind::LetGo`
  branch: after `index_dir_libs(&dirs, index)`, call
  `lgx::index_letgo_core(root, index)`. Ensure the "library indexing complete"
  log still fires.
- [ ] **Step 4:** `cargo test --test test_e2e letgo_core`, then `bb check && bb e2e`
  → PASS.
- [ ] **Step 5:** `git commit -m "e2e: navigate into let-go core/stdlib from a pinned project"`

### Task 5: ROADMAP note

**Files:** Modify `docs/ROADMAP.md`

- [ ] **Step 1:** Check the "let-go core navigation" Phase 5 item, noting
  navigation works when `:lg-version` is pinned and lgx has fetched the source.
- [ ] **Step 2:** `git commit -m "Roadmap: note let-go core navigation"`

---

## Notes & limitations

- **Opt-in via `:lg-version`.** Unpinned let-go projects get no core nav (by
  design — lgx only fetches source when pinned).
- **Source must be fetched.** Requires a prior `lgx install` (or the dir present);
  otherwise skip. A version mismatch between the fetched source and the running
  `lg` is lgx's concern (its compat check), not clj-pulse's.
- **Go primitives don't navigate** (`+`, `apply*`, special forms) — they have no
  `.lg` definition. Same stance as clojure special forms.
- **Cache-version note:** none — clj-pulse reads dir-libs live (no jar cache for
  source dirs), so no `JarCacheEntry::format_version` bump is needed.
