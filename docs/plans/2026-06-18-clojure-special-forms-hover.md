# Clojure Special-Forms Hover & Completion

> **Status: ✅ Completed (2026-06-18).** All four tasks implemented, codex-reviewed
> per task, and verified with `bb check` + `bb e2e`. See the
> [Implementation summary](#implementation-summary) at the end.

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give hover and completion for Clojure's **special forms** (`if`, `do`,
`def`, `quote`, `var`, `recur`, `throw`, `try`/`catch`/`finally`, `set!`, `new`,
`monitor-enter`/`-exit`, and the `let*`/`fn*`/`loop*` primitives) — the compiler
intrinsics that have no `clojure.core` var and so currently show nothing on hover
and never appear in completion. This is the special-forms half of the let-go
builtins feature, generalized so both dialects share it. Goto-def stays a no-op
(special forms have no source); the value is the description + completion entry.

**Tech stack:** Rust; the existing `resolve_symbol` / hover / completion handlers;
the dialect-aware special-forms table introduced here.

---

## Design

clojure.core **functions and macros** (`map`, `reduce`, `let`, `fn`, `loop`,
`when`, `cond`, `case`, `letfn`, `deftype`, `reify`, …) already hover, complete,
and navigate in a Clojure project via the existing static `core_symbols()` table
(`src/index/core.rs`) plus the indexed clojure JAR. The gap is the **special
forms** — verified absent from `core_symbols()`: `if do def set! quote var recur
throw try catch finally new monitor-enter monitor-exit let* fn* loop*`.

The let-go feature already built a special-forms mechanism (`SpecialForm`,
`ResolvedSymbol::SpecialForm`, hover formatting, no-op goto-def/signature). This
plan **generalizes that table to be dialect-aware** and turns it on for Clojure.
No "native fns" analog is needed for Clojure (clojure.core fns have real `.clj`
source and are already covered).

### Dialect-aware table

The two dialects share almost every special form; only the extras differ:

- `COMMON_SPECIAL_FORMS`: `if do def set! fn* quote var let* loop* recur try catch
  finally throw` (14) — identical usage/doc in both dialects (relocated from the
  current let-go `SPECIAL_FORMS`).
- `LETGO_EXTRA`: `trace` (let-go VM tracing).
- `CLOJURE_EXTRA`: `.` `new` `monitor-enter` `monitor-exit` (Java interop /
  locking primitives).

A name is looked up via `special_form(name, letgo: bool)` = search `COMMON` then
the dialect's extra. Macros already in `core_symbols()` (`let`/`fn`/`loop`/`case`/
`letfn`/`deftype`/`reify`/`when`/`cond`/…) and obscure compiler internals
(`case*`/`letfn*`/`deftype*`/`reify*`/`import*`) are intentionally excluded.

### Dialect signal

`resolve_symbol`/completion decide dialect via `index.letgo_core()` (the only
signal available there): set → let-go set; unset → Clojure set. Minor accepted
edge: an *unpinned* let-go project (marker off) gets the Clojure extras and misses
`trace` — the `COMMON` forms (the ones that matter) are correct either way.

### Resolution / hover / completion

- **`resolve_symbol`**: the let-go bare-word branch calls `special_form(word,
  true)` (was `special_form(word)`); the Clojure bare-word branch gains a
  `special_form(word, false)` check **after** the current-ns / refer / factory
  lookups (so a project var named `new` still wins) and **before** the
  `core_symbols` fallback (no overlap — order is just for clarity).
- **Hover / goto-def / signature**: unchanged — the existing `SpecialForm` arms
  already format on hover and no-op for goto-def/signature.
- **Completion**: the Clojure (`!letgo_core()`) branch offers
  `special_forms(false)` matching the prefix, alongside the existing `core_symbols`
  pool; the let-go branch offers `special_forms(true)` (was iterating the static
  `SPECIAL_FORMS`).

### Module rename

`src/handlers/letgo_builtins.rs` → `src/handlers/builtins.rs`, since it now serves
both dialects and `SpecialForm` needs a dialect-neutral home. Let-go natives
(`is_native`/`native_names`/`NATIVE_NAMES` via `letgo_native_names`) stay in it.
The `throw` doc is reworded to drop the let-go-specific aside (it is now shared).

## File Structure

- **Rename `src/handlers/letgo_builtins.rs` → `src/handlers/builtins.rs`** — restructure
  the special-forms data into `COMMON_SPECIAL_FORMS` + `LETGO_EXTRA` +
  `CLOJURE_EXTRA`; `pub fn special_form(name: &str, letgo: bool) -> Option<&'static
  SpecialForm>`; `pub fn special_forms(letgo: bool) -> impl Iterator<Item =
  &'static SpecialForm>` (for completion); keep `is_native` / `native_names`.
