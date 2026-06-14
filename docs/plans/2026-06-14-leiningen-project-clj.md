# Leiningen `project.clj` Support Implementation Plan

> **Status: COMPLETED (2026-06-15).** See the summary at the end.

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Index and navigate the dependencies of a Leiningen project by reading `project.clj` directly — mapping its declared Maven coordinates to the JARs already in `~/.m2/repository` — so completion, hover, and go-to-definition reach library code without running `java` or `lein classpath`.

**Tech Stack:** Rust, `edn-format` (parses each extracted `[…]` vector after masking strings/comments — not the whole `(defproject …)` form), the existing `index_classpath_libs` JAR pipeline.

---

## Design

A Leiningen project declares Maven dependencies in `project.clj`. Those deps are
already downloaded as JARs under `~/.m2/repository`, and we already have a full
JAR-indexing pipeline (`scanner::index_classpath_libs`) used by the
deps.edn/`.cpcache` path. So Leiningen support is just **a new classpath
source**: parse `project.clj`, map each `[group/artifact "version"]` coordinate
to its `~/.m2` JAR path, and hand the JARs that exist on disk to the existing
indexer. No new indexing code, no `java`, no `lein classpath`.

The one wrinkle is parsing. `project.clj` is Clojure, not EDN, and real files
use reader macros that EDN cannot represent. **This was validated against
`../tickets/project.clj`** (a real Leiningen + cljs project): it contains
`^{:protect false}`, `^:replace`, and `#"user"`, and `edn_format` rejects all
three (`^` → `UnexpectedCharacter`, `#"` → bad dispatch). So EDN-parsing the
*whole* `(defproject …)` form fails on common real-world files. Instead we
extract and parse only the small plain-data vectors we care about (see Parsing
strategy below).

### Parsing strategy (validated against `../tickets/project.clj`)

We never parse the whole form. Instead:

1. **Mask** the source: produce a same-length copy with the contents of strings
   (`"…"`, respecting `\"`), line comments (`; …`), and character literals
   (`\x`) blanked to spaces. Brackets and keywords *in code* are preserved;
   brackets/keywords *inside strings or comments* are erased so they can't
   mislead the scan.
2. For each keyword of interest, find every occurrence in the masked text (at a
   token boundary), skip whitespace to the next form, and `edn_format::parse_str`
   the **original** slice from that point. `parse_str` reads exactly one value
   and stops, so it consumes just the `[…]` vector (or `"…"` string) and ignores
   the `^`/`#"…"`/reader-macro junk that follows elsewhere in the file. A slice
   that fails to parse is skipped; the others still succeed.

This makes parsing robust to metadata and regex anywhere outside the targeted
vectors, and — because it matches *every* occurrence — it naturally **unions
top-level, `:profiles`, and `:cljsbuild` deps and source-paths**. That picks up
dev/test deps (`etaoin`, `clj-http`, `figwheel` in tickets) and extra source
dirs (`dev`, `src/cljs`), which are exactly the things you navigate to.

### Key decisions

1. **Read `project.clj` by masked, per-vector EDN extraction — never by running
   anything.** Targeted keywords: `:dependencies`, `:source-paths`,
   `:test-paths`, `:local-repo`. Union every occurrence (top-level + profiles +
   builds). Any slice that won't parse is skipped, never fatal. A file so broken
   that nothing parses still gets default `src`/`test` source indexing — just no
   dep navigation.

2. **Direct dependencies only — no transitive resolution.** Without an
   aether/`java` resolver we only see what is literally in `project.clj`. This
   matches the ROADMAP, which keeps "Transitive Clojure deps" as a separate
   item. The libs you actually `:require` are direct deps, so this is useful on
   its own.

3. **deps.edn / `.cpcache` stays authoritative.** In
   `server::resolve_and_index_libs`, the Clojure branch tries
   `classpath::discover` (cpcache) first; only when that is empty **and** a
   `project.clj` exists do we fall back to Leiningen resolution. A populated
   cpcache carries the full transitive classpath, so it is strictly better and
   we never override it.

4. **Repo location honors `:local-repo`, defaults to `~/.m2/repository`.**
   `:local-repo` is a real Leiningen key (relative-to-root or absolute);
   honoring it makes us correct for users who set it *and* lets the e2e test be
   fully hermetic (point it at a temp dir) without a synthetic env var. This
   mirrors how `lgx.rs` takes a `home` argument for testability via
   `resolve_with_home`.

