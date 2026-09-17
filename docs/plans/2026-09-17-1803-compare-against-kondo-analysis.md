# `bb compare`: Differential Correctness Against clj-kondo Analysis — Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Status: completed 2026-09-17** (commits a71935f … cddeeb9 on `bb-compare`). Summary at the end.

**Goal:** A `bb compare` gate that asks one production clj-pulse definition, references and rename questions at every position clj-kondo's analysis knows the answer to, on a pinned real corpus, and reports every disagreement by language construct.

**Tech Stack:** Rust integration test (`tests/`), the existing `LspClient` harness, clj-kondo `--config '{:analysis …}'` JSON output, tree-sitter-clojure (via the `clj_pulse` lib crate) for coverage accounting, babashka task in `bb.edn`.

---

## Design

### Why

E2E tests check the cases we thought of. The soak compares a churned clj-pulse against a fresh clj-pulse, so a shared resolution bug passes. The bench accepts a definition that lands in the right file, either dialect. Nothing checks that a definition lands on the *right declaration*, that references are the *whole* set and nothing more, or that rename edits exactly the reference sites. This gate does, against an oracle that is not our own extractor.

### The oracle

clj-kondo's analysis output. One run per corpus, from the corpus directory so its own `.clj-kondo/config.edn` (`:lint-as` included) applies to the oracle the way it applies to us:

```
clj-kondo --lint src test \
  --config '{:analysis {:locals true :keywords true} :output {:format :json}}'
```

Output shape (verified on `tests/fixtures/simple_project`, clj-kondo v2026.08.04):

- `var-definitions`: `ns`, `name`, `filename`, `row`/`col` (form), `name-row`/`name-col` (the name token), `defined-by`.
- `var-usages`: `from`, `to`, `name`, `filename`, `row`/`col` (whole token incl. alias), `name-row`/`name-col` (name part), `macro`, optional `refer`/`alias` fields. Unresolvable ones carry `to: "clj-kondo/unknown-namespace"`.
- `locals`: `id`, `name`, `filename`, `row`/`col`, `derived-location` (non-null for a binding kondo synthesized, e.g. `:keys` destructuring), `scope-end-row`.
- `local-usages`: `id`, `name`, `filename`, `row`/`col`, `name-row`/`name-col`.
- `keywords`: `name`, `filename`, `row`/`col`/`end-col`, optional `ns`, optional `alias`, optional `keys-destructuring: true`, `from` (the namespace, `user` before the ns form is read).

Rows and cols are 1-based; LSP is 0-based. `name-row`/`name-col` is where the probe's cursor goes, so a cursor is never on the alias half of `h/greet` (the known alias-half divergence, ROADMAP 2026-09-16, is thereby out of scope by construction).

Namespace-usages are not probed in v1 (definition on a namespace symbol is a different feature).

### Probes and expectations

Probes are oracle entries, not tree-sitter tokens: each carries a position and an answer. Tree-sitter is used only to count, per file, the `sym_lit`/`kwd_lit` tokens no oracle entry covers ("unjudged"), so the report shows what neither side can judge.

```rust
pub struct Probe {
    pub file: PathBuf,        // absolute
    pub line: u32,            // 0-based
    pub character: u32,       // 0-based, at name-col
    pub token: String,        // for the report
    pub bucket: Bucket,
    pub expect: Expectation,
}

pub enum Expectation {
    /// Definition must land on this declaration (file, 0-based line of name-row).
    Definition { file: PathBuf, line: u32 },
    /// Definition must land in a library file of this namespace, in the asking file's dialect.
    LibraryDefinition { ns: String, dialect: Dialect },
    /// References (includeDeclaration: true) must equal this multiset of sites.
    References(Sites),
    /// Rename must be refused (prepareRename or rename answers `invalid params`, or null).
    RenameRefused,
    /// Rename edits must touch exactly this multiset of sites.
    RenameSites(Sites),
}

/// Expected sites: how many occurrences each (file, line) holds, plus the
/// exact (file, line, col) set for the soft column check. Two usages of `f` on
/// one line are two sites, and a server answering one of them is wrong.
pub struct Sites {
    pub per_line: BTreeMap<(PathBuf, u32), usize>,
    pub exact: BTreeSet<(PathBuf, u32, u32)>,
}
```

