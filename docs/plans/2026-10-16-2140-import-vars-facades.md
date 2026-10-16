# `potemkin/import-vars` Facades Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A var re-exported through a potemkin-style facade (`import-vars`, `import-fn`, `import-def`, `import-macro`, and macros `:lint-as`'d to them) navigates, hovers, completes and is found by references and rename from every spelling: `mu/defn`, `t2/select`, `mt/with-temp` land on the original definition, references on the original list the callers that went through a facade, and a rename of the original rewrites every token that spells it.

**Status:** planned, not started. Reopens the 2026-09-30 rejection (ROADMAP *Not planned*, [archived issue](../archive/2026-09-17-import-vars-re-exports-are-not-definitions.md)), whose reason was "a facade pattern the corpora use in two places". The first `bb compare metabase` run (2026-10-16, `docs/MEMORY.md`) counts 124 `p/import-vars` forms, 26 `p/import-def`, 8 `p/import-fn` and 43 `metabase.util.namespaces/import-fn(s)` (`:lint-as … potemkin/import-vars`), and toucan2 (`t2/…`, a library) is built on it. About 900 of that run's 2043 new answers are this class: the null `var-usage/*/aliased` definitions, the 351 `var-def/import-vars` rows, and references on originals that miss facade callers (`with-native-query-testing-context`: 2 of 170).

---

## Design

### What a facade form says

| form | re-exports |
|---|---|
| `(import-vars [ns a b] ns2/c)` | `a`, `b` from `ns`, `c` from `ns2`, under the same names |
| `(import-fn ns/x)` / `(import-macro ns/x)` / `(import-def ns/x)` | `x` |
| `(import-fn ns/x y)` (and the other two) | `x` under the new name `y` |

`ns` is resolved through the facade file's aliases. The head resolves by fqn (`head_fqn_candidates`): `potemkin/import-*`, `potemkin.namespaces/import-*`, and any macro `:lint-as`'d to one of them (`metabase.util.namespaces/import-fns` → `potemkin/import-vars`), which `settings::merge` routes to a third table, `ExtractConfig::lint_as_imports`.

### Extraction

- `NsMeta` gains `reexports: HashMap<String, String>`, facade name → original fqn (`#[serde(default)]`, so old caches deserialize; `CACHE_FORMAT_VERSION` still bumps, since library facades must be re-read).
- The occurrence walker records each original token (`ns/x`, or `x` inside `[ns x …]`) as an occurrence of the original fqn. References on the original then list the import entry, and a rename of the original rewrites it.
- No `Symbol` is created for the facade name: the original stays the one definition, so the "project beats library" and dialect-slot rules are untouched.

### Resolution (index level, at query time)

The walker of a *caller* cannot see the facade's `NsMeta`, so `mu/defn` is recorded as `metabase.util.malli/defn`, as today. One index helper, `Index::reexport_target(fqn) -> Option<String>`, follows `ns_meta(ns).reexports[name]` transitively (cycle guard, depth cap), and the handlers ask it where they already fall back:

- definition, hover, signature help, ClojureDocs: `resolve_symbol` tries the fqn, then its re-export target.
- references and rename (`occurrences_for`, `matching_occurrences`): an fqn's sites are its own occurrences plus those of every facade fqn re-exporting it (a reverse map, `Index::reexported_as`, rebuilt from `NsMeta` on insert/remove like `keyword_counts`). Rename keeps the `references::spells` rule, so a renamed export (`(import-fn table/database table->database)`) is listed by references and left alone by rename, like a `:refer … :rename` call.
- completion: `mu/` offers the facade's re-exported names (from `reexports`), resolved through the target for docs.
- `rename_target` on a facade spelling (`mu/defn`) renames the original.

### Not in scope

Running potemkin's semantics beyond naming (`import-vars` of a whole namespace does not exist in potemkin). Hook-defined vars (`defsetting`, `defenterprise`) stay the separate kondo-enrichment backlog item.

## Tasks

- [ ] `settings`: `lint_as_imports` table; tests.
- [ ] Extractor: recognize the head, fill `NsMeta.reexports`, record original tokens as occurrences; extractor tests for all four shapes, aliases, `:lint-as`, renamed exports. Bump `CACHE_FORMAT_VERSION`.
- [ ] Index: `reexport_target`, `reexported_as` maintained on insert/remove, library files included; index tests incl. a chain and a cycle.
- [ ] Handlers: definition/hover/signature help, references/rename/highlight, completion, `rename_target`; e2e test on a fixture facade (project) and on a library facade.
- [ ] Docs: FEATURES.md, AGENTS.md invariant, ROADMAP (remove from *Not planned*, archive note).
- [ ] Gates: `bb check`, `bb e2e`, `bb e2e-pulse`, `bb e2e-calva`, `bb compare clj-kondo` and `metabase` (`var-def/import-vars`, `defprotocol+` and the `*/aliased` null rows should move), `bb bench` and `bb soak` (index change).