5. **Reuse, don't duplicate.** `leiningen::resolve` returns a `Vec<PathBuf>` of
   **existing** m2 JARs and feeds straight into
   `scanner::index_classpath_libs` — the same pipeline deps.edn JARs already
   take. Filtering to existing JARs in `resolve` keeps the server's `n > 0`
   gate and its log count meaningful (undownloaded transitive deps don't
   inflate it).

### Maven coordinate → JAR path

For a dependency `[group/artifact "version"]`:

- Coordinate symbol with a namespace (`org.clojure/clojure`) → `group =
  org.clojure`, `artifact = clojure`.
- Coordinate symbol with no namespace (`hiccup`) → `group = artifact = hiccup`.
- JAR path: `<repo>/<group dots→slashes>/<artifact>/<version>/<artifact>-<version>.jar`.
  Example: `org.clojure/clojure 1.11.1` →
  `~/.m2/repository/org/clojure/clojure/1.11.1/clojure-1.11.1.jar`.

Entries with extra options (`[g/a "1.0" :exclusions [...]]`) read only element
[0] (symbol) and [1] (string version); the rest is ignored. An entry with no
string version is skipped. Classifiers and SNAPSHOT timestamp resolution are
out of scope (noted limitation).

### Source paths

`config::source_paths` already unions declared roots with the conventional
`src`/`test` defaults, and Leiningen's defaults *are* `src`/`test`, so the
standard layout works with no change. The gain from parsing `project.clj` is
non-standard `:source-paths`/`:test-paths` (e.g. `["src/clj" "src/cljs"]` and
`["dev"]` in tickets). When the project is not let-go and deps.edn declares no
`:paths`, read the unioned `:source-paths`/`:test-paths` from `project.clj`.
Non-existent dirs are skipped by the file walker, so over-collecting is safe.

## File Structure

- **Create `src/leiningen.rs`** — the resolver. Public `resolve(root) ->
  Vec<PathBuf>` (existing m2 JARs) and `source_paths(edn) -> Vec<String>`;
  internal `resolve_with_repo(root, repo)` for hermetic unit tests; coordinate
  parsing, Maven path mapping, and `m2_repo(root, edn)`
  (`:local-repo` else `~/.m2/repository`). Owns all `project.clj` parsing.
- **Modify `src/lib.rs`** — add `pub mod leiningen;`.
- **Modify `src/config.rs`** — extend `source_paths` so a Leiningen project
  (no let-go, no deps.edn `:paths`) reads `:source-paths`/`:test-paths` from
  `project.clj`.
- **Modify `src/server.rs`** — in `resolve_and_index_libs`, add the Leiningen
  fallback to the Clojure branch (cpcache first, else `project.clj`).
- **Modify `tests/test_e2e.rs`** + **create `tests/fixtures/lein_project/`** —
  end-to-end navigation into an m2 JAR resolved from `project.clj`.

Reuse the shared `edn` helpers (`crate::edn::{get, kw, as_str, str_vec_at}`)
rather than duplicating typed accessors. Mirror `src/lgx.rs` for module shape,
doc-comment style, and the `resolve_with_<x>` test seam.

---

### Task 1: `project.clj` masked extractor + dependency/path parsing (`src/leiningen.rs`)

**Files:**
- Create: `src/leiningen.rs`
- Modify: `src/lib.rs`