- **Modify `src/handlers/mod.rs`** — `mod builtins` (was `letgo_builtins`); enum
  variant type `builtins::SpecialForm`; let-go branch → `special_form(word, true)`;
  Clojure branch → add `special_form(word, false)` check before `core_symbols`.
- **Modify `src/handlers/hover.rs`** — `super::builtins::SpecialForm` in
  `format_for_special_form`; update the test reference. (No new logic.)
- **Modify `src/handlers/completion.rs`** — reference `builtins::`; let-go branch
  iterates `special_forms(true)`; Clojure branch adds `special_forms(false)`.
- **Modify `tests/test_e2e.rs`** (+ a `simple_project` fixture source file) — a
  Clojure hover-on-`if` assertion.
- **Modify `docs/ROADMAP.md`** — note special-forms hover/completion now covers
  Clojure too.

Reuse the existing `SpecialForm`, `ResolvedSymbol::SpecialForm`, hover
`format_for_special_form`, the completion helper `special_form_to_completion`, and
`Index::letgo_core()`. No new dependencies; no runtime process spawning.

---

## Tasks

### Task 1: make the special-forms table dialect-aware (rename + restructure)

Pure refactor — let-go behavior must stay identical; Clojure not wired yet.

**Files:** rename `letgo_builtins.rs` → `builtins.rs`; modify `mod.rs`, `hover.rs`,
`completion.rs`.

- [x] **Step 1:** `git mv src/handlers/letgo_builtins.rs src/handlers/builtins.rs`.
- [x] **Step 2:** In `builtins.rs`, split the current `SPECIAL_FORMS` into
  `COMMON_SPECIAL_FORMS` (the 14: `if do def set! fn* quote var let* loop* recur try
  catch finally throw`) + `LETGO_EXTRA` (`trace`) + `CLOJURE_EXTRA` (`.`, `new`,
  `monitor-enter`, `monitor-exit`). Reword the `throw` doc to be dialect-neutral
  (drop the "let-go implements it as a native fn" aside). Add CLOJURE_EXTRA entries:
  `.` `(. instance-or-Class member args*)` "Java interop member access (method call
  or field)."; `new` `(new Class args*)` "Constructs a Java object; reader form
  `(Class. args*)`."; `monitor-enter` `(monitor-enter x)` "Acquires x's monitor
  lock (low-level; prefer `locking`)."; `monitor-exit` `(monitor-exit x)` "Releases
  x's monitor lock (low-level; prefer `locking`).".
- [x] **Step 3:** Replace `special_form(name)` with `special_form(name: &str,
  letgo: bool)` (search `COMMON` then `if letgo { LETGO_EXTRA } else { CLOJURE_EXTRA
  }`), and add `special_forms(letgo: bool) -> impl Iterator<Item = &'static
  SpecialForm>` (`COMMON.iter().chain(extra.iter())`).
- [x] **Step 4:** Update references: `mod.rs` (`mod builtins`; enum variant
  `builtins::SpecialForm`; let-go branch `builtins::special_form(word, true)`),
  `hover.rs` (`super::builtins::SpecialForm`), `completion.rs` (`builtins::`;
  let-go branch iterates `builtins::special_forms(true)` instead of the old static).
  Update existing tests' `special_form(...)` calls to pass `true`, and the module
  paths in `hover.rs`/`handlers` tests.
- [x] **Step 5: Unit tests** (in `builtins.rs`): `special_form("if", true)` and
  `special_form("if", false)` both `Some`; `special_form("trace", true)` `Some` but
  `("trace", false)` `None`; `special_form("new", false)` `Some` but `("new", true)`
  `None`; `special_forms(false)` contains `new` and not `trace`.
- [x] **Step 6:** `cargo test --lib handlers` → PASS (let-go tests unchanged);
  `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --all`.
- [x] **Step 7:** `git commit -m "Make special-forms table dialect-aware (rename letgo_builtins -> builtins)"`

### Task 2: resolve + complete special forms in Clojure projects

**Files:** modify `src/handlers/mod.rs`, `src/handlers/completion.rs`.

- [x] **Step 1:** In `resolve_symbol`'s Clojure (non-`letgo_core`) bare-word path,
  after the current-ns / refer / factory lookups and before the `core_symbols`
  fallback: `if let Some(sf) = builtins::special_form(word, false) { return
  Some(ResolvedSymbol::SpecialForm(sf)); }`.
- [x] **Step 2:** In `completion.rs`, the `else` (Clojure) branch of the
  builtins pool: also push `builtins::special_forms(false)` whose `name` starts
  with the prefix, via the existing `special_form_to_completion`.
