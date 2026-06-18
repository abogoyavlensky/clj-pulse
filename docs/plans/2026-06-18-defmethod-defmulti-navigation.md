# Goto-Definition from `defmethod` to its `defmulti`

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Goto-definition on the multimethod name in a `(defmethod m/foo …)`
form should navigate to the `(defmulti foo …)` declaration — for both Clojure
(`.clj`) and let-go (`.lg`) projects. Today it navigates to the defmethod itself
(a no-op for the user) instead of the multimethod.

**Tech stack:** Rust; the existing `resolve_fqn_at` resolver and the stdio e2e
harness.

---

## Design

For `(defmethod ig/init-key ::db …)`, `extract_def` creates a **self-referential
Symbol** for the defmethod: name `init-key`, fqn `<current-ns>/init-key`,
`name_range` on the `init-key` token, pointing at the defmethod form.
`resolve_fqn_at` (shared by goto-def, references, and rename) scans
**definition name_ranges before occurrence name_ranges**, so a cursor on
`ig/init-key` matches the defmethod's own symbol and returns its self-fqn —
goto-def lands back on the defmethod, never the `defmulti`.

However, the multimethod name is **also recorded as an occurrence** that resolves
the alias correctly (`ig` → `integrant.core` → `integrant.core/init-key`, see
`extractor.rs` `record_occurrence`). Protocol-method *implementations* already
navigate to their declaration (`test_e2e_definition_on_protocol_method_impl`)
precisely because they have **no competing self-symbol** — `defmethod` is the
asymmetric case that creates one.

**Fix:** in `resolve_fqn_at`, skip `Defmethod` symbols in the definition-name
loop, so the multimethod occurrence resolves instead. A `defmethod` head names
the multimethod it extends — it is not a new definition. Goto-def then lands on
the `defmulti` (when indexed); references/rename likewise target the multimethod
(renaming a *library* multimethod is correctly rejected; a *project-local* one
renames across the `defmulti` and all `defmethod` heads).

### Dialects

The fix is dialect-agnostic: `.lg` files use the same extractor (`str_to_defkind`
maps `defmethod`/`defmulti` regardless of dialect) and the same `resolve_fqn_at`
(no `letgo_core()` gating). let-go supports multimethods (`defmulti`/`defmethod`
are macros in its `core.lg`). So the same fix covers `.lg` automatically; an e2e
variant locks it in.

### Key decisions

- **Keep the `defmethod` Symbol in extraction; change only resolution.** The
  symbol still populates the document outline (`symbols.rs` maps `Defmethod` →
  `METHOD`, matching clojure-lsp). Removing it would regress the outline, so the
  fix is in `resolve_fqn_at`, not the extractor — which also keeps
  `test_extractor.rs` green.
- **Resolves to the `defmulti`.** Works for library multimethods (e.g. integrant,
  when its jar is indexed) and project-local/let-go ones alike. When the target
  isn't indexed, it degrades to no navigation — same as any unindexed symbol, no
  worse than today.

## File Structure

- **Modify `src/handlers/references.rs`** — in `resolve_fqn_at`, skip
  `Defmethod` symbols in the definition-name loop; import `DefKind`.
- **Modify `tests/test_e2e.rs`** — `test_e2e_definition_on_defmethod` (Clojure)
  and `test_e2e_definition_on_defmethod_letgo` (let-go), mirroring
  `test_e2e_definition_on_protocol_method_impl`.
- **Modify `docs/ROADMAP.md`** — note `defmethod` → `defmulti` navigation.

Reuse `resolve_fqn_at`, the occurrence machinery, and the scratch-file e2e
pattern. No extractor change; no new dependencies.

---

## Tasks

### Task 1: resolve_fqn_at skips the `defmethod` self-symbol

**Files:** modify `src/handlers/references.rs`.

- [ ] **Step 1:** In `resolve_fqn_at`'s first loop (`for sym in &syms`), `continue`
  when `sym.kind == DefKind::Defmethod`, with a comment that a `defmethod` head
  names the multimethod it extends (resolved via the occurrence below), not a new
  definition. Import `DefKind` (`use crate::index::{..., DefKind, ...}`).
- [ ] **Step 2:** `cargo test --lib` → PASS; `cargo clippy --all-targets -- -D
  warnings` clean; `cargo fmt --all`.
- [ ] **Step 3:** `git commit -m "resolve_fqn_at: defmethod head resolves to its defmulti"`

### Task 2: e2e — Clojure and let-go

**Files:** modify `tests/test_e2e.rs`.

- [ ] **Step 1 (Clojure):** `test_e2e_definition_on_defmethod`, mirroring
  `test_e2e_definition_on_protocol_method_impl`: write `src/multi_def.clj`
  (`(ns app.multi)\n(defmulti area :kind)\n`) and `src/multi_impl.clj`
  (`(ns app.impl\n  (:require [app.multi :as m]))\n(defmethod m/area :circle [x] (:r x))\n`)
  into a `setup_project()` copy; `start`, `initialize`, `wait_for_log("Indexed")`,
  `did_open` the impl. goto-def on the `area` of `m/area` → uri ends
  `/src/multi_def.clj` and `range.start.line` equals the `defmulti area` line.
- [ ] **Step 2 (let-go):** `test_e2e_definition_on_defmethod_letgo`: a let-go
  project (`setup_named("letgo_core_project")`), `start_with_env` with `LGX_HOME`
  pointed at an empty `root/lgxhome` (hermetic — no core indexed); write
  `src/mdef.lg` (`(ns mdef)\n(defmulti area :kind)\n`) and `src/mimpl.lg`
  (`(ns mimpl\n  (:require [mdef :as m]))\n(defmethod m/area :circle [x] (:r x))\n`);
  `initialize`, `wait_for_log("Indexed")`, `did_open` the impl. goto-def on the
  `area` of `m/area` → uri ends `/src/mdef.lg` at the `defmulti area` line.
- [ ] **Step 3:** `cargo test --test test_e2e defmethod` → PASS, then `bb check &&
  bb e2e` → PASS.
- [ ] **Step 4:** `git commit -m "e2e: defmethod head navigates to defmulti (Clojure + let-go)"`

### Task 3: ROADMAP note

**Files:** modify `docs/ROADMAP.md`.

- [ ] **Step 1:** Note goto-def from a `defmethod` to its `defmulti` (both
  dialects), alongside the existing protocol-impl→declaration item.
- [ ] **Step 2:** `git commit -m "Roadmap: note defmethod -> defmulti navigation"`

---

## Notes & limitations

- **Outline unchanged:** `defmethod` symbols are still extracted and listed in
  document/workspace symbols; only navigation resolution changes.
- **Library targets need indexing:** navigating to a library `defmulti` (e.g.
  integrant) requires its jar to be on the indexed classpath; otherwise goto-def
  degrades to no navigation (unchanged for unindexed libs).
- **References/rename** on a multimethod name now target the multimethod
  (a bonus of the shared `resolve_fqn_at` path), consistent with the occurrence.
- **Cache-version note:** none — no jar cache or extractor output changes.