- [x] **Step 1: Write failing unit tests**
  In `src/leiningen.rs` `#[cfg(test)]`, cover pure parsing (no filesystem). Use
  a realistic fixture string modeled on `../tickets/project.clj` — it MUST
  include `^{:protect false}`, `^:replace`, `#"user"`, and a `:profiles` block
  with its own `:dependencies` — so the tests prove robustness to the real
  shape:
  - `parse_deps` on that fixture returns the union of top-level **and**
    `:profiles` deps, each with correct group/artifact/version (`ring` →
    group=artifact=`ring`; `org.clojure/clojure` → group=`org.clojure`,
    artifact=`clojure`; dev-profile `etaoin` is present).
  - The `^{:protect false}`, `^:replace`, and `#"user"` constructs do not break
    extraction (the result is non-empty and correct).
  - A dependency entry with trailing options (`[org.clojure/clojurescript
    "1.10.879" :scope "provided"]`) parses to coord + version, extras ignored;
    an entry with no string version is skipped.
  - `source_paths` returns the union of all `:source-paths`/`:test-paths`
    occurrences (top-level `src/clj`, `src/cljs`; profile `dev`; `test/clj`).
  - A file with no defproject / no targeted keywords yields empty, no panic.
  - Masking: a `:dependencies` token that appears inside a string or `;` comment
    is NOT picked up.
  - Coord→path: `org.clojure/clojure 1.11.1` under a given repo dir →
    `<repo>/org/clojure/clojure/1.11.1/clojure-1.11.1.jar`.

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib leiningen`
  Expected: FAIL (module/functions do not exist).

- [x] **Step 3: Implement the extractor + parser**
  Add `pub mod leiningen;` to `src/lib.rs`. In `src/leiningen.rs`:
  - `mask(src) -> Vec<char>` (or same-length `String`): single pass over chars
    tracking normal / in-string / in-line-comment state. Blank string contents
    (honor `\"`), comment contents (`;` to `\n`), and the char after a top-level
    `\` (character literal). Preserve everything else, so brackets/keywords in
    code keep their positions for slicing the original.
  - `forms_after(src, masked, keyword) -> Vec<edn_format::Value>`: for each
    token-boundary occurrence of `keyword` in `masked`, advance past whitespace
    to the next form and `edn_format::parse_str(&original[offset..])`; collect
    the `Ok` values (parse_str stops after one value, ignoring trailing junk).
    This is the one primitive the rest builds on.
  - `parse_deps(src) -> Vec<Coord>`: `forms_after(:dependencies)` → for each
    `Value::Vector`, each element is a `Value::Vector` whose [0] is a
    `Value::Symbol` (namespace=group, else group=name) and [1] a
    `Value::String` version. Build `Coord { group, artifact, version }`; dedup.
  - `source_paths(src) -> Vec<String>`: union the string elements of every
    `forms_after(:source-paths)` and `forms_after(:test-paths)` vector.
  - Maven path helper:
    `<repo>/<group with '.'→'/'>/<artifact>/<version>/<artifact>-<version>.jar`.
  - Reuse `crate::edn::{as_str, str_vec_at}` where they fit. Keep every function
    total: malformed input → empty, mirroring `lgx.rs`.

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib leiningen`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Add Leiningen project.clj extractor (masked per-vector parse)"`

### Task 2: Resolve deps to existing m2 JARs (`src/leiningen.rs`)

**Files:**
- Modify: `src/leiningen.rs`

- [x] **Step 1: Write failing unit tests**
  Using `tempfile`, mirror `lgx.rs`'s filesystem tests:
  - Lay out a fake repo dir with `org/clojure/clojure/1.11.1/clojure-1.11.1.jar`
    and `hiccup/hiccup/1.0.5/hiccup-1.0.5.jar`; write a `project.clj` declaring
    both. `resolve_with_repo(root, repo)` returns both JAR paths.
  - A declared dep whose JAR is absent on disk is omitted from the result.
  - `m2_repo` honors `:local-repo "m2"` (relative to root) and absolute
    `:local-repo`; with no `:local-repo`, `resolve_with_repo` is the seam the
    public `resolve` builds on (test the default-repo wiring only via the
    `:local-repo` path to stay hermetic).

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib leiningen`
  Expected: FAIL (`resolve_with_repo`/`m2_repo` not implemented).

- [x] **Step 3: Implement resolution**
  - `m2_repo(root, edn)`: extract `:local-repo` via `forms_after` (it yields a
    `Value::String`); if present, use it absolute or joined to `root`. Else
    `~/.m2/repository` via `HOME`/`USERPROFILE` (mirror `lgx::lgx_home`).
  - `resolve_with_repo(root, repo)`: read `project.clj`, `parse_deps`, map each
    coord to its JAR path under `repo`, keep only paths where `.exists()`.
  - `pub fn resolve(root)`: read `project.clj`, compute `m2_repo`, delegate to
    `resolve_with_repo`. Return empty (with a `tracing::debug!`) when there is
    no `project.clj`.

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib leiningen`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Resolve Leiningen deps to existing ~/.m2 JARs"`

### Task 3: Source paths from `project.clj` (`src/config.rs`)

**Files:**
- Modify: `src/config.rs`

