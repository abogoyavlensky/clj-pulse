# let-go Core Navigation (clj-pulse side) Implementation Plan

> **Status: ✅ Completed (2026-06-17).** All five tasks implemented, reviewed
> (codex second-opinion per task), and verified with `bb check` + `bb e2e`. See
> the [Implementation summary](#implementation-summary) at the end.

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

- [x] **Step 1: Failing unit test** in `lgx.rs` `#[cfg(test)]`: `lg_version` of
  `{:lg-version "1.10.0" :paths ["src"]}` → `Some("1.10.0")`; of `{:paths …}` →
  `None`; of a blank/non-string value → `None`.
- [x] **Step 2:** `cargo test --lib lgx::` → FAIL.
- [x] **Step 3:** Implement `pub fn lg_version(edn: &str) -> Option<String>`
  using the `edn` helpers (mirror `paths`): top-level map → `:lg-version` →
  non-blank string.
- [x] **Step 4:** `cargo test --lib lgx::` → PASS.
- [x] **Step 5:** `git commit -m "lgx: parse :lg-version from lgx.edn"`

### Task 2: index let-go core + clojure.* aliases + marker

**Files:** Modify `src/lgx.rs`, `src/index/mod.rs`

- [x] **Step 1: Failing unit test** (in `lgx.rs` or `index/mod.rs` tests): build
  an `Index`, point it at a temp `…/pkg/rt/core/{core.lg,string.lg}` (ns `core`
  with `(defn map …)`, ns `string` with `(defn join …)`), run the indexing
  helper, then assert: `lookup_in_ns("string","join")` and
  `lookup_in_ns("clojure.string","join")` both resolve to the same file;
  `ns_meta("clojure.core")` exists; `index.letgo_core` is true.
- [x] **Step 2:** `cargo test --lib` → FAIL.
- [x] **Step 3:** Implement
  - `Index.letgo_core: AtomicBool` (default false); a `mark_letgo_core(&self)`
    setter and a reader used by `resolve_symbol`.
  - `lgx::index_letgo_core(root, index)`: return early unless `ProjectKind::LetGo`
    and `lg_version` present and the core dir exists; `index_dir_libs(&[core_dir])`;
    then for each `NS_ALIASES` pair whose let-go ns is in `ns_symbols`, clone its
    `NsMeta` (rename) + symbols (rewrite ns/fqn, keep file/ranges) and
    `insert_lib_file` under the `clojure.*` name; `index.mark_letgo_core()`.
  - A `const NS_ALIASES: &[(&str,&str)]` (clojure.* ↔ let-go) in `lgx.rs`.
  - Codex review: gated alias-duplication on the source file living under the
    core dir (don't alias a project/dep ns that shares a bare name); used
    `std::slice::from_ref` to satisfy clippy `-D warnings`.
- [x] **Step 4:** `cargo test --lib` → PASS.
- [x] **Step 5:** `git commit -m "Index let-go core source under both let-go and clojure.* names"`

### Task 3: `core` auto-refer in resolve_symbol

**Files:** Modify `src/handlers/mod.rs`

- [x] **Step 1: Failing unit test** (handlers `#[cfg(test)]`): an `Index` with a
  `core/map` symbol and `letgo_core` set → `resolve_symbol(index,"map","app")`
  resolves to the `core/map` Project symbol (not a Core builtin). With the marker
  unset, behavior is unchanged (still the static core list). (Also added a third
  test: marker set + builtin missing from core → resolves to `None`, never the
  static list.)
- [x] **Step 2:** `cargo test --lib handlers` → FAIL.
- [x] **Step 3:** In the bare-word fallback of `resolve_symbol`, before the
  static clojure.core list: if `index.letgo_core` is set, return
  `lookup_in_ns("core", word)` as a `Project` match (and skip the static list).
- [x] **Step 4:** `cargo test --lib handlers` → PASS.
- [x] **Step 5:** `git commit -m "resolve_symbol: let-go core is the bare-word builtin"`

### Task 4: wire into indexing + e2e

**Files:** Modify `src/server.rs`, `tests/test_e2e.rs`

- [x] **Step 1: Failing e2e** (`tests/test_e2e.rs`): a let-go fixture whose
  `lgx.edn` has `:paths ["src"]` and `:lg-version "0.0.1"`; build
  `<tmp_home>/let-go/source/0.0.1/pkg/rt/core/core.lg` (`(ns core)\n(defn map [f c] …)`)
  and `string.lg` (`(ns string)\n(defn join [sep c] …)`); a project `.lg` file
  `(ns app (:require [clojure.string :as str]))\n(map identity [])\n(str/join "," [])`.
  `start_with_env(root, &[("LGX_HOME", tmp_home)])`, initialize,
  `wait_for_log("library indexing complete")`, `did_open` the app file.
  goto-def on `map` → URI ends `…/pkg/rt/core/core.lg`; goto-def on the `join` of
  `str/join` → `…/pkg/rt/core/string.lg`. (Fixture committed at
  `tests/fixtures/letgo_core_project`; core source built in-test under LGX_HOME.)
- [x] **Step 2:** `cargo test --test test_e2e letgo_core` → FAIL (timeout: log never fired).
- [x] **Step 3:** In `server.rs` `resolve_and_index_libs`, `ProjectKind::LetGo`
  branch: after `index_dir_libs(&dirs, index)`, call
  `lgx::index_letgo_core(root, index)`. Ensure the "library indexing complete"
  log still fires. (Done by adding `index_letgo_core`'s namespace count to
  `dirs.len()` so a deps-less pinned project is still non-zero.)
- [x] **Step 4:** `cargo test --test test_e2e letgo_core`, then `bb check && bb e2e`
  → PASS.
  - Codex review: reset the `letgo_core` marker in `Index::clear_libs` so a
    watched `lgx.edn` change that un-pins `:lg-version` (or removes the source)
    restores the static clojure.core fallback instead of leaving navigation
    broken until restart; added a unit test.
- [x] **Step 5:** `git commit -m "e2e: navigate into let-go core/stdlib from a pinned project"`

### Task 5: ROADMAP note

**Files:** Modify `docs/ROADMAP.md`

- [x] **Step 1:** Check the "let-go core navigation" Phase 5 item, noting
  navigation works when `:lg-version` is pinned and lgx has fetched the source.
  (Also updated the lgx-support item's "still deferred" note for consistency.)
- [x] **Step 2:** `git commit -m "Roadmap: note let-go core navigation"`

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

## Implementation summary

Implemented as designed, in five commits on `lg-core-navigation`:

1. **`dbaddfa`** — `lgx::lg_version(edn)` parser (mirrors `paths`).
2. **`eeff83b`** — `Index.letgo_core` (`AtomicBool`) marker + `mark_letgo_core`/
   reader; `lgx::index_letgo_core` (resolve `$LGX_HOME/let-go/source/<version>/
   pkg/rt/core`, `index_dir_libs` it, duplicate each indexed stdlib ns under its
   `clojure.*` alias, set the marker); `NS_ALIASES` table.
3. **`6ccf57e`** — `resolve_symbol` bare-word fallback resolves via the let-go
   `core` ns when the marker is set, skipping the static clojure.core list.
4. **`112c5c8`** — wired `index_letgo_core` into the `ProjectKind::LetGo`
   indexing branch (its namespace count is added so a deps-less pinned project
   still logs "library indexing complete"); e2e fixture + test.
5. **`96de9ae`** — ROADMAP note.

**Design choices the plan left open.** `index_letgo_core` returns the count of
core namespaces indexed (so the "nothing to index" guard stays correct with no
deps); a private `index_core_dir(core_dir, index)` helper takes the dir
explicitly so the unit test stays hermetic (no `LGX_HOME` env mutation).

**Findings fixed during the per-task codex reviews.**

- *Alias duplication could clone an unrelated namespace* that merely shares a
  bare name (`core`/`string`/`test`) from project or dependency code. Fixed:
  `register_alias_copy` only clones a namespace whose source file lives under
  the (canonicalized) core dir. Regression test added.
- *Clippy `-D warnings`* (`cloned_ref_to_slice_refs`): switched
  `&[core_dir.clone()]` to `std::slice::from_ref`.
- *Stale marker across re-index*: the `letgo_core` latch never reset, so
  un-pinning `:lg-version` (or deleting the source) mid-session left bare-word
  navigation broken until restart. Fixed by resetting the marker in
  `Index::clear_libs` (always run on re-index, regardless of project kind);
  `index_letgo_core` re-sets it when core is actually re-indexed. Unit test
  added.

**Verification.** `bb check` (fmt + clippy `-D warnings` + 129 lib / 8 + 5 + 5 +
3 integration tests) and `bb e2e` (52 passed, 1 ignored) both green, including
the new `test_e2e_letgo_core_navigation` (bare `map` → `core.lg`, `str/join`
through the `clojure.string` alias → `string.lg`).

**No deviations** from the plan's intended behavior; the limitations above
(opt-in via `:lg-version`, source must be fetched, Go primitives don't navigate)
hold as written.
