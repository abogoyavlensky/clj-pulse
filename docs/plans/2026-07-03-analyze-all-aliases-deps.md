# Analyze Deps From All Aliases / Contexts / Tasks Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make go-to-definition and completion work on external libraries that are
declared only under a deps.edn `:alias` (`:extra-deps`) or an lgx.edn `:contexts`/`:tasks`
entry (`:extra-deps`), by resolving those deps from the config files — best-effort, no
subprocess.

**Tech Stack:** Rust, tower-lsp, `edn-format` v3, existing `~/.m2` / `$LGX_HOME/gitlibs`
resolution helpers.

---

## Design

### The problem

Alias/context/task **source paths** already work — deps.edn top-level `:paths` plus every
alias `:extra-paths` are unioned in `config::parse_paths_from_deps_edn` (`src/config.rs:124`).
The gap is external **library deps** declared only under an alias/context/task:

- **deps.edn:** `:deps`/`:extra-deps` are never parsed. The library classpath comes solely
  from `classpath::discover`, which reads `.cpcache` and returns the entries of the *single
  newest* `.cp` file (`src/classpath.rs:16-45`). That `.cp` only reflects whatever alias set
  the user last invoked `clojure` with, so alias-only deps are usually invisible.
- **lgx.edn:** only the top-level `:deps` map is resolved (`src/lgx.rs:308`). lgx.edn also has
  `:contexts` and `:tasks` top-level maps whose entries carry `:extra-deps`/`:extra-paths`
  (same idea as tools.deps `:aliases`, different key names) — those are never read.
- **project.clj:** already handled. `leiningen::parse_deps` folds in `:profiles` deps and
  resolves them to `~/.m2` JARs (`src/leiningen.rs:237`). No new work beyond a locking test.

Approach: **file-only, best-effort** — no shelling out to the Clojure CLI, consistent with
the current architecture (the server never runs `clojure -Spath`; that string is only
user-facing warning text).

### deps.edn — two complementary mechanisms

**1. Merge all `.cpcache/*.cp` files instead of newest-wins.** Change `classpath::discover`
to union the still-existing entries across *every* `.cp` file, deduped. Non-existent entries
are dropped as today, so staleness stays self-healing. Effect: any alias set the user has
ever run (`-M:test`, `-M:dev`, …) contributes its fully-transitive classpath for free.

**2. New `deps_edn` resolver for direct `:deps` + every alias `:extra-deps`.** Parse deps.edn,
walk top-level `:deps` and `:aliases` → each alias map → `:extra-deps`, and resolve each
coordinate to a path that exists on disk:
- `:mvn/version` → `~/.m2/repository/<group>/<artifact>/<version>/<artifact>-<version>.jar`
- `:local/root` → directory (absolute, or relative to root)
- git coords → **deferred** (the `~/.gitlibs` layout differs from lgx's and is
  version-sensitive); invoked git deps are still covered by mechanism #1.

Both are **unioned** with the classpath in `resolve_and_index_libs` (`src/server.rs:40`),
deduped, indexed once. #1 gives transitive coverage for alias sets you've run; #2 gives
direct coverage even for alias deps you've never run (as long as the jar was downloaded).

### lgx.edn — walk `:contexts` and `:tasks`

