# Release Plan 1: Correctness and Coverage Before 1.0 Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the wrong answers and coverage holes that should not ship in 1.0: Integrant keys offered as vars, functions defined through qualified def macros (`mu/defn`, `s/defn`) invisible to navigation, no `jar:` navigation in Neovim, unverified let-go support in the Clojure Pulse extension, settings scattered across the README, and stale Leiningen docs (ROADMAP Milestone 5, first of three release plans).

**Tech Stack:** Rust, tower-lsp 0.20, tree-sitter-clojure; Lua for the Neovim snippet; Node for the Clojure Pulse e2e harness. Tests: `tests/test_completion.rs`, `tests/test_extractor.rs`, `tests/test_e2e.rs`, `bb e2e-nvim`, `bb e2e-pulse`.

---

## Design

### Integrant keys in the completion pools

`(defmethod ig/init-key ::database …)` indexes a `DefKind::IntegrantKey` symbol whose fqn is the keyword `:ns/database` and whose name is `database`. The auto-require pool filters `sym.fqn.starts_with(':')` (`handlers/completion.rs`, around line 666); the current-namespace pool, the alias-qualified pool, and the `:refer :all` pool (around lines 321, 379, 415) do not, so they offer `database` and `sys/database` as vars that do not exist. Fix: one predicate, `fn is_var_symbol(sym) -> bool` (fqn does not start with `:`), applied in all four places. The keyword side already works: the dispatch keyword is an occurrence, so `::datab` completes to `::database` through `complete_keywords`.

### Qualified def-family heads

`process_top_level_list` (`extractor.rs`, around line 548) resolves a defining head with `str_to_defkind(first_text)`, then `macro_def_kind` through `:lint-as` and the three-entry built-in table. For `mu/defn` the head text is `mu/defn`, which matches nothing, so the function is never indexed. Measured on metabase: 1 497 of about 12 000 function definitions use `mu/defn`, and the project's kondo config handles that through a hook, not `:lint-as`.

Fix: one shared resolver, `head_def_kind(head, ns_meta, source, lint_as) -> Option<(String, DefKind)>`, that tries `:lint-as` and the built-in table on the resolved fqn first (today's `macro_def_kind`), then, for a head with a namespace part, `DefKind::from_def_symbol` on its *name* part. `mu/defn`, `s/defn`, `p/defn-`, `mu/defmethod`, `clojure.core/defn` all resolve to their kind by name, whatever the qualifier. This is what Cursive does. `:lint-as` still wins because it is consulted first.

Three code paths classify defining forms independently and all three must use the resolver, or the index and the scope walkers disagree:

- `process_top_level_list` (definition extraction): the symbol is indexed.
- `walk_list` (occurrence walker): today a `defn` head is a binding form only when `head_is_core_form` says so, meaning unqualified or qualified to `clojure.core`. A head the resolver maps to `Defn`, `DefnPrivate`, `Defmacro`, or `Defmethod` must take the same `walk_fn_form` path, so `mu/defn`'s parameters bind as locals instead of being recorded as var occurrences of the current namespace.
- `walk_scope` (position-directed locals for definition, completion, rename): the same mapping, so a parameter of a `mu/defn` resolves as a local under the cursor.

Guards: the name-part fallback applies only when the form's second child is a symbol, so `(s/def ::user (s/keys …))` keeps its current behavior, a keyword occurrence and no var (spec's `s/def` names a keyword, and `extract_def` already returns early on a non-symbol name; the test pins it). A head the resolver does not map keeps today's behavior exactly. Bump `CACHE_FORMAT_VERSION` (currently 14) since library extraction output changes.

### Neovim `jar:` navigation

Neovim's built-in client creates an empty buffer for a `jar:` location and fires `BufReadCmd` for it. A snippet handles that event for `jar:*` patterns: find the clj-pulse client, request `clojure/dependencyContents` with the buffer's name as the URI, fill the buffer, set `filetype=clojure`, `buftype=nofile`, `modifiable=false`. One source of truth, `editors/nvim/jar.lua`, which:

- exports a `setup(opts)` function with `client_name` defaulting to `"clj_pulse"` (the name the README's `vim.lsp.config` uses);
- is embedded verbatim in the README's Neovim section under "Library navigation", replacing the "not opened yet" caveat, with a one-line `dofile`/`require` alternative for users who vendor the file;
- is loaded by `scripts/e2e_nvim.lua` with `client_name = "clj-pulse"`, which then jumps to `str` in `utils.clj`'s `(str "Hello, " name)` after waiting for `library indexing complete` (the fixture's `.cpcache` names the clojure JAR, and the jar cache is committed), opens the location with `vim.lsp.util.show_document`, and asserts the new buffer contains `(defn str`.

No server change.

### let-go verification

Server e2e on `tests/fixtures/letgo_project` (hermetic through `LGX_HOME`, as the existing let-go tests do): references on `run`'s call of `loc/hello` from `app.lg` lists the definition in `vendor/loc/src/loc/core.lg` plus the usage; rename of a project-local `.lg` var edits both sites; an unused `:require` in a `.lg` file publishes `unused-namespace` with `source: "clj-pulse"` (kondo skips `.lg`, `kondo::lints_file`). Add a second file `src/util.lg` to the fixture with a var used from `app.lg` so rename has a cross-file case.

Clojure Pulse e2e: the fixture becomes a two-project workspace by adding `scripts/pulse-e2e/fixture/letgo/` with `lgx.edn` (`:paths ["src"]`, a `:local/root` dep at `vendor/loc`, no git dep so no network and no `LGX_HOME`), `src/app.lg`, and `vendor/loc/src/loc/core.lg`. The server's project detection indexes it as a sub-project. New checks in `tests.js`: definition on `loc/hello` in `app.lg` lands in `vendor/loc/src/loc/core.lg`; hover on `when` returns the special-form description; completion after `loc/` offers `loc/hello`; a diagnostic with code `unused-namespace` appears on an unused require in `app.lg`. The extension already associates `.lg` with the clojure language.

### Settings in one place

`docs/SETTINGS.md`, three tables:

1. `.clj-pulse/config.edn`: `:projects` (`:path`, `:classpath {:enabled :cmd}`), `:lint-as`, `:kondo {:enabled :path :live-max-kb}`. Columns: key, default, live-reload, Clojure Pulse setting (`clojurePulse.projects`, `clojurePulse.kondo.enabled`, `clojurePulse.kondo.path`, `clojurePulse.kondo.liveMaxKb` marked pending until the extension ships it).
2. `initializationOptions`: `projects`, `kondo`, `clojuredocs.path`.
3. Environment variables: `CLJ_PULSE_TOOL_DIRS`, `CLJ_PULSE_JDK_SRC`, `CLJ_PULSE_CLOJUREDOCS_EXPORT`, `CLJ_PULSE_DISABLE_CLASSPATH_CLI`, `CLJ_PULSE_DISABLE_KONDO`, with `CLJ_PULSE_TEST_PANIC` listed as test-only.

Defaults are copied from the parsers (`projects.rs`, `kondo.rs`, `clojuredocs.rs`), not from prose. README's Configuration and Linting sections keep their explanations and link to the page; RELEASE.md's docs sweep gains "SETTINGS.md matches the parsers".

### Leiningen docs

Stage 3 runs `lein classpath` for a root Leiningen project by default (`projects::LEIN_CMD`), so transitive deps are indexed. Direct-deps-only is the stage-2 fallback and the sub-project default. Correct: the README "Dependency depth" note, the `docs/MEMORY.md` section "Leiningen indexes only direct dependencies" (retitle and rewrite the stance: the JVM cost is paid once per resolution in the background, never on the hot path), and note the extension README line for the extension change.

## File Structure

Create:

- `editors/nvim/jar.lua`.
- `docs/SETTINGS.md`.
- `tests/fixtures/letgo_project/src/util.lg`.
- `scripts/pulse-e2e/fixture/letgo/{lgx.edn,src/app.lg,vendor/loc/src/loc/core.lg}`.

Modify:

- `src/handlers/completion.rs`: `is_var_symbol` in four pools.
- `src/index/extractor.rs`: name-part fallback for qualified heads; `src/index/jar_cache.rs`: version 15.
- `scripts/e2e_nvim.lua`, `scripts/pulse-e2e/tests.js`.
- `tests/test_completion.rs`, `tests/test_extractor.rs`, `tests/test_e2e.rs`, `tests/fixtures/snippets/qualified_defs.clj` (new).
- `README.md`, `docs/MEMORY.md`, `docs/RELEASE.md`, `AGENTS.md`, `docs/ROADMAP.md`.

## Tasks

### Task 1: Integrant keys out of the var pools

**Files:**
- Modify: `src/handlers/completion.rs`
- Test: `tests/test_completion.rs`

- [x] **Step 1: Write the failing tests**
  Using `tests/fixtures/integrant_project`: `test_integrant_key_not_offered_as_var` (prefix `d` in `readx.db` has no item labeled `db`), `test_integrant_key_not_offered_through_alias` (a namespace aliasing `readx.db` as `db` gets no `db/db`), `test_integrant_key_not_offered_through_refer_all` (a namespace with `[readx.db :refer :all]` gets no bare `db`), and `test_integrant_key_completes_as_keyword` (`::d` yields `::db`).

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_completion integrant_key`
  Expected: FAIL on the first two.

- [x] **Step 3: Implement**
  `is_var_symbol` and its four call sites.

- [x] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Keep Integrant keys out of the var completion pools"`

### Task 2: Qualified def-family heads

**Files:**
- Modify: `src/index/extractor.rs`, `src/index/jar_cache.rs`
- Test: `tests/test_extractor.rs`, `tests/fixtures/snippets/qualified_defs.clj`, `tests/test_e2e.rs`

- [x] **Step 1: Write the failing tests**
  Snippet with `[malli.util :as mu]`, `[schema.core :as s]`, and `[clojure.spec.alpha :as spec]` requires, `(mu/defn f :- :int [x :- :int] (str x))`, `(s/defn g [y] y)`, `(mu/defn- h [] 1)`, `(mu/defmethod m :k [_] 1)`, and `(spec/def ::user string?)`. Assert symbols `f` (Defn), `g` (Defn), `h` (DefnPrivate), `m` (Defmethod) with the right names and ranges, and *no* symbol for `::user` while its keyword occurrence is still recorded. Assert the occurrences: `x` inside `f` is not a var occurrence (it is a bound local), and `str` is. Add a `:lint-as` override test: with `malli.util/defn` mapped to `clojure.core/def`, `f` is indexed as `Def`, proving the fallback never outranks the config. Check the existing `mu/defn` param-vector handling: `f`'s params must not include the `:-` return schema; if `extract_def` picks up `:-` and `:int` as params, add the fix here and a test for the signature.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor qualified_defs`
  Expected: FAIL (no symbols).

- [x] **Step 3: Implement**
  `head_def_kind` in the extractor, used by `process_top_level_list`, `walk_list`, and `walk_scope` as designed. Bump `CACHE_FORMAT_VERSION` to 15.

- [x] **Step 4: e2e**
  Add `(mu/defn scale [factor x] (* factor x))` to a `simple_project` file, with a top-level `(def factor 10)` elsewhere in the project. Assert definition, hover, and references reach `scale`; references on the parameter `factor` inside `scale` list only the local's sites, never the global `factor`; and rename of that parameter edits only the local (the parameter-shadowing regression the review asked for).

- [x] **Step 5: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 6: Commit**
  `git commit -m "Resolve qualified def-family heads like mu/defn by name"`

> Deviation: the e2e fixture keeps `(def factor 10)` in the same new file as
> `(mu/defn scale …)` rather than elsewhere in the project — `core.clj`'s
> outline is pinned by `test_e2e_document_symbols_outline`, and a global in the
> same namespace is the sharper shadowing test (the name really does resolve
> there).
> Deviation: codex found that the new fallback sent `[x :- s/Int]` whole to the
> binding collectors, inventing a local `Int` and losing the reference to the
> schema var. Fixed in a follow-up commit (`is_schema_annotation_marker`): the
> element after a `:-` marker is walked as a usage in both the occurrence and
> the scope collector.
> Advisory (not fixed, filed in the ROADMAP backlog): `walk_scope` cannot see
> `:lint-as`, so a qualified head the config maps to a *non*-fn kind still binds
> its vector as parameters there. Pre-existing for bare heads; fixing it means
> threading `ExtractConfig` through `locals_in_scope_at`.

