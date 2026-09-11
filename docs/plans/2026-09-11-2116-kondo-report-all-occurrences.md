# clj-kondo Reports Every Occurrence Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every clj-kondo finding show in the editor at once. Today an unresolved namespace, symbol, or var used five times gets one squiggle, and fixing it reveals the next one (ROADMAP Milestone 5, a pre-1.0 fix).

**Tech Stack:** Rust (`src/kondo.rs`), the fake clj-kondo (`tests/fixtures/fake-clj-kondo/clj-kondo`), e2e (`tests/test_e2e.rs`), `bb e2e-real-kondo`. No new settings.

---

## Design

### Cause

clj-kondo's `unresolved-namespace`, `unresolved-symbol`, and `unresolved-var` linters report each name **once per file** unless `:report-duplicates true` is set (`clj_kondo/impl/linters.clj`, the `hide-duplicates?` checks; documented in `doc/linters.md`). Those are the only three linters that hide duplicates; every other linter already reports every occurrence. Reproduced on the user's case: an alias renamed in the ns form, two usages of the old alias in the body, one squiggle by default and two with the flag.

A second effect compounds it: clj-pulse's native `unresolved-namespace` lint reports every occurrence, but a successful kondo run owns that code and the native copies are dropped, so kondo's single squiggle replaces our complete set.

The bridge itself is not at fault; a real run with nine findings arrives as nine diagnostics.

### Fix

`kondo::lint` passes one more `--config` after the JSON-output one:

```clojure
{:linters {:unresolved-namespace {:report-duplicates true}
           :unresolved-symbol    {:report-duplicates true}
           :unresolved-var       {:report-duplicates true}}}
```

clj-kondo merges every `--config` over the project's `.clj-kondo/config.edn`, so levels, excludes, `:lint-as`, and hooks still apply; only the duplicate hiding changes. Single-report is a terminal convenience to keep CLI output short. An editor's job is to mark every site, and that is what every other editor integration shows, so there is no setting: it is always on.

The warm run (`kondo::warm`, `--dependencies`) produces no findings, so it does not carry the flag.

### Tests

- The fake kondo records its argv for `--lint` runs the way it already does for `--dependencies` (`FAKE_KONDO_LOG`), so the e2e can assert the second `--config` and its content.
- `test_e2e_kondo_lint_requests_report_duplicates`: with the fake, one lint pass on the `kondo_project` fixture writes an argv line containing both `--config` values, and the duplicates one names all three linters.
- `test_e2e_real_kondo_reports_every_unresolved_occurrence` (ignored, `bb e2e-real-kondo`): a buffer with two usages of an unrequired alias publishes two `unresolved-namespace` diagnostics with `source: "clj-kondo"`, on different lines. This is the proof that matters; the fake cannot prove kondo honors the flag.
- The existing `test_e2e_kondo_run_drops_native_unused_binding` and the threshold tests are unaffected.

### Docs

README Linting: one sentence that clj-pulse asks clj-kondo to report every occurrence of an unresolved namespace, symbol, or var, not just the first as the CLI does. AGENTS.md invariants: one line. ROADMAP: the item under Milestone 5, ticked; plus one Backlog entry for the unrelated findings made while diagnosing this: a lint pass fails silently on a 2 s timeout or when the mise shim, run from the file's directory, hits an untrusted or unpinned mise config, and neither reaches `lintStatus`.

## File Structure

Modify:

- `src/kondo.rs`: `REPORT_DUPLICATES_CONFIG` constant, second `--config` in `lint`.
- `tests/fixtures/fake-clj-kondo/clj-kondo`: log argv on `--lint` runs.
- `tests/test_e2e.rs`: the two tests.
- `README.md`, `AGENTS.md`, `docs/ROADMAP.md`.

## Tasks

### Task 1: The flag and the fake

**Files:**
- Modify: `src/kondo.rs`, `tests/fixtures/fake-clj-kondo/clj-kondo`
- Test: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing test**
  Extend the fake: in the `--lint` branch, when `FAKE_KONDO_LOG` is set, append `"$@"` to it before reading stdin (keep the `--dependencies` logging as is). One existing test, `test_e2e_kondo_cache_not_warmed_without_a_clj_kondo_dir`, opens a buffer and then treats the log's absence as proof the warm never ran; once lint runs log too, that proof is wrong. Change it to read the log if present and assert no line contains `--dependencies`. Add `test_e2e_kondo_lint_requests_report_duplicates` next to `test_e2e_kondo_run_drops_native_unused_binding`, using `start_with_kondo_env` with `FAKE_KONDO_LOG`, opening a fixture file, waiting for its diagnostics, then asserting the log has a line containing `--lint -`, the JSON-output config, and a second `--config` whose text contains `:unresolved-namespace {:report-duplicates true}`, `:unresolved-symbol`, and `:unresolved-var`.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e report_duplicates`
  Expected: FAIL (no second `--config`).

- [x] **Step 3: Implement**
  `const REPORT_DUPLICATES_CONFIG: &str` with the EDN from the design, one line; `lint` adds `.arg("--config").arg(REPORT_DUPLICATES_CONFIG)` after the JSON-output config. Update the doc comment on `lint` to say why.

- [x] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Ask clj-kondo to report every unresolved occurrence, not just the first"`

### Task 2: Proof against the real binary

**Files:**
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Write the ignored test**
  `test_e2e_real_kondo_reports_every_unresolved_occurrence`, `#[ignore]` like the other real-kondo test, using `start_with_real_kondo`. Open a temp project file with `(ns x.y)` and, on separate lines: two calls through the unrequired alias `missing` (namespace), two uses of an undefined bare symbol `nowhere` (symbol), and two calls of `clojure.string/no-such-fn` (var, which needs the clojure JAR in kondo's cache; if the cache is cold on the box, `unresolved-var` may not fire, so assert it only when at least one appears and note it). Wait for diagnostics; assert two `unresolved-namespace` and two `unresolved-symbol` diagnostics with `source: "clj-kondo"`, each pair on different lines.

- [x] **Step 2: Run it**
  Run: `bb e2e-real-kondo`
  Expected: PASS on this box (clj-kondo is installed through mise). If the run skips because the binary is missing, say so in the completion note rather than marking the task done.

- [x] **Step 3: Commit**
  `git commit -m "Prove clj-kondo reports every unresolved occurrence through the bridge"`

> Deviation: `unresolved-var` is asserted strictly, not only when it appears. clj-kondo ships `clojure.string`'s analysis built in, so `(str/no-such-fn …)` under `(:require [clojure.string :as str])` fires without a `.clj-kondo` dir or a warm run; the plan's bare `clojure.string/no-such-fn` would have been an `unresolved-namespace` instead. Verified the test fails on the pre-flag `kondo.rs`.

### Task 3: Docs and roadmap

**Files:**
- Modify: `README.md`, `AGENTS.md`, `docs/ROADMAP.md`

- [x] **Step 1: Update docs**
  README Linting paragraph: the sentence from the design. AGENTS.md diagnostics invariant: "every `lint` run passes `REPORT_DUPLICATES_CONFIG`, so the three unresolved linters mark every site; the CLI default of one per file is a terminal convenience, not an editor one". ROADMAP: tick the Milestone 5 item and set its `Plan:` to `done` (the Backlog entry on silent lint-pass failures was recorded when the plan was linked). Use /writing-clearly.

- [x] **Step 2: Verify and commit**
  Run: `bb check && bb e2e && bb e2e-pulse`
  Expected: PASS. The Pulse gate is required: the change alters which diagnostics the editor shows.
  `git commit -m "Document that clj-kondo reports every occurrence in the editor"`