- [x] **Step 1: Write failing unit tests**
  In `config.rs` tests:
  - A project with only `project.clj` declaring `:source-paths
    ["src/main/clojure"]` makes `source_paths` include
    `root.join("src/main/clojure")` alongside the `src`/`test` defaults.
  - A standard `project.clj` (no `:source-paths`) still yields `src`/`test`.
  - deps.edn `:paths` still takes precedence when both files exist.

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib config`
  Expected: FAIL.

- [x] **Step 3: Implement**
  In `source_paths`, after the let-go and deps.edn branches: when the declared
  set is empty and `root.join("project.clj")` exists, read it and use
  `leiningen::source_paths`. Keep the existing `src`/`test` union and dedup.

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib config`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Read :source-paths/:test-paths from project.clj"`

### Task 4: Wire the Leiningen fallback into indexing (`src/server.rs`)

**Files:**
- Modify: `src/server.rs`

- [x] **Step 1: Implement the fallback**
  In `resolve_and_index_libs`, Clojure branch: keep
  `classpath::discover(root)` first. When it returns empty **and**
  `root.join("project.clj")` exists, call `leiningen::resolve(root)`; if it
  returns JARs, pass them to `scanner::index_classpath_libs` and return their
  count. Add a `use crate::leiningen;` import. Update the function doc-comment
  to mention the Leiningen fallback.

- [x] **Step 2: Verify the workspace builds and unit tests pass**
  Run: `bb check`
  Expected: PASS (fmt clean, clippy `-D warnings` clean, all unit tests green).

- [x] **Step 3: Commit**
  `git commit -m "Index Leiningen deps when no .cpcache is present"`

### Task 5: End-to-end navigation into an m2 JAR

**Files:**
- Create: `tests/fixtures/lein_project/` (`project.clj`, `src/`)
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing e2e test**
  Model on `test_e2e_completion_from_jar_library`:
  - Fixture `project.clj`: `(defproject lein-app "0.1.0" :local-repo "m2"
    :dependencies [[mylib "1.0.0"]] :source-paths ["src"])`. (`:local-repo`
    keeps it hermetic — the repo lives inside the temp project.)
  - In the test, build a JAR at
    `<root>/m2/mylib/mylib/1.0.0/mylib-1.0.0.jar` containing
    `mylib/util.clj` with a `defn` (reuse the `zip::ZipWriter` pattern).
  - A consumer `src/uses_lib.clj` that `:require`s `[mylib.util :as u]`.
  - `LspClient::start(&root)`, `initialize`, `wait_for_log("library indexing
    complete")`, `did_open`, then assert completion offers `u/<fn>` (and/or
    `goto_definition` returns a `jar:` URI ending in `!/mylib/util.clj`).

- [x] **Step 2: Run to verify it fails**
  Run: `bb e2e` (or `cargo test --test test_e2e e2e_lein`)
  Expected: FAIL (no completion / no definition) before wiring is exercised end
  to end.

- [x] **Step 3: Make it pass**
  No new production code expected — Tasks 1–4 supply the behavior. Fix any
  fixture-path or coordinate-mapping mismatches the test reveals.

- [x] **Step 4: Run the full e2e + check suite**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Smoke-test against the real project (manual, not CI)**
  The maintainer has a real Leiningen project at `../tickets` (relative to this
  repo). Build the binary and point it there; confirm the server resolves the
  declared deps that are actually downloaded in `~/.m2` (in the current VM only
  `cheshire` is present, so at minimum `cheshire.core` should index/navigate).
  This is a sanity check on the masked parser against a real file with
  `^`/`#"…"`/profiles — not a CI gate (it depends on local `~/.m2` contents).

- [x] **Step 6: Commit**
  `git commit -m "e2e: navigate into m2 JAR resolved from project.clj"`

### Task 6: Mark the roadmap item done

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Check the box**
  Change the Phase 5 Leiningen line from `- [ ]` to `- [x]` and append a short
  note: direct deps only (transitive deferred), `~/.m2`/`:local-repo`, JARs
  reuse the existing classpath indexer, reader-macro `project.clj` falls back to
  default source indexing.

- [x] **Step 2: Commit**
  `git commit -m "Mark Leiningen project.clj support complete in roadmap"`

---

## Notes & limitations (carry into the roadmap note)

- **Direct deps only.** Transitive deps need a resolver we deliberately avoid;
  tracked separately on the ROADMAP. We do, however, union top-level +
  `:profiles` + `:cljsbuild` deps, so dev/test deps are covered.
- **Reader macros are tolerated where it counts.** Metadata (`^…`) and regex
  (`#"…"`) anywhere outside the targeted `:dependencies`/`:source-paths`/
  `:test-paths`/`:local-repo` vectors are ignored by the masked per-vector
  parse. Only a reader macro *inside one of those vectors themselves* would drop
  that vector (rare). A totally unparseable file still gets default `src`/`test`
  source indexing.