### Task 3: Neovim `jar:` snippet

**Files:**
- Create: `editors/nvim/jar.lua`
- Modify: `scripts/e2e_nvim.lua`, `README.md`

- [x] **Step 1: Write the snippet**
  As designed: `setup({ client_name = ... })` registers the `BufReadCmd` autocommand. Keep it under 40 lines with no dependencies beyond `vim.lsp`.

- [x] **Step 2: Extend the Neovim gate**
  Load the file, wait for `library indexing complete`, jump to `str`, show the document, assert `(defn str` in the buffer. Run: `bb e2e-nvim`. Expected: the new check passes.

- [x] **Step 3: README**
  Replace the caveat paragraph with a "Library navigation" sub-section embedding the snippet and stating the client name must match.

- [x] **Step 4: Commit**
  `git commit -m "Open jar: locations in Neovim through clojure/dependencyContents"`

> Deviation: the plan assumed the fixture's `.cpcache` and jar cache are
> committed; both are gitignored local artifacts, so the gate needs a resolved
> classpath on the box (it has one). No change to the checks.
> Deviation: Neovim does not recognize `jar:file://…` as a URL, so it names the
> buffer relative to the working directory. The autocommand matches `*/jar:*`
> as well as `jar:*` and reads the URI back out of the buffer name.
> Deviation: the README frames the snippet as a file to save (or vendor and
> `dofile`) rather than paste into `init.lua` — it is a module ending in
> `return M`, which is a syntax error mid-`init.lua`.

### Task 4: let-go verification

**Files:**
- Create: `tests/fixtures/letgo_project/src/util.lg`, `scripts/pulse-e2e/fixture/letgo/…`
- Modify: `tests/test_e2e.rs`, `scripts/pulse-e2e/tests.js`

- [ ] **Step 1: Server e2e**
  `test_e2e_letgo_references_across_files`, `test_e2e_letgo_rename_across_files`, `test_e2e_letgo_unused_require_diagnostic`. Run: `cargo test --test test_e2e letgo`. Expected: PASS, or a real finding to fix in the smallest way.

- [ ] **Step 2: Pulse fixture and checks**
  Add the `letgo/` sub-project and the four checks to `tests.js`. Run: `bb e2e-pulse`. Expected: PASS. If the sub-project is not detected, check `projects::detect` depth and the `.gitignore` in the fixture before touching the server.

- [ ] **Step 3: Commit**
  `git commit -m "Verify let-go support end to end, server and extension"`

### Task 5: Settings page and Leiningen docs

**Files:**
- Create: `docs/SETTINGS.md`
- Modify: `README.md`, `docs/MEMORY.md`, `docs/RELEASE.md`

- [ ] **Step 1: Write SETTINGS.md**
  The three tables, defaults read from the parsers. Use /writing-clearly.

- [ ] **Step 2: Link and correct**
  README: Configuration and Linting link to the page; the Dependency depth note describes stage 3 for Leiningen. MEMORY.md: retitle and rewrite the Leiningen section. RELEASE.md: the sweep checks SETTINGS.md.

- [ ] **Step 3: Commit**
  `git commit -m "Document every setting in one place and correct the Leiningen docs"`

### Task 6: Roadmap and invariants

**Files:**
- Modify: `AGENTS.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Update**
  AGENTS.md invariants: var pools exclude keyword-fqn symbols; qualified def heads resolve by name after `:lint-as`. ROADMAP Milestone 5: tick the items this plan owns, set this plan's status to `done`.

- [ ] **Step 2: Verify and commit**
  Run: `bb check && bb e2e && bb e2e-nvim && bb e2e-pulse && bb e2e-calva && bb bench`
  Expected: all gates pass; the bench table is within noise of the MEMORY.md tables (the extractor changed, and AGENTS.md requires a bench run after extractor changes). Calva is required because definition results changed shape for `mu/defn` targets.
  `git commit -m "Record release plan 1 in the roadmap and invariants"`