- [x] **Step 3: Unit tests** (`handlers` + `completion`): in a non-let-go `Index`
  (no `mark_letgo_core`) with a `count` `CoreSymbol`, `resolve_symbol(index, "if",
  "app")` → `SpecialForm("if")`, while `resolve_symbol(index, "count", "app")` stays
  `Core`; a project var named `new` still resolves to `Project`, not the special
  form; completion for prefix `"i"` offers `if`.
- [x] **Step 4:** `cargo test --lib handlers` → PASS; clippy clean; `cargo fmt`.
- [x] **Step 5:** `git commit -m "resolve_symbol + completion: Clojure special forms"`

### Task 3: e2e (Clojure hover)

**Files:** modify a `tests/fixtures/simple_project` source file and `tests/test_e2e.rs`.

- [x] **Step 1:** Add a top-level `(if true 1 2)` form to an existing
  `simple_project` `.clj` source file (inspect the fixture for the right file).
- [x] **Step 2: e2e** (`test_e2e.rs`, e.g. `test_e2e_clojure_special_form_hover`):
  open that file, hover on `if` → markdown contains `"special form"`; goto-def on
  `if` returns null (no navigation). (A clojure.core fn like `map`/`inc` continuing
  to hover as `clojure.core` may also be asserted.)
- [x] **Step 3:** `cargo test --test test_e2e` → PASS, then `bb check && bb e2e`
  → PASS.
- [x] **Step 4:** `git commit -m "e2e: Clojure special-form hover"`

### Task 4: ROADMAP note

**Files:** modify `docs/ROADMAP.md`.

- [x] **Step 1:** Note that special-forms hover/completion now covers Clojure
  projects too (not just let-go) — the table is dialect-aware.
- [x] **Step 2:** `git commit -m "Roadmap: note Clojure special-forms hover/completion"`

---

## Notes & limitations

- **No goto-def for special forms** — they have no source; goto-def is a
  deliberate no-op (unchanged from the let-go feature).
- **Dialect via `letgo_core()`**: an unpinned let-go project gets the Clojure
  extras and misses `trace` — accepted edge; the shared `COMMON` forms are correct.
- **Macros are not special forms**: `let`/`fn`/`loop`/`when`/`cond`/… stay served
  by the `core_symbols()` clojure.core table (hover + navigate), not this table.
- **Cache-version note:** none — no jar cache or extractor output changes.

## Implementation summary

Implemented as designed, in four commits on `lg-core-navigation`:

1. **`42487dc`** — renamed `letgo_builtins.rs` → `builtins.rs` and made the
   special-forms table dialect-aware: `COMMON_SPECIAL_FORMS` (14) + `LETGO_EXTRA`
   (`trace`) + `CLOJURE_EXTRA` (`.`/`new`/`monitor-enter`/`monitor-exit`);
   `special_form(name, letgo)` + `special_forms(letgo)`. Pure refactor — let-go
   behavior unchanged.
2. **`88f584a`** — `resolve_symbol`'s Clojure path resolves special forms (before
   the `core_symbols` fallback, after project lookups so a project var named `new`
   still wins); the Clojure completion branch offers them too.
3. **`1f6fbfe`** — e2e: in a Clojure project, hover on `if` → "special form",
   goto-def on `if` → no-op, and `map` still hovers as clojure.core.
4. **`7e488cf`** — ROADMAP note.

clojure.core fns/macros were already covered by the static `core_symbols()` table
+ the clojure JAR, so no "native fns" work was needed for Clojure — special forms
were the whole gap.

**Deviations / notes:**

- *Removed an obsolete test* (`without_letgo_marker_special_form_is_not_resolved`)
  whose premise — marker off ⇒ `if` unresolved — is exactly the behavior Task 2
  intentionally changes; the new `clojure_special_form_resolves_for_hover` covers
  the updated behavior.
- *e2e fixture*: the plan said to add an `if` to a `simple_project` source file,
  but `test_e2e_document_symbols_outline` asserts `core.clj`'s symbols *exactly*,
  so instead the test writes its own scratch `.clj` at runtime (isolated; no
  committed-fixture change). `core.clj` was reverted.
- *Tooling*: the first review attempt mis-launched codex (an inner `&` inside a
  backgrounded call); recovered with a file-waiter. The Task 1 commit initially
  captured only the rename (a stale `git add` pathspec aborted staging) — amended
  to include the content changes.

**Codex reviews:** Task 1's two findings ("wire Clojure resolution/completion")
were exactly Task 2's planned scope — independent confirmation, not defects.
Tasks 2–3 came back clean. No must-fix items.

**Verification.** `bb check` (fmt + clippy `-D warnings` + 145 lib + integration
tests) and `bb e2e` (54 passed, 1 ignored) both green. Special forms now resolve
for both dialects; Clojure projects gained hover + completion for `if`/`do`/`try`/
`new`/… with goto-def a deliberate no-op.
