# let-go Builtins: Hover & Completion for Special Forms and Native Core Fns

> **Status: ✅ Completed (2026-06-18).** All five tasks implemented, codex-reviewed
> per task, and verified with `bb check` + `bb e2e`. See the
> [Implementation summary](#implementation-summary) at the end.

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In a let-go project, give hover and completion for let-go's built-in
**special forms** (`if`, `do`, `try`, `catch`, …) and **native core functions**
(`throw`, `count`, `subs`, `str`, …) — the forms that are implemented in let-go's
Go runtime/compiler and therefore have no `.lg` source to navigate into. Goto-def
on them is a correct no-op (there is nothing to navigate to); the value is the
description on hover and the names in completion.

**Tech stack:** Rust; the existing `resolve_symbol` / hover / completion handlers;
the `letgo_core` marker; the existing static `core_symbols()` clojure.core table.

---

## Design

This builds on the let-go core navigation already shipped (`index_letgo_core`,
the `Index::letgo_core()` marker, and the `resolve_symbol` let-go branch). Today,
a bare word in a let-go file resolves to the live `.lg` `core` namespace or to
nothing. Special forms and native fns currently resolve to nothing → no hover.

### Two categories, two static tables

let-go's built-ins that have no `.lg` source split into two groups, confirmed
against let-go 1.10.0:

1. **Special forms** — compiler intrinsics from `pkg/compiler/compiler.go`'s
   `specialForms` dispatch map: `if do def set! fn* quote var let* loop* recur
   trace try`, plus `catch` / `finally` (parsed inside `try`). These are not vars
   (`resolve` cannot see them), so they get a small **hand-authored** table of
   `(name, usage, doc)`.

2. **Native core functions** — registered in Go via `ns.Def("name", goFn)` (e.g.
   `count`, `subs`, `str`, `throw`, `reduce`). At runtime their type is
   `let-go.lang.NativeFn`, vs `let-go.lang.Fn` for `.lg`-defined fns. They carry
   **no** doc/arglist metadata in let-go. We get their richness by **reusing the
   existing `core_symbols()` clojure.core table** (let-go core mirrors Clojure
   semantics): a native named `count` borrows clojure.core's arglists + docstring,
   labelled *"let-go core (native)"*.

### Runtime is pure static — no scripts, ever

At clj-pulse runtime there is **no process spawning**. Both tables are compiled-in
static values (exactly like the existing `core_symbols()` array), and resolution
is in-memory lookup. The only thing that ever runs `lg` is a **committed, offline,
developer-run generator** that produces the native-names list once per supported
let-go version — never on any runtime path.

### How the native-names list is generated (offline, no Go parsing)

A committed dev script feeds the clojure.core names (already in `core_symbols()`)
to the installed `lg` and keeps those whose `(type (resolve (symbol name)))` is
`NativeFn`. That yields the authoritative native set straight from let-go's own
runtime — no Go is read. Output is a generated, committed
`src/handlers/letgo_native_names.rs` holding `pub static NATIVE_NAMES: &[&str]`
(sorted). `.lg`-defined names (type `Fn`, e.g. `map`/`when`/`filter`) and names
absent from this let-go build (`nil`) are excluded — they are served by the live
`.lg` index, not this list.

### Resolution, hover, completion

- **`resolve_symbol`** (let-go branch, after the existing live `core` lookup):
  live `.lg` `core` def (navigable, wins) → **special form** → **native** → `None`.
  Two new `ResolvedSymbol` variants carry the result.
- **Hover** renders both: special form → usage + *special form* + doc; native →
  `(name arglists)` + *let-go core (native)* + the borrowed doc.
- **goto-def & signature-help** are correct no-ops for both variants (no source,
  no real arglist contract for special forms).
- **Completion** (let-go projects only): the clojure.core static pool (Pool C) is
  replaced by the let-go-correct set — special forms + native fns + the live `.lg`
  `core` namespace symbols — which avoids duplicate/mislabelled entries and offers
  exactly what a let-go file can call. Clojure projects are unchanged.

### Scope & caveats (agreed)

- **Native coverage = clojure.core names that let-go implements natively.** let-go
  has no `ns-publics`, so we can only probe known names; the user-facing natives are
  all clojure.core names, so this covers the real cases. let-go-specific natives
  outside clojure.core (e.g. `apply*`) are out of scope (hand-addable later).
- **Borrowed docs are "Clojure-equivalent."** Reusing clojure.core prose assumes
  let-go's fn matches Clojure — true in the vast majority; labelled clearly.
- **Versioning**: both tables are a snapshot (generated against the installed
  let-go). Drift only affects *informational* hover/completion, never navigation —
  same tradeoff as the existing clojure.core table. The generator is committed for
  a one-command refresh.
- **Let-go-gated**: all new behavior is behind `index.letgo_core()`. Clojure
  projects are untouched (hover/completion/resolve unchanged when the marker is off).

## File Structure

- **New `src/handlers/letgo_builtins.rs`** — `pub struct SpecialForm { name,
  usage, doc: &'static str }`, `pub static SPECIAL_FORMS: &[SpecialForm]` (the
  hand-authored table), `pub fn special_form(name) -> Option<&'static SpecialForm>`,
  and `pub fn is_native(name) -> bool` (membership in the generated
  `NATIVE_NAMES`). `mod`/`use` the generated names file.
- **New `src/handlers/letgo_native_names.rs`** — GENERATED. Header comment marking
  it generated + the refresh command. `pub static NATIVE_NAMES: &[&str] = &[…]`
  (sorted).
- **New `scripts/gen_letgo_native_names.sh`** (+ a `bb gen-letgo-natives` task in
  `bb.edn`) — the offline generator. Dev-only; never invoked by clj-pulse.
- **Modify `src/handlers/mod.rs`** — `pub mod letgo_builtins;`; add
  `ResolvedSymbol::SpecialForm(&'static letgo_builtins::SpecialForm)` and
  `ResolvedSymbol::LetgoNative(CoreSymbol)`; extend the `letgo_core()` branch of
  `resolve_symbol` with the special-form then native fallbacks.
- **Modify `src/handlers/hover.rs`** — `format_for_special_form`,
  `format_for_letgo_native`, and the two new match arms.
- **Modify `src/handlers/definition.rs`** — match arm: both new variants → `Ok(None)`.
- **Modify `src/handlers/signature.rs`** — match arm: both new variants → `Ok(None)`.
- **Modify `src/handlers/completion.rs`** — gate the clojure.core pool on
  `!letgo_core()`; add a let-go pool (special forms + natives + live `core` ns).
- **Modify `tests/test_e2e.rs`** and **`tests/fixtures/letgo_core_project/src/app.lg`**
  — add `if` and `count` usages; assert hover content.
- **Modify `docs/ROADMAP.md`** — note hover/completion for let-go builtins.

Reuse the existing `CoreSymbol`, `core_symbols()`, `Index::letgo_core()`,
`resolve_symbol`, the hover `format_for_*` pattern, and the completion pools. No
new dependencies; no runtime process spawning.

---

## Tasks

### Task 1: special-forms table + resolver/hover/no-op wiring

**Files:** new `src/handlers/letgo_builtins.rs`; modify `src/handlers/mod.rs`,
`hover.rs`, `definition.rs`, `signature.rs`.

- [x] **Step 1:** Create `src/handlers/letgo_builtins.rs` with the `SpecialForm`
  struct and `SPECIAL_FORMS` table (one entry per form below, each with a `usage`
  string and a one/two-line `doc`), plus `pub fn special_form(name) ->
  Option<&'static SpecialForm>` (linear scan). Forms (let-go 1.10.0 compiler):
  `if` `(if test then else?)`; `do` `(do exprs*)`; `def` `(def sym doc? init?)`;
  `set!` `(set! place expr)`; `fn*` `(fn* [params*] exprs*)` (note: prefer the
  `fn` macro); `quote` `(quote form)` (reader `'form`); `var` `(var sym)` (reader
  `#'sym`); `let*` `(let* [bindings*] exprs*)` (prefer `let`); `loop*`
  `(loop* [bindings*] exprs*)` (prefer `loop`); `recur` `(recur exprs*)`; `trace`
  `(trace exprs*)` (let-go VM instruction tracing — let-go extension); `try`
  `(try body* (catch sym handler*)? (finally cleanup*)?)`; `catch`
  `(catch binding-sym body*)`; `finally` `(finally body*)`. (`throw` is **not**
  here — it is a native fn, handled in Task 2.)
- [x] **Step 2:** In `src/handlers/mod.rs`: `pub mod letgo_builtins;`; add
  `ResolvedSymbol::SpecialForm(&'static letgo_builtins::SpecialForm)`. In
  `resolve_symbol`'s `if index.letgo_core()` block, after the existing
  `lookup_in_ns("core", word)` Project return, add: if `letgo_builtins::
  special_form(word)` is `Some`, return `ResolvedSymbol::SpecialForm`. (Keep the
  trailing `return None` so the static clojure.core list is still skipped.)
- [x] **Step 3:** In `hover.rs`: add `format_for_special_form(&SpecialForm) ->
  String` (` ```clojure\n{usage}\n``` ` + `*special form*` + blank line + doc) and
  a `ResolvedSymbol::SpecialForm(sf) => Some(format_for_special_form(sf))` arm.
- [x] **Step 4:** In `definition.rs` and `signature.rs`: add a
  `Some(ResolvedSymbol::SpecialForm(_)) => …` arm returning `Ok(None)` (no
  navigation / no signature for special forms).
- [x] **Step 5: Unit tests** (`letgo_builtins` and `handlers` `#[cfg(test)]`):
  `special_form("if")` is `Some`, `special_form("nope")` is `None`; with
  `letgo_core` set, `resolve_symbol(index,"if","app")` → `SpecialForm("if")`, and
  with the marker unset it is unchanged (`None`/static); `format_for_special_form`
  contains `"special form"` and the usage. (Codex review: clean.)
- [x] **Step 6:** `cargo test --lib handlers` → PASS; `cargo clippy --all-targets
  -- -D warnings` → clean.
- [x] **Step 7:** `git commit -m "Hover for let-go special forms"`

### Task 2: native core fns — generate list + resolver/hover

**Files:** new `scripts/gen_letgo_native_names.sh` + `bb.edn` task; new (generated)
`src/handlers/letgo_native_names.rs`; modify `src/handlers/letgo_builtins.rs`,
`mod.rs`, `hover.rs`, `definition.rs`, `signature.rs`.

> **Deviation:** `throw` is a let-go native fn but **not** a clojure.core var, so
> it is absent from `core.rs` and the generator cannot probe it (Step 2's
> "contains throw" expectation was wrong). `throw` is instead covered by the
> hand-authored `SPECIAL_FORMS` table (Task 1) — which is how Clojure tooling
> presents it anyway. Codex review also fixed: (1) goto-def on a require alias
> that collides with a native name (e.g. `[clojure.string :as str]`) now mirrors
> the `Core` arm's `on_alias_declaration` handling; (2) the generator uses a
> portable `mktemp -d` (BSD/macOS lacks `--suffix`); (3) `cargo fmt` on the test.

- [x] **Step 1: Generator.** Write `scripts/gen_letgo_native_names.sh` (wired as a
  `bb gen-letgo-natives` task). It must: (a) extract clojure.core names from
  `src/index/core.rs` (`grep -oE 'name: "…"'`); (b) emit a temporary `.lg` that,
  for each name as a **string**, prints `name|TYPE` via
  `(type (resolve (symbol s)))` (string input avoids reader edge cases like `+`,
  `*'`, `..`); (c) run the installed `lg`; (d) keep names whose type contains
  `NativeFn`; (e) write sorted output to `src/handlers/letgo_native_names.rs` as
  `pub static NATIVE_NAMES: &[&str] = &[…];` with a generated-file header comment
  (including the refresh command). Document that it is dev-only and never run by
  clj-pulse.
- [x] **Step 2:** Run `bb gen-letgo-natives`. Sanity-check the output: it contains
  `"count"`, `"subs"`, `"str"`, `"throw"`, `"reduce"`; it does **not** contain
  `"map"`, `"when"`, `"filter"` (those are `.lg` `Fn`s); the array is sorted.
- [x] **Step 3:** In `letgo_builtins.rs`: `mod`/`use` `letgo_native_names`; add
  `pub fn is_native(name: &str) -> bool` (e.g. `NATIVE_NAMES.binary_search(&name)
  .is_ok()`).
- [x] **Step 4:** In `mod.rs`: add `ResolvedSymbol::LetgoNative(CoreSymbol)`. In
  `resolve_symbol`'s let-go block, after the special-form fallback: if
  `letgo_builtins::is_native(word)`, find the matching `CoreSymbol` in
  `index.core_symbols` by name and return `ResolvedSymbol::LetgoNative(core.clone())`.
- [x] **Step 5:** In `hover.rs`: add `format_for_letgo_native(&CoreSymbol)` (like
  `format_for_core` but labelled `*let-go core (native)*`) and the match arm. In
  `definition.rs`/`signature.rs`: add `LetgoNative(_) => Ok(None)` arms (combine
  with the `SpecialForm` arm).
- [x] **Step 6: Unit tests**: `is_native("count")` true, `is_native("map")` false;
  with `letgo_core` set and a `count` `CoreSymbol` present, `resolve_symbol(index,
  "count","app")` → `LetgoNative` whose name is `count`; `format_for_letgo_native`
  contains `"let-go core (native)"` and the arglists.
- [x] **Step 7:** `cargo test --lib handlers` → PASS; `cargo clippy --all-targets
  -- -D warnings` → clean.
- [x] **Step 8:** `git commit -m "Hover for let-go native core functions"`

### Task 3: completion for let-go builtins

**Files:** modify `src/handlers/completion.rs`.

- [x] **Step 1:** In `complete_symbols`, the unqualified (bare-prefix) branch: gate
  the existing clojure.core pool (Pool C) on `!index.letgo_core()`. When
  `index.letgo_core()`, add instead: (a) `SPECIAL_FORMS` whose `name` starts with
  the prefix (completion kind `KEYWORD`, detail `"special form"`, usage as
  documentation); (b) `NATIVE_NAMES` whose name starts with the prefix, each
  rendered from its `core_symbols()` entry with detail `"let-go core (native)"`;
  (c) the live `core` namespace symbols (`index.ns_symbols.get("core")`) whose name
  starts with the prefix, via the existing `symbol_to_completion`.
- [x] **Step 2: Unit test** (`completion` `#[cfg(test)]`): build a let-go-marked
  `Index` (with a `count` `CoreSymbol` and a `.lg` `core/map`); completing prefix
  `"i"` offers `if`; `"cou"` offers `count` (detail mentions native); `"ma"` offers
  `map`; and a clojure.core-only name absent from let-go (not in `NATIVE_NAMES`,
  e.g. `agent`) is **not** offered. With the marker unset, the clojure.core pool is
  used as before.
- [x] **Step 3:** `cargo test --lib handlers::completion` → PASS; clippy clean.
- [x] **Step 4:** `git commit -m "Completion offers let-go special forms and native core fns"`

### Task 4: e2e + fixture

**Files:** modify `tests/fixtures/letgo_core_project/src/app.lg`, `tests/test_e2e.rs`.

- [x] **Step 1:** Add to `app.lg` an `if` and a `count` usage (e.g.
  `(if true (count []) 0)`), keeping the existing `map`/`str/join` lines.
- [x] **Step 2: e2e** in `test_e2e.rs` (extend `test_e2e_letgo_core_navigation` or
  add `test_e2e_letgo_builtins_hover`): after indexing, hover on `if` →
  markdown contains `"special form"`; hover on `count` → contains `"let-go core
  (native)"`. (Goto-def on `if`/`count` returning no location may also be asserted.)
- [x] **Step 3:** `cargo test --test test_e2e letgo` → PASS, then `bb check && bb
  e2e` → PASS.
- [x] **Step 4:** `git commit -m "e2e: hover let-go special forms and native core fns"`

### Task 5: ROADMAP note

**Files:** modify `docs/ROADMAP.md`.

- [x] **Step 1:** Extend the Phase 5 "let-go core navigation" item: hover and
  completion now also cover let-go special forms and native core functions (no
  navigation — they have no `.lg` source).
- [x] **Step 2:** `git commit -m "Roadmap: note let-go builtins hover/completion"`

---

## Notes & limitations

- **No runtime scripts.** Tables are compiled-in static values; the `lg`-based
  generator is offline/dev-only (committed in `scripts/`), run once per supported
  let-go version.
- **No goto-def for builtins** — special forms and native fns have no `.lg`
  source; goto-def is a deliberate no-op (same stance as Clojure special forms).
- **Native docs are Clojure-equivalent**, borrowed from the clojure.core table.
- **Cache-version note:** none — no jar cache or extractor output changes.

## Implementation summary

Implemented as designed, in five commits on `lg-core-navigation`:

1. **`522947f`** — `src/handlers/letgo_builtins.rs`: `SpecialForm` + `SPECIAL_FORMS`
   table + `special_form()`; `ResolvedSymbol::SpecialForm`; resolver fallback;
   hover format; goto-def/signature no-ops.
2. **`f902865`** — generator `scripts/gen_letgo_native_names.sh` (+ `bb
   gen-letgo-natives`); committed `letgo_native_names.rs` (227 names);
   `is_native()`; `ResolvedSymbol::LetgoNative`; native resolver fallback + hover.
3. **`7bc9bba`** — completion: clojure.core pool gated to non-let-go; let-go gets
   special forms + natives + live `.lg` `core` ns.
4. **`a61d483`** — e2e: hover `if` → special form, hover `count` → native,
   goto-def `if` → no-op, and the `str` alias still navigates.
5. **`23fe76c`** — ROADMAP note.

**Runtime is pure static** as required: no process spawning; the `lg` generator is
offline/dev-only and its output is a committed static array.

**Deviation from the written plan:** `throw` is a let-go native fn but **not** a
clojure.core var, so the generator (which probes clojure.core names) can't find
it and Step 2's "contains throw" expectation was wrong. `throw` is instead covered
by the hand-authored special-forms table — which matches how Clojure tooling
presents it. Documented inline in Task 2.

**Findings fixed during the per-task codex reviews:**

- *Alias-definition navigation regression* — a require alias colliding with a
  native name (`[clojure.string :as str]`) resolved as `LetgoNative` and the
  no-op arm returned `None` before the alias fallback. Fixed by mirroring the
  `Core` arm's `on_alias_declaration` handling; locked with an e2e assertion.
- *Non-portable `mktemp --suffix`* (fails on the maintainer's macOS) → portable
  `mktemp -d` + a `.lg` file inside it.
- *rustfmt* on new test assertions (twice — `bb check` runs fmt before tests).

**Verification.** `bb check` (fmt + clippy `-D warnings` + 142 lib + integration
tests) and `bb e2e` (53 passed, 1 ignored) both green, including the new
`test_e2e_letgo_builtins_hover`. All behavior is gated on `Index::letgo_core()`,
so Clojure projects are unaffected (covered by the marker-off unit tests).