- **SNAPSHOT / classifiers.** Timestamped SNAPSHOT artifacts and classified
  JARs are not resolved.
- **Precedence.** deps.edn `.cpcache`, when present, always wins over
  `project.clj`.

---

## Implementation summary (2026-06-15)

Implemented as designed, on branch `leiningen-project-clj`. All `bb check`
(fmt + clippy `-D warnings` + 107 lib tests) and `bb e2e` (37 tests) pass.

- **`src/leiningen.rs`** (new) — `mask()` blanks string/comment/char-literal
  contents; `forms_after()` finds each targeted keyword in the masked text,
  seeks the opening delimiter (stepping over `^:replace`-style metadata), and
  `edn_format::parse_str`es one value from the original. `parse_deps()`,
  `source_paths()`, `m2_repo()` (`:local-repo` else `~/.m2/repository`),
  `jar_path()`, `resolve_with_repo()`, and `pub fn resolve()`. 8 unit tests,
  including a fixture carrying `^{:protect false}`, `^:replace`, `#"user"`, and
  profile-level `:dependencies`.
- **`src/config.rs`** — `source_paths` falls back to `project.clj`'s
  `:source-paths`/`:test-paths` when not let-go and deps.edn declares no
  `:paths`. 3 new tests (incl. deps.edn-wins precedence).
- **`src/server.rs`** + **`src/main.rs`** — `resolve_and_index_libs` consults
  `leiningen::resolve` only when `.cpcache` is empty and `project.clj` exists;
  registered the module in the binary's module tree (`main.rs` re-declares
  modules separately from `lib.rs` — both needed).
- **`tests/fixtures/lein_project/` + `tests/test_e2e.rs`** — hermetic e2e
  (`:local-repo "m2"`) proving completion from a project.clj-resolved JAR.

### Notes & deviations

- **Tasks 1 & 2 share one commit.** Codex's first review correctly flagged that
  Task 1 alone left private items used only by tests, failing
  `clippy -D warnings`. Task 2 makes `resolve`/`source_paths` public and wires
  the chain, so the two were committed together to keep every commit green.
- **Implementation-first, not strict per-task TDD-then-wire.** Because the
  feature was wired across Tasks 1–4 before the e2e, the Task 5 test passed on
  first run rather than failing first.
- **Real-project smoke (Task 5, step 5) exceeded expectations.** A throwaway
  `leiningen::resolve("../tickets")` resolved 4 downloaded jars — `slingshot`,
  `cheshire`, `cljs-ajax` (top-level) and `eftest` (from the `:coverage`
  profile) — confirming the masked parser handles the real file's
  metadata/regex and that profile-dep unioning works end to end.
- **Branching.** Initial commits landed on `master`; moved to branch
  `leiningen-project-clj` and restored `master` to `origin/master`.

### Codex review follow-ups (both fixed)

A second-opinion codex review surfaced two P2 correctness issues, fixed in a
follow-up commit:

- **`#_` reader-discard breaks parsing.** Empirically, `edn_format` 3.3.0 fails
  the *entire* parse on a `#_` discard inside a vector (`UnexpectedCharacter`),
  so one disabled dep entry would drop all deps. Fixed by stripping discarded
  forms before parsing: `prepare()` now returns a `locator` (strings/comments/
  discards blanked, for finding keywords) and a `parse_buf` (only comments and
  discards blanked, strings preserved, fed to `edn_format`). `discard_ranges()`
  + `form_end()` compute the spans. Covered by `skips_reader_discarded_forms`.
- **project.clj edits left indexes stale.** The watcher only treated
  `deps.edn`/`lgx.edn` as manifests. Added `project.clj`, so live edits
  re-resolve deps and source-paths via the existing re-index path. Covered by
  the `test_e2e_project_clj_change_indexes_new_deps` e2e.

### Post-completion fix

- **project.clj was linted as source.** Reported after merge: opening
  `project.clj` flagged its dependency coordinates (`org.clojure/clojure`,
  `ring/ring-defaults`) as unresolved namespaces, because `is_clojure_source`
  matched its `.clj` extension. Excluded `project.clj` by name (so `build.clj`
  and real sources stay linted), mirroring the EDN-config exclusion. Covered by
  `test_e2e_no_diagnostics_on_project_clj` and a `config` unit test.