Refactor `lgx::parse_deps` to expose a reusable "parse a `:deps`-shaped map" helper, then
seed the resolver's BFS from top-level `:deps` **+** every `:contexts/*/:extra-deps` **+**
every `:tasks/*/:extra-deps`. Existing coord logic (git → `$LGX_HOME/gitlibs`, `:local/root`
→ dir) and transitive walking (each dep's own lgx.edn `:deps`) are reused unchanged.
Semantics match tools.deps: contexts/tasks `:extra-deps` activate only at the top level, not
transitively — so transitive expansion (`read_deps`) keeps reading only `:deps`. Also union
`:contexts/*/:extra-paths` and `:tasks/*/:extra-paths` into `lgx::paths` so their source dirs
are indexed too (mirrors deps.edn's alias `:extra-paths`).

### Re-index on config change — already wired, verify + lock

`did_change_watched_files` already sets `classpath_changed` on any `deps.edn`/`lgx.edn`/
`project.clj` edit and calls `index.clear_libs()` + `resolve_and_index_libs`
(`src/server.rs:619-628`). Because all new resolution routes through `resolve_and_index_libs`,
editing an alias/context dep re-indexes automatically. No new plumbing — add an e2e test that
locks the behavior.

### Shared Maven helpers

Extract `default_m2()` and the coord→jar-path builder out of `src/leiningen.rs` into a new
`src/maven.rs`, reused by both `leiningen` and the new `deps_edn` resolver. `leiningen::m2_repo`
(which also reads `:local-repo`) stays in `leiningen` but calls `maven::default_m2` as its
fallback.

### Decisions & non-goals

- **Merge all `.cp` files** (not newest-wins): can index two versions of a lib if the user
  switched versions — acceptable best-effort; project-wins invariant untouched.
- **Git deps in deps.edn `:extra-deps` are deferred** — backstopped by the `.cp` merge.
- **No `jar_cache::format_version` bump** — no change to extractor output or `Symbol`/`NsMeta`
  layout.
- **Can't index what isn't on disk** — a dep never downloaded (no jar in `~/.m2`, no
  `.gitlibs` checkout) is silently skipped. Best-effort by definition.

## File Structure

- **Create `src/maven.rs`** — shared `~/.m2` helpers: `default_m2()`, `jar_path(repo, group,
  artifact, version)`. One responsibility: Maven local-repo path math.
- **Create `src/deps_edn.rs`** — `resolve_lib_paths(root) -> Vec<PathBuf>`: parse deps.edn
  `:deps` + all alias `:extra-deps`, resolve `:mvn/version`/`:local/root` coords to existing
  on-disk paths, deduped.
- **Modify `src/leiningen.rs`** — use `maven::{default_m2, jar_path}` (delete the local copies).
- **Modify `src/classpath.rs`** — `discover` merges all `.cp` files; update tests.
- **Modify `src/lgx.rs`** — parse `:contexts`/`:tasks` `:extra-deps` into the BFS seed; union
  their `:extra-paths` into `paths`.
- **Modify `src/server.rs`** — `resolve_and_index_libs` unions `deps_edn::resolve_lib_paths`
  into the Clojure classpath before indexing.
- **Modify `src/main.rs` and `src/lib.rs`** — register `mod maven;` / `mod deps_edn;` (pub in
  lib.rs).
- **Modify `tests/test_e2e.rs`** (+ fixture under `tests/fixtures/`) — alias-only dep
  navigation, at startup and after a live deps.edn edit.
- **Modify `docs/MEMORY.md` and `CLAUDE.md`** — document the new resolution + invariants.

---

### Task 1: Shared Maven helpers (`src/maven.rs`)

**Files:**
- Create: `src/maven.rs`
- Modify: `src/leiningen.rs`, `src/main.rs`, `src/lib.rs`

- [ ] **Step 1: Write failing tests**
  In `src/maven.rs`, add `#[cfg(test)]` tests: `jar_path` builds
  `<repo>/org/clojure/test.check/1.1.1/test.check-1.1.1.jar` from
  `("org.clojure", "test.check", "1.1.1")` (group dots → slashes); `default_m2` returns
  `<HOME>/.m2/repository` when `HOME` is set (guard with an env-set temp, matching the style
  in `leiningen.rs`).

- [ ] **Step 2: Register the module and run tests to verify they fail**
  Add `mod maven;` to `src/main.rs` and `pub mod maven;` to `src/lib.rs`.
  Run: `cargo test --lib maven`
  Expected: FAIL (functions not yet defined / compile error).

- [ ] **Step 3: Implement `maven.rs`**
  Move `default_m2()` and a generalized `jar_path(repo: &Path, group: &str, artifact: &str,
  version: &str) -> PathBuf` from `leiningen.rs` into `maven.rs` (make them `pub`).

- [ ] **Step 4: Refactor `leiningen.rs` to use `maven`**
  Replace `leiningen::default_m2` calls with `maven::default_m2`; change the private
  `jar_path(repo, &Coord)` to call `maven::jar_path(repo, &coord.group, &coord.artifact,
  &coord.version)`. Delete the now-dead local copies.

- [ ] **Step 5: Run tests to verify they pass**
  Run: `cargo test --lib maven && cargo test --lib leiningen`
  Expected: PASS (leiningen's existing coord/resolve tests still green).

- [ ] **Step 6: Commit**
  `git commit -m "refactor: extract shared Maven repo helpers into maven module"`

---

### Task 2: deps.edn dep resolver (`src/deps_edn.rs`)

**Files:**
- Create: `src/deps_edn.rs`
- Modify: `src/main.rs`, `src/lib.rs`

- [ ] **Step 1: Write failing tests**
  In `src/deps_edn.rs`, add tests over inline EDN + a `tempfile` root:
  - top-level `:deps` `{org.clojure/test.check {:mvn/version "1.1.1"}}` resolves to the m2 jar
    **only when that jar file exists** on the fake repo (set `HOME` to the temp dir, create the
    jar); missing jar is skipped.
  - a symbol without a namespace (`ring {:mvn/version "1.9.0"}`) maps to group == artifact.
  - an alias `:extra-deps` dep is included (walk `:aliases` → each → `:extra-deps`).
  - a `:local/root` dep resolves to its directory (relative to root, and absolute), included
    only when the dir exists.
  - a git coord (`:git/url`) is skipped (deferred), no panic.
  - result is de-duplicated when the same coord appears in `:deps` and an alias.

- [ ] **Step 2: Register the module and run tests to verify they fail**
  Add `mod deps_edn;` to `src/main.rs` and `pub mod deps_edn;` to `src/lib.rs`.
  Run: `cargo test --lib deps_edn`
  Expected: FAIL (function not defined).

- [ ] **Step 3: Implement `resolve_lib_paths(root: &Path) -> Vec<PathBuf>`**
  Read `<root>/deps.edn`; parse with `edn_format::parse_str`. Collect coord specs from
  top-level `:deps` and from every `:aliases/*/:extra-deps` map. For each `(lib-symbol, spec)`:
  derive `(group, artifact)` from the symbol (namespace/name, else name/name); if `spec` has
  `:mvn/version`, resolve via `maven::jar_path(m2, group, artifact, version)` where `m2 =
  maven::default_m2()`; else if `:local/root`, resolve the dir (abs or root-relative); else
  skip. Keep only paths that `exists()`, deduped (preserve first-wins order). Reuse `crate::edn`
  helpers (`get`, `kw`, `kw_ns`, `as_str`). Return `vec![]` on missing/malformed deps.edn.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib deps_edn`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: resolve deps.edn :deps and alias :extra-deps to lib paths"`

---

### Task 3: Merge all `.cp` files (`src/classpath.rs`)

**Files:**
- Modify: `src/classpath.rs`

- [ ] **Step 1: Update/write tests for merge semantics**
  Rewrite `test_discover_picks_most_recent_cp` → `test_discover_merges_all_cp`: two `.cp` files
  referencing `lib1` and `lib2` respectively → `discover` returns **both** (order-independent
  assertion). Keep `test_discover_returns_existing_paths_filters_missing` and
  `test_discover_falls_back_when_newest_cp_is_stale` passing (stale/missing entries still
  dropped; a mix of one live + one dead `.cp` still yields the live entries). Add a test that
  duplicate entries across `.cp` files are de-duplicated.

- [ ] **Step 2: Run tests to verify the new one fails**
  Run: `cargo test --lib classpath`
  Expected: FAIL on `test_discover_merges_all_cp`.

- [ ] **Step 3: Implement the merge**
  Change `discover` to iterate every file from `cp_files_newest_first`, collect all
  resolved-and-existing entries into one `Vec`, de-duplicate (preserve first-seen order,
  newest `.cp` first), and return the union. Drop the early-return-on-first-valid logic; keep
  the per-entry existence filter and relative-to-root resolution. Update the doc comment.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib classpath`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: merge all .cpcache classpath files instead of newest-only"`

---

### Task 4: Union deps.edn resolver into library indexing (`src/server.rs`)

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Wire `deps_edn::resolve_lib_paths` into `resolve_and_index_libs`**
  In the `ProjectKind::Clojure` arm (`src/server.rs:36-49`): after building `classpath` (from
  `classpath::discover`, or the leiningen fallback when empty + `project.clj` present), extend
  it with `deps_edn::resolve_lib_paths(root)` and de-duplicate the combined list (preserve
  order). Index the union via `scanner::index_classpath_libs` as today. Add `use
  crate::deps_edn;`.

- [ ] **Step 2: Build and run the fast checks**
  Run: `cargo build && cargo test --lib server 2>/dev/null; cargo test --lib`
  Expected: PASS (compiles; existing lib tests green).

- [ ] **Step 3: Commit**
  `git commit -m "feat: index deps.edn alias deps alongside the .cpcache classpath"`

---

### Task 5: lgx `:contexts` / `:tasks` extra-deps + extra-paths (`src/lgx.rs`)

**Files:**
- Modify: `src/lgx.rs`

- [ ] **Step 1: Write failing tests**
  In `src/lgx.rs` tests, add inline-EDN cases:
  - `parse_deps`-for-root includes a dep declared under `:contexts {:dev {:extra-deps {...}}}`
    and under `:tasks {:build {:extra-deps {...}}}`, in addition to top-level `:deps`.
  - a transitive dep's own lgx.edn `:contexts`/`:tasks` are **not** followed (only its
    `:deps`), preserving tools.deps semantics.
  - `paths` unions `:contexts/*/:extra-paths` and `:tasks/*/:extra-paths` with top-level
    `:paths`, de-duplicated.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib lgx`
  Expected: FAIL (contexts/tasks not parsed).

- [ ] **Step 3: Implement**
  Extract the current `parse_deps` map-walking body into a helper `parse_deps_map(deps:
  &BTreeMap<Value, Value>) -> Vec<(String, Dep)>`. Add a root-seed function (e.g.
  `parse_root_deps(edn)`) that parses top-level `:deps` **plus** each `:contexts/*/:extra-deps`
  and `:tasks/*/:extra-deps` map through `parse_deps_map`, first-wins deduped by lib name (base
  `:deps` first). Seed the BFS in `resolve_with_home` from `parse_root_deps`; keep `read_deps`
  (transitive) using the `:deps`-only `parse_deps`. Extend `paths` to union
  `:contexts/*/:extra-paths` and `:tasks/*/:extra-paths`.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib lgx`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: resolve lgx :contexts and :tasks :extra-deps and :extra-paths"`

---

### Task 6: Lock project.clj profile-dep resolution (`src/leiningen.rs`)

**Files:**
- Modify: `src/leiningen.rs`

- [ ] **Step 1: Add a resolve-level locking test**
  Add a test that `resolve` (or `resolve_with_repo`) returns the JAR for a `:profiles`-only
  dependency when that jar exists under a temp `:local-repo`/`~/.m2`. Confirms the already-working
  path stays working after the Task 1 refactor.

- [ ] **Step 2: Run test**
  Run: `cargo test --lib leiningen`
  Expected: PASS (behavior already present; this pins it).

- [ ] **Step 3: Commit**
  `git commit -m "test: lock project.clj profile-dep resolution"`

---

### Task 7: E2E — alias-only dep navigation at startup (`tests/test_e2e.rs`)

**Files:**
- Modify: `tests/test_e2e.rs`
- Create: fixture under `tests/fixtures/` (a project whose alias `:extra-deps` is a
  `:local/root` dep, so no `~/.m2` is needed)

- [ ] **Step 1: Build the fixture**
  Create a fixture project: `deps.edn` with `{:paths ["src"] :aliases {:dev {:extra-deps
  {some/lib {:local/root "vendor/lib"}}}}}`, a `vendor/lib/` dir containing a `.clj` file that
  defines a namespace + a `defn` (`some.lib`/`hello`), and a `src/` file that `require`s
  `some.lib`. No `.cpcache` — proves the file-only resolver, not the classpath cache.

- [ ] **Step 2: Write the failing e2e test**
  Following the `LspClient` template (`setup_project`, `initialize`, `did_open`,
  `wait_for_log("library indexing complete")`): assert `textDocument/definition` on the
  `some.lib/hello` usage resolves into `vendor/lib/...clj`. Optionally assert completion offers
  `hello`.

- [ ] **Step 3: Run to verify it fails on a stale build, then passes**
  Run: `bb e2e` (or `cargo test --test test_e2e <name>`)
  Expected: PASS with the implemented resolver (would FAIL without Tasks 2 & 4).

- [ ] **Step 4: Commit**
  `git commit -m "test(e2e): navigate into an alias-only :local/root dep"`

---

### Task 8: E2E — live re-index after adding an alias dep (`tests/test_e2e.rs`)

**Files:**
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing e2e test**
  Start from a fixture whose `:dev` alias has **no** extra-dep and whose `src` file references
  `some.lib/hello` (initially unresolved). After `initialize` + first index, write the
  `:local/root` alias `:extra-deps` into `deps.edn` on disk and send
  `workspace/didChangeWatchedFiles` for it; `wait_for_log("library re-indexing complete")`,
  then assert `textDocument/definition` now resolves into `vendor/lib`. (Confirms the existing
  `classpath_changed` → `clear_libs` + `resolve_and_index_libs` path carries the new resolver.)

- [ ] **Step 2: Run the test**
  Run: `bb e2e` (or `cargo test --test test_e2e <name>`)
  Expected: PASS.

- [ ] **Step 3: Commit**
  `git commit -m "test(e2e): re-index alias deps live on deps.edn change"`

---

### Task 9: Docs + full verification

**Files:**
- Modify: `docs/MEMORY.md`, `CLAUDE.md`

- [ ] **Step 1: Update docs**
  In `docs/MEMORY.md`, note: deps.edn alias `:extra-deps` and lgx `:contexts`/`:tasks`
  `:extra-deps` are now resolved (file-only, best-effort); `.cpcache` files are merged (union)
  rather than newest-only; deps.edn git `:extra-deps` are deferred. In `CLAUDE.md`, extend the
  Invariants section: alias/context/task `:extra-deps` are resolved best-effort from the config
  files; `.cp` files are unioned; project deps still win over library deps. Use /writing-clearly.

- [ ] **Step 2: Full check**
  Run: `bb check`
  Expected: PASS (fmt + clippy `-D warnings` + all tests).

- [ ] **Step 3: Full e2e**
  Run: `bb e2e`
  Expected: PASS (all e2e including the two new tests).

- [ ] **Step 4: Commit**
  `git commit -m "docs: document alias/context/task dep resolution"`