Where expectations come from:

| Oracle entry | Requests asked | Expectation |
|---|---|---|
| var-usage with `to` defined in the corpus | definition | `Definition` at that var-definition's `name-row`. When several var-definitions share ns+name (a `declare` and its def), the non-`clojure.core/declare` one wins — our de-dup rule. |
| var-usage with `to` not defined in the corpus and not `clj-kondo/unknown-namespace` | definition | `LibraryDefinition` in the dialect of the asking file: `.clj` accepts `.clj`/`.cljc`, `.cljs` accepts `.cljs`/`.cljc`, `.cljc` accepts any. |
| var-usage with `to: clj-kondo/unknown-namespace` | none | counted as unjudged |
| var-definition | references, rename | `References` = the definition's line + every `declare` of the same ns+name + every var-usage with same `to`+`name`; `RenameSites` = the same set. `declare` entries carry no probe of their own (the def does), but they are sites: our references and rename reach the declare. |
| local with `derived-location: null` | definition (from each local-usage), references, rename | `Definition` at the local; `References` = local + local-usages with that `id`; `RenameSites` = same. |
| local with `derived-location` non-null | rename | `RenameRefused` (a `:keys` binding). Definition/references are still asked and compared. |
| keyword with `ns` | references, rename | `References` = every keyword with same `ns`+`name`. Rename is all-or-nothing in clj-pulse, so `RenameRefused` when *any* entry of that group has `keys-destructuring`, or when `ns` is not a namespace defined in the corpus (library-namespaced keywords are refused by `rename_target`); else `RenameSites` = the same set. |
| keyword without `ns` | none | skipped (allowlist: unqualified keywords have no navigable definition and our occurrence model differs by design) |

Sites compare as a multiset of `(file, line)` with counts: the answer's sites are bucketed the same way and the two maps must be equal, so two usages on one line need two answers. Columns are never a divergence: our ranges and kondo's `col`/`name-col` differ on alias-qualified tokens by design. When the per-line counts agree but the `(file, line, col)` sets differ, the verdict is `Soft`, counted in its own column.

### Buckets

Derived from oracle fields, one per probe:

- `var-usage/project`, `var-usage/library`, `var-usage/core` (to = `clojure.core`), each further tagged `macro` when `macro: true`; `var-usage/referred` when the entry has `refer: true`, `var-usage/aliased` when it has `alias`.
- `var-def/<defined-by short name>` (`defn`, `def`, `defmulti`, `defmethod`, `defprotocol`, `deftest`, other).
- `local/plain`, `local/destructured`.
- `keyword/qualified`, `keyword/alias` (has `alias`), `keyword/keys` (has `keys-destructuring`).

A probe is counted under one bucket; the report prints one row per bucket: `probes`, `agree`, `diverge`, `known`, `null`, `soft`, and one `unjudged` line per corpus.

### Allowlist

A static table in `test_compare.rs`:

```rust
struct Known { bucket_prefix: &'static str, matches: fn(&Probe, &Verdict) -> bool, reason: &'static str }
```

Applied after judging: a divergence that a `Known` entry matches is counted as `known` and printed under its reason, not under new divergences. Initial entries:

- Integrant keys (a var-definition whose name is a keyword): reason "Integrant keys are definitions in clj-pulse only". Empty on the clj-kondo corpus; the hook is what matters.
- let-go core namespaces (`to` starting with `letgo.`): reason "let-go core is indexed from lgx deps only".
- Everything else is a new divergence until triaged; triage adds entries here with a dated reason, and a fixed bug removes its entry.

### Driving the server

One `LspClient::start_production(root)` with `with_request_timeout(REQUEST_TIMEOUT)`, settled by the soak's `settle` (moved to common). Files are visited in path order; each is `did_open`ed, its probes asked, then `did_close`d, as the bench does. Requests:

- `textDocument/definition` → `definition_uris` (sites.rs) and the first location's start line.
- `textDocument/references` with `includeDeclaration: true` → `locations` set (moved to common), reduced to `(file, line)`.
- `textDocument/prepareRename` then `textDocument/rename` with `newName: "<token>__cmp"` (keyword probes: `"<name>__cmp"` without a colon). A refusal is a JSON-RPC error with code `-32602` (`invalid params`, which is how `server.rs` maps every `rename_target` rejection) or a `null` result. Any other error code is a harness failure, recorded like every other unexpected error. The `workspaceEdit` is reduced to the sites its `changes` touch. The server is never asked to apply it, so the buffer stays unchanged.

Every JSON-RPC error answer is recorded (the soak's `Session` does this) except a `-32602` on prepareRename/rename, which is an answer.

A `Null` verdict (an empty or `null` answer where the oracle has one) is a wrong answer, not a neutral one: it is reported in its own `null` column so the report separates "resolved wrong" from "did not resolve", but it goes through the allowlist and into `new_divergences` like a `Diverge`, and strict mode fails on it. A `panicked at` in `server.log` fails the run, as in the soak.

### Sampling

Deterministic. Files sorted by path; entries by row. Each bucket takes at most `CLJ_PULSE_COMPARE_LIMIT` probes (default 200), in that order. `CLJ_PULSE_COMPARE_FILES` (default unlimited) caps the number of files visited, so a first look on metabase is possible.

### Report and exit

Per bucket, a table row and one `COMPARE_JSON` line: `{"corpus","bucket","probes","agree","diverge","known","null","soft"}`. Then every new divergence (`file:line token — request — expected … got …`), then known ones grouped by reason. Exit is success unless: the server died, a `panicked at` appeared, an unexpected JSON-RPC error was answered, or clj-kondo could not run. `CLJ_PULSE_COMPARE_STRICT=1` also fails on any new divergence.

The run prints the clj-kondo version and warns when it is not the pinned `2026.08.04`.

### The narrow clojure-lsp change (sites.rs)

`answers` becomes a three-way verdict so the bench keeps timing while the correctness gates get strict:

```rust
pub enum Landing { Landed, WrongDialect, Miss }
pub fn landing(result: &Value, expect: &Expect, asking: &Path) -> Landing;
pub fn answers(result: &Value, expect: &Expect, asking: &Path) -> bool // Landed | WrongDialect
```

- `Expect::File(path)`: `Landed` only when a URI's path equals `path` exactly (the corpus-relative comparison already used by `namespace_file`), not a suffix match.
- `Expect::Archive(entry)`: `Landed` when the archive URI's extension is acceptable for the asking file's dialect (table above); `WrongDialect` when the entry matches but the extension is not; `Miss` otherwise.
- The bench (`test_bench.rs:267`, `:341`) times on `answers` and prints `dialect: wrong` beside the library-definition row when the landing was `WrongDialect`. The soak (`test_soak.rs:1022`, `:1384`) counts `resolved` only on `Landed`.

### Reuse by moving

From `tests/test_soak.rs` into `tests/common/`:

- `session.rs`: `Session` (with `REQUEST_TIMEOUT`), `settle`.
- `diff.rs`: `Divergence`, `location_key`, `locations`, `symbol_set`, `brief`, `brief_set`.

The soak imports them; no behavior change.

### Testing the gate itself

- Unit tests in `oracle.rs` on `tests/fixtures/simple_project`: parsing, expectation building, bucket assignment. They run clj-kondo from the host and skip with a printed message when it is not installed (`bb check` must stay green on a box without it).
- `tests/test_compare.rs` has one non-ignored test that runs the whole pipeline on `simple_project` with `LspClient::start` (kill switches on) and asserts zero new divergences. Same skip rule. This is what keeps the harness working between corpus runs.
- The corpus run is `#[ignore]`, driven by env like the bench.

## File Structure

- Create `tests/common/oracle.rs` — run clj-kondo, parse analysis JSON into `Analysis`, build `Vec<Probe>`, count unjudged tokens with tree-sitter.
- Create `tests/common/session.rs` — `Session`, `settle`, `REQUEST_TIMEOUT` (moved from the soak).
- Create `tests/common/diff.rs` — `Divergence`, `location_key`, `locations`, `symbol_set`, `brief`, `brief_set` (moved from the soak).
- Modify `tests/common/mod.rs` — declare the three modules.
- Modify `tests/common/sites.rs` — `Landing`, `landing`, `answers` with the asking file; dialect table.
- Modify `tests/test_soak.rs` — import moved items; `resolved` counts `Landed` only.
- Modify `tests/test_bench.rs` — pass the asking file; print the dialect verdict.
- Create `tests/test_compare.rs` — the gate: judge, allowlist, report, fixture test, corpus test.
- Modify `bb.edn` — `compare` task.
- Modify `CLAUDE.md`, `README.md`, `docs/SETTINGS.md`, `docs/ROADMAP.md`, `docs/MEMORY.md`.

---

### Task 1: Move `Session`/`settle` and the diff helpers into `tests/common`

**Files:**
- Create: `tests/common/session.rs`, `tests/common/diff.rs`
- Modify: `tests/common/mod.rs`, `tests/test_soak.rs`

- [x] **Step 1: Move the code**
  Cut `Session`, its `impl`, `REQUEST_TIMEOUT` and `settle` from `test_soak.rs` (around lines 775–887) into `tests/common/session.rs`; make the items and fields `pub`. Cut `Divergence` + `impl`, `location_key`, `locations`, `symbol_set`, `brief`, `brief_set` (around lines 1043–1210) into `tests/common/diff.rs`, `pub`. Add `pub mod session; pub mod diff;` in `mod.rs`. Fix the soak's imports. No logic changes.

- [x] **Step 2: Verify**
  Run: `bb check`
  Expected: green. Note `common` is compiled per test binary, so unused-item warnings in `test_e2e` are possible; add `#[allow(dead_code)]` at the module level of the two new files, matching how `sites.rs`/`sampling.rs` handle it (check first).

- [x] **Step 3: Commit**
  `git commit -m "Move soak session and diff helpers into tests/common"`

> Deviation: `Divergence` fields renamed `churned`/`reference` → `mine`/`theirs` and `print` takes the two labels (the plan's Task 4 hint, done here as it said). `SAMPLES`/`POLL` stayed in the soak; only `REQUEST_TIMEOUT` moved.

### Task 2: Three-way landing verdict in `sites.rs`

**Files:**
- Modify: `tests/common/sites.rs`, `tests/test_bench.rs`, `tests/test_soak.rs`

- [x] **Step 1: Write failing unit tests**
  `tests/common` has no unit tests of its own, so add a small `tests/test_sites.rs` with three cases: `Expect::File` accepts the exact path and rejects a longer path with the same suffix; `Expect::Archive("clojure/string")` from a `.clj` asker returns `WrongDialect` for `jar:…/clojure/string.cljs` and `Landed` for `.clj` and `.cljc`; a `.cljc` asker accepts all three.

- [x] **Step 2: Run to verify they fail**
  Run: `cargo test --test test_sites`
  Expected: compile error (`Landing` does not exist).

- [x] **Step 3: Implement**
  Add `Dialect` (`Clj`, `Cljs`, `Cljc`, from a path's extension) and `accepts(&self, ext: &str) -> bool`, `Landing`, `landing(result, expect, asking)`. `answers` = `landing != Miss`. `Expect::File` compares full paths (`u.strip_prefix("file://") == path`). Update the four call sites to pass `&site.file`. In the bench, keep a `wrong_dialect: bool` per library sample and print `(dialect: wrong)` on that row. In the soak, `resolved += 1` only on `Landed`.

- [x] **Step 4: Verify**
  Run: `cargo test --test test_sites && bb check`
  Expected: PASS, green.

- [x] **Step 5: Commit**
  `git commit -m "Judge a definition landing by exact file and dialect"`

> Deviation: `Expect::File` percent-decodes the URI path before the exact compare (codex P2, fixup dc81f01). Codex P1 asked the bench to time only on `Landed`; declined — the plan's design keeps the bench timing a wrong-dialect landing and flags the row, so a server that only ever answers the other dialect does not burn the ceiling and print `None`.

### Task 3: Oracle parsing and probe building

**Files:**
- Create: `tests/common/oracle.rs`
- Modify: `tests/common/mod.rs`

- [x] **Step 1: Write failing tests** (in `tests/test_compare.rs`, module `oracle_tests`, all guarded by `oracle::kondo_available()` which prints and returns when clj-kondo is missing)
  On `tests/fixtures/simple_project`:
  - `run` returns an `Analysis` with non-empty `var_definitions`, `locals`, `keywords`.
  - `probes` yields a `Definition` probe for a var-usage in `consumer.clj` pointing at the `name-row` in `core.clj` (pick a concrete pair by reading the fixture).
  - a `local/destructured` probe carries `RenameRefused`; a `keyword/keys` probe carries `RenameRefused` (fixture `kw_destructure.clj`).
  - a `declare` pair resolves to the non-declare definition, and the def's `References`/`RenameSites` include the declare's line (add a `declare` + `defn` pair to a fixture file if none exists; check `tests/test_e2e.rs` for an existing one first).
  - two usages of one var on one line produce a `Sites` with count 2 for that line.
  - a keyword group with one `keys-destructuring` entry gives `RenameRefused` on every probe of the group, including the plain `::k` ones; a keyword whose `ns` no corpus file defines gives `RenameRefused`.
  - `unjudged` for a file is `>= 0` and lower than the total `sym_lit` count.

- [x] **Step 2: Run to verify they fail**
  Run: `cargo test --test test_compare oracle_tests`
  Expected: compile error.

- [x] **Step 3: Implement `oracle.rs`**
  - `kondo_available() -> Option<String>`: `clj-kondo --version` via `std::process::Command`, returning the version line. `PINNED_KONDO: &str = "2026.08.04"`.
  - `run(root, paths: &[&str]) -> Analysis`: run the command from the Design section in `root`, parse `analysis` with `serde_json` into typed structs (`#[serde(rename = "name-row")]` etc.; unknown fields ignored). Rows/cols 1-based kept as-is in the structs; conversion happens in `probes`.
  - `probes(analysis, root) -> Vec<Probe>` per the expectation table. Index var-definitions by `(ns, name)` preferring non-declare; local-usages by `id`; keywords by `(ns, name)`. Filenames in the JSON are relative to `root`; make them absolute.
  - `unjudged(file_text, covered: &BTreeSet<(u32,u32)>) -> usize`: `clj_pulse::index::extractor::parse_tree`, walk, count `sym_lit`/`kwd_lit` whose start point is not in `covered`. Covered = every oracle entry position (`name-row/name-col` and `row/col`) for that file.
  - `Bucket` as a `String` built from the rules in Design (a `&'static str`-per-variant enum is more code for no gain).

- [x] **Step 4: Verify**
  Run: `cargo test --test test_compare oracle_tests`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Build compare probes from clj-kondo analysis"`

> Deviation: kondo's `name-col` on an aliased token is the token *start* (`c` of `core/add`), not the name part, so `probes` moves the cursor to the first character after the `/` (`Texts::name_part`) for var-usages and keywords; the "never on the alias half" property is kept, by construction in the harness rather than in kondo's output. kondo is spawned from the test's own cwd with absolute `--lint` paths and `--config-dir <root>/.clj-kondo` (a mise shim needs a tool config in the cwd's ancestry, and a temp-dir fixture copy has none); the corpus config still applies. A `:keys`/`:strs`/`:syms` binding is detected from the live tree (`Texts::destructured_at`), since kondo's `derived-location` is null for a literal `{:keys [a]}` entry; a `keys-destructuring` keyword entry gets only a `RenameRefused` probe (its cursor is a local binding, whose own probes cover definition and references). Macro-expanded usages (`are` → `is`) and `%` carry no position and are skipped. A usage whose `to` the corpus defines but whose var it does not (`import-vars`) is skipped. `twice.clj` was added to `simple_project` for the two-usages-on-one-line case. `covered` also counts namespace-definition/usage positions. Buckets: `var-usage/<project|library|core>[/macro][/referred|/aliased]`.

### Task 4: Judging, allowlist and the fixture-level compare test

**Files:**
- Modify: `tests/test_compare.rs`

- [x] **Step 1: Write the failing test**
  `compare_simple_project`: skip if no kondo; `LspClient::start(root)` (not production), `initialize`, `wait_for_log("Indexed")`; run `oracle::run` + `probes`; `ask_and_judge` over all probes; assert `report.new_divergences.is_empty()` (nulls included) with the printed report as the failure message.
  Also `judge_unit_tests` on synthetic `Value`s: a `null` definition answer is `Null` and lands in `new_divergences`; an answer with one site on a line the oracle counts twice is `Diverge`; same counts with shifted columns is `Soft`; a rename answered with error code `-32603` is a harness error, not a refusal.

- [x] **Step 2: Run to verify it fails**
  Run: `cargo test --test test_compare compare_simple_project`
  Expected: compile error.

- [x] **Step 3: Implement**
  - `Verdict { Agree, Diverge { expected: String, got: String }, Null, Soft }`.
  - `ask(client, probe) -> Value` per request kind; `judge(probe, answer) -> Verdict`:
    - `Definition`: `definition_uris` first URI's path == file and start line == line → `Agree`; empty/null → `Null` (a divergence for reporting and strict mode, see Design); else `Diverge`.
    - `LibraryDefinition`: `landing`-style check via `Expect::Archive(ns path)` plus `SymbolSource::Dir` (a `file:` URI outside root ending in the ns path) → `Agree`; `WrongDialect` → `Diverge`.
    - `References`/`RenameSites`: reduce the answer to a `Sites`; `per_line` maps equal and `exact` sets equal → `Agree`; `per_line` equal only → `Soft`; else `Diverge` using `brief_set` over `file:line×count` keys for both directions. A rename `workspaceEdit` may hold `changes` or `documentChanges`; read both.
    - `RenameRefused`: `-32602` error or null from prepareRename or rename → `Agree`; an edit → `Diverge`; any other error code → harness error, verdict `Null`.
  - `KNOWN: &[Known]` with the two initial entries. `Report { rows: BTreeMap<String, Row>, new_divergences: Vec<Divergence>, known: BTreeMap<&str, Vec<Divergence>>, unjudged: usize, soft: usize }`.
  - `ask_and_judge(client, probes, root) -> Report`: group probes by file, `did_open`, ask, judge, `did_close`; a `Diverge` runs through `KNOWN` first.
  - Print: table, `COMPARE_JSON` lines, new divergences, known by reason. Reuse `Divergence::print` for the divergence lines (`churned`/`reference` fields hold `got`/`expected`; rename the fields to `mine`/`theirs` in Task 1 if that reads better — do it there, not here).

- [x] **Step 4: Run and triage**
  Run: `cargo test --test test_compare compare_simple_project -- --nocapture`
  Expected: PASS after fixing the harness (not the server). If a real server divergence appears on the fixture, add it to `KNOWN` with a reason `TODO(triage)` and a ROADMAP backlog line; do not fix the server in this plan.

- [x] **Step 5: `bb check` green, then commit**
  `git commit -m "Compare clj-pulse answers against clj-kondo analysis on the fixture"`

> Deviation: codex hit its usage limit after Task 2's review, so Tasks 3–5 use the inline `code-review` skill as the checkpoint; re-run `review-with-codex` on abde0d4..HEAD when it is back. Triage on the fixture changed the oracle rules rather than the server: (1) keyword rename is refused only for a *library* namespace (required but not defined in the corpus) — the plan's "not defined in the corpus" rule would have refused `:db/id`-style keywords clj-pulse renames fine; (2) a `::c/keys`/`:strs`/`:syms` destructuring directive gets no probe; (3) `_` locals get no probe; (4) keyword expectations carry two site sets, token starts for references (the server ranges the whole token) and name parts for rename (it edits the suffix); (5) the fixture test passes malli's `:lint-as` inline (`oracle::run`'s third argument), since the fixture's `.clj-kondo` is gitignored and without it kondo reads `(mu/defn scale [factor x] …)` as a call. One real by-design divergence went to `KNOWN` (a qualified `{:keys [c/x]}` entry resolves as the keyword, not the local — ROADMAP backlog in Task 6). `LibraryDefinition` probes are filtered out of the fixture test (no classpath under the kill switches on CI).

### Task 5: The corpus run and `bb compare`

**Files:**
- Modify: `tests/test_compare.rs`, `bb.edn`

- [x] **Step 1: Corpus test**
  `#[ignore] fn compare_corpus`: read `CLJ_PULSE_COMPARE_ROOT`, `CLJ_PULSE_COMPARE_CORPUS`, `CLJ_PULSE_COMPARE_LIMIT` (200), `CLJ_PULSE_COMPARE_FILES`, `CLJ_PULSE_COMPARE_STRICT`. `Session::production(root)`, `initialize`, `settle` with the bench's ceiling constant (reuse whatever `test_bench.rs:110` defines; move it to `sampling.rs` if it is private). Oracle paths: `["src", "test"]` — the clj-kondo corpus's `corpus/` dir holds broken samples, and `is_source_ish` already documents why. Apply limits, run `ask_and_judge`, print, then the fail rules from Design (`server.log` panic check as the soak does it, `session.errors` non-empty, strict mode).

- [x] **Step 2: bb task**
  After `soak` in `bb.edn`:
  ```clojure
  compare {:doc "Compare clj-pulse against clj-kondo's analysis on a pinned real repo: `bb compare` (clj-kondo), `bb compare metabase`. Advisory: prints divergences by construct; CLJ_PULSE_COMPARE_STRICT=1 fails on any new one"
           :task (let [[name] *command-line-args*
                       name (or name "clj-kondo")
                       dir (bench-corpus! name)]
                   (bench-prepare! dir)
                   (shell {:extra-env {"CLJ_PULSE_COMPARE_ROOT" (str dir)
                                       "CLJ_PULSE_COMPARE_CORPUS" name}}
                          "cargo test --release --test test_compare -- --ignored --nocapture"))}
  ```

- [x] **Step 3: First run**
  Run: `bb compare`
  Expected: the table prints, exit 0 (advisory). Save the output; it feeds Task 6. Expect real divergences: `:lint-as` binding forms, `defmethod`/`defmulti` targets, protocol methods, `::alias` keywords are the predicted ones.

- [x] **Step 4: Commit**
  `git commit -m "Add bb compare: differential run against clj-kondo analysis"`

> Deviation: the oracle lints `clj_pulse::config::source_paths(root)` (deps.edn `:paths` + alias `:extra-paths` + `src`/`test`), not a fixed `src test`, and answer sites outside those roots are dropped before judging (`within`) — the clj-kondo corpus has an `analysis/` sub-project clj-pulse indexes that the oracle would otherwise never see. Sampling is a per-bucket stride (every k-th probe) rather than the first `limit` in path order, because the vendored `inlined/*.cljs` tree sorted first and filled every library bucket. The oracle runs with `:skip-comments true` (clj-pulse indexes nothing inside `(comment …)`), skips special forms and the `RT.java` vars `core.clj` only documents (`*out*`, `*warn-on-reflection*`, …), and tolerates a missing `to` (`js/parseInt`). `Verdict::Diverge` carries `missing` (expected sites the answer lacks) so a `KNOWN` entry can accept superset answers: protocol method implementations are sites in clj-pulse and callers-only in kondo. The settle ceiling is `session::REQUEST_TIMEOUT` (120 s, the bench's clj-pulse ceiling is the same number). First-run output kept in `.tmp/compare-run4.log`.

### Task 6: Docs and roadmap

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `docs/SETTINGS.md`, `docs/ROADMAP.md`, `docs/MEMORY.md`

- [x] **Step 1: CLAUDE.md**
  Add a `bb compare` paragraph after `bb soak` in Verification, a gate-table row ("after extractor, resolver or rename changes"), and a Testing note: probes come from the oracle, cursors sit at `name-col`, sets compare by file and line, `KNOWN` is the allowlist and triage adds to it with a reason.

- [x] **Step 2: README and SETTINGS**
  README: one line under the gates list. SETTINGS.md: `CLJ_PULSE_COMPARE_ROOT`, `_CORPUS`, `_LIMIT` (200), `_FILES`, `_STRICT` in the environment table, marked test-harness-only like the bench/soak variables.

- [x] **Step 3: ROADMAP and MEMORY**
  ROADMAP: tick the item (add it under the bench/soak milestone entry with `Plan:` linking this file). Add one backlog line per distinct divergence class the first run found, dated, with the bucket and one example site. MEMORY.md: a "Compare against clj-kondo analysis" section with the first run's per-bucket table and the kondo version, the way the bench section records its tables.

- [x] **Step 4: Verify and commit**
  Run: `bb check`
  `git commit -m "Document bb compare and record its first run"`

---

## Completion summary (2026-09-17)

**Implemented.** `bb compare [metabase|clj-kondo]`: `tests/common/oracle.rs`
runs clj-kondo (`--config-dir`, `:skip-comments`, absolute lint paths) and
turns its analysis into probes; `tests/test_compare.rs` asks one production
server, judges (`Agree`/`Diverge`/`Null`/`Soft`), applies the `KNOWN`
allowlist, prints the per-bucket table, `COMPARE_JSON` lines and every
divergence, and gates on a dead server, a panic, an unexpected JSON-RPC
error, or strict mode. `Session`/`settle` and the diff helpers moved into
`tests/common`; `sites.rs` judges a landing three ways (`Landed` /
`WrongDialect` / `Miss`) with `Dialect`, which the bench times through and
the soak/compare gates hold strictly. Fixture pipeline (`compare_simple_project`)
runs in `bb check` at 174/175 agree, 1 known. First corpus run: 3759 probes,
3069 agree, 538 diverge, 133 null, 18 known, 1 soft — thirteen divergence
classes filed in the ROADMAP backlog, table in MEMORY.md.

**Issues encountered.** Codex hit its usage limit after Task 2; Tasks 3–6
were checkpointed with the inline `code-review` skill instead (re-run
`review-with-codex` on `abde0d4..cddeeb9` when it is back). Two clj-kondo
output facts the plan had wrong: `name-col` on an aliased token is the token
start, and `derived-location` is null for a literal `{:keys [a]}` entry —
both handled in the harness (name-part shift, tree-based destructuring check).

**Deviations, gathered.**
- Task 1: `Divergence` fields are `mine`/`theirs`, `print` takes the labels.
- Task 2: `Expect::File` percent-decodes the URI (codex P2); codex P1 (time
  only on `Landed`) declined per the plan's design.
- Task 3: cursor moves to the name part; kondo spawned from the test's cwd
  with `--config-dir`; destructuring detected from the tree; a
  `keys-destructuring` keyword gets a rename probe only; positionless
  entries skipped; `twice.clj` fixture added; buckets
  `var-usage/<kind>[/macro][/referred|/aliased]`.
- Task 4: keyword rename refused only for *library* namespaces (required,
  not defined); destructuring directives and `_` locals get no probe;
  keyword expectations carry token-start sites for references and name-part
  sites for rename; the fixture test passes malli's `:lint-as` inline;
  library probes left out of the fixture test.
- Task 5: oracle lints `config::source_paths(root)` and answers outside those
  roots are dropped; per-bucket stride sampling; special forms and RT vars
  skipped; `Verdict::Diverge { missing }` so the protocol-implementation
  superset is a `KNOWN`; settle ceiling is `session::REQUEST_TIMEOUT`.
- Task 6: the compare variables are documented in SETTINGS.md as a
  harness-only table after the server table (bench/soak variables were never
  there, so a note covers all three).

**What the plan could have specified better.** It pinned the oracle's shape
from one fixture: the `name-col`-is-the-token-start and
`derived-location`-is-null facts, and that kondo analyzes `(comment …)`,
attributes special forms and `RT.java` vars to `clojure.core`, and emits
positionless entries for macro expansions, each cost a harness iteration
that a five-minute survey of the real corpus's JSON would have pre-empted.
It also assumed `src test` covers what clj-pulse indexes; the corpus's
`:paths` and sub-projects made that wrong on the first run.
