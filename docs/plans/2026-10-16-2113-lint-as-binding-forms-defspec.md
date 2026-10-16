# `:lint-as` Binding Forms, `defspec` and `def-catch-all` Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A macro that `:lint-as` maps to a core binding form (`metabase.util/for-map` → `clojure.core/for`) binds its locals the way the target does, in both the occurrence walker and the locals walker. `clojure.test.check.clojure-test/defspec` and the `clj-kondo.lint-as/def-catch-all` target define the var they name.

**Evidence:** `bb compare metabase`, 2026-10-16 (`docs/MEMORY.md`, "metabase, first run"). `{:keys [name base_type]}` inside `u/for-map` at `enterprise/backend/test/metabase_enterprise/transforms_python/drivers_test.clj:335` is no local, so references on `name` answer all 1390 `clojure.core/name` sites. 29 `var-def/defspec` rename probes are refused, and `define-migration` (`def-catch-all`) vars refuse rename too. metabase's config maps about 70 macros to core binding and def forms.

---

## Design

`settings::merge` keeps a second table, `ExtractConfig::lint_as_forms`: macro fqn → the name of a core binding form, for targets in `clojure.core` / `cljs.core` (or bare) whose name is one the walkers bind on: `is_let_like` names, `fn`, `letfn`, `as->`, `reify`, `extend-type`, `extend-protocol`. Def-family targets keep going to `lint_as` as before; anything else is still dropped.

Both walkers resolve a `sym_lit` head through `head_fqn_candidates` against that table *before* the core-form check (`lint_as_form`), and when it hits, use the target name as `head_text`. The arms already record the head through `record_occurrence`, so `u/for-map` stays a usage of `metabase.util/for-map`. `OccurrenceCtx` and `ScopeCtx` carry the table beside `lint_as`.

`DefKind::from_def_symbol` maps `def-catch-all` to `Def` (kondo's semantics: the first symbol names a var; the body is not checked, which for us means its symbols are ordinary usages). `DefKind::from_macro_fqn` maps `clojure.test.check.clojure-test/defspec` to `Def`.

Extractor output changes for files using these macros, so `CACHE_FORMAT_VERSION` is bumped.

## Tasks

- [ ] `ExtractConfig::lint_as_forms`, filled by `settings::merge`; settings tests for kept and dropped targets.
- [ ] `lint_as_form` resolver; `walk_list` and `walk_scope` consult it before the core check.
- [ ] `def-catch-all` and `defspec` def kinds.
- [ ] Extractor tests: references, rename and unused-binding through a `:lint-as` `for` and `let`; `defspec` and a `def-catch-all` macro define their var.
- [ ] Bump `CACHE_FORMAT_VERSION`; update CLAUDE.md invariants and docs/SETTINGS.md (`:lint-as` now honors binding-form targets).
- [ ] `bb check`, `bb e2e`, `bb compare` (clj-kondo must not regress; metabase `var-def/defspec`, `var-def/other` and the `name` local should move).
