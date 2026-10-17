# penpot as a Correctness Corpus, Keyword `:keys` Entries and `:` in Names Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `bb compare penpot` and `bb soak penpot` run on a pinned penpot checkout, and the two server bugs its first compare run found are fixed: a keyword entry of a key directive (`{:keys [::db]}`) binds its name and is a site of the key it reads, and a var whose name holds a `:` (`sql:get-libraries`) can be renamed.

**Why penpot:** the third corpus, chosen on 2026-10-16 over datahike. It is the first multi-project workspace the gates cover (a root `deps.edn` gathering `backend`, `common`, `frontend` and `exporter` through `:local/root`), the first with real Integrant code, `.cljc` shared by a Clojure backend and a ClojureScript frontend, and names defined by kondo hooks (`mf/defc`). It is a correctness corpus only: `bb bench` stays on metabase and clj-kondo.

---

## Design

### The corpus

`bb.edn` pins `penpot` at `main` `da1791b` with `:projects ["backend" "common" "frontend" "exporter"]`; `bench-order` does not list it. `bb compare` and `bb soak` pass the list as `CLJ_PULSE_CORPUS_PROJECTS`. `sites::corpus_bases` reads it (the root alone when unset), and every "the corpus's own source" rule goes through it: the compare oracle lints the union of `config::source_paths` of each base, `is_source_ish` and `namespace_paths` accept `src`/`test` directly under any base, and the soak churns each base's `src` and creates its files under the first base's `src`. With the variable unset nothing changes, so the metabase and clj-kondo tables stay comparable.

### `{:keys [::db]}`

`clojure.core/destructure` binds `(-> entry name symbol)` for a keyword entry and reads `(keyword (or directive-ns (namespace entry)) (name entry))`. Both walkers' binding collectors skipped `kwd_lit` entries, so the local was unbound (definition null, references on a usage answered nothing) and the key had no site there (references on `::mtx/metrics` missed 10 of 53). Now:

- `keyword_entry_binding` binds the name part, in `collect_binding_names` and `collect_binding_targets` (entries before `&` only).
- `record_destructuring_keys` records the keyword as the key it reads.
- `find_binding_sym` accepts the keyword, so `is_destructured_key` holds and a local rename refuses.
- `keyword_target` refuses a keyword rename when a site is such an entry (`extractor::is_keyword_key_entry`), since rewriting it renames the local.

### `:` inside a symbol name

`is_valid_symbol_name` rejected any `:`, so `rename` refused every new name for `sql:get-libraries`. The reader accepts an inner `:`, never a leading or trailing one, never `::`.

## Tasks

- [x] Pin penpot; `corpus_bases` in `tests/common/sites.rs`; compare and soak read it; `bb.edn` wiring.
- [x] First `bb compare penpot`; triage.
- [x] Keyword `:keys` entries: walkers, occurrences, both rename refusals; extractor and e2e tests; `CACHE_FORMAT_VERSION` 25.
- [x] Inner `:` in rename names; unit test.
- [x] `bb check`, `bb e2e`, `bb compare clj-kondo` (no regression), `bb compare penpot` and `metabase` (record), `bb soak penpot`.
- [x] MEMORY.md tables, CLAUDE.md, FEATURES.md, ROADMAP.
