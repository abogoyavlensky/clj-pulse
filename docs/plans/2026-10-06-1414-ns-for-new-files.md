# Namespace Form for New Files Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the editor creates an empty `.clj`/`.cljs`/`.cljc`/`.lg` file under a project source root (VS Code Explorer → New File…), the server inserts `(ns <name>)` derived from the file's path, through the standard `workspace/didCreateFiles` → `workspace/applyEdit` exchange (ROADMAP Milestone 4).

**Tech Stack:** Rust, tower-lsp 0.20 (lsp-types 0.94.1). Tests: unit tests in the new module, e2e in `tests/test_e2e.rs` on `tests/fixtures/simple_project`, `bb e2e-pulse` (client-visible), `bb e2e-nvim` (new capability).

---

## Design

### Today

VS Code has no file templates. Its file-creation event (`workspace.onDidCreateFiles`) fires for files created through the Explorer or a `WorkspaceEdit`, not for files written to disk by a terminal or git. `vscode-languageclient` forwards that event as the `workspace/didCreateFiles` notification, but only when the server advertises `workspace.fileOperations.didCreate` with matching filters. clj-pulse advertises no file operations and never sends `workspace/applyEdit`, so a new file stays empty. clojure-lsp has this feature (`:auto-add-ns-to-new-files?`), and Calva users expect it.

The Clojure Pulse extension needs **no code**: `vscode-languageclient` 9 sends the notification as soon as the capability is advertised, and it applies `workspace/applyEdit` itself.

### Behavior

1. **Capability.** `ServerCapabilities.workspace.file_operations.did_create` lists four filters, one per extension: `scheme: "file"`, glob `**/*.clj`, `**/*.cljs`, `**/*.cljc`, `**/*.lg`, `matches: File`. Four plain globs instead of `**/*.{clj,…}` keep the filters independent of brace support in each client's glob engine.
2. **`did_create_files`.** For each created file URI:
   - Convert it to a path (`uri.to_file_path()`). Skip it unless `config::is_clojure_source(&path)` holds. The filters already exclude `.edn` and `.bb`; this check also rejects `project.clj`.
   - **Only empty files.** If the document is open in `DocumentStore`, read its text from `snapshot`. Otherwise read the file from disk. Skip the file unless the text is exactly empty (`""`). This leaves Explorer copy/paste and duplication alone, since those also fire the event but carry content. A read error (file already gone) also means skip.
   - **Resolve the namespace.** Find the owning project with `owning_project` (`src/server.rs`), call `config::source_paths(&project.dir)`, and pass the result to the pure function `ns_for_path`. If any step returns nothing, skip the file: no project, or the file sits outside every source root. A wrong `ns` is worse than none.
   - **Edit.** Send `client.apply_edit(WorkspaceEdit { changes: {uri: [TextEdit { range: 0:0–0:0, new_text: "(ns <name>)\n" }]} })`. Log the result at `info` (`inserted ns <name> into new file <path>`). Log a failure or a `applied: false` response at `debug`. Never surface either to the user.
3. **Same `ns` everywhere.** Source files and test files get the same bare form: no `:require`, no `clojure.test` template, no docstring. `test/foo/bar_test.clj` → `(ns foo.bar-test)`.
4. **No setting.** The feature is on for everyone. An empty Clojure file without an `ns` form is never what the user wants, and the files it reacts to come only from user actions in the editor. An opt-out can follow if someone asks (YAGNI).

### `ns_for_path`

A pure function in a new module, `src/handlers/new_file.rs`:

```rust
/// The namespace a file at `path` declares by convention, relative to the
/// longest `roots` entry containing it; `None` outside every root.
pub fn ns_for_path(path: &Path, roots: &[PathBuf]) -> Option<String>
```

- Pick the **longest** root that `path.starts_with`, so a nested root such as `src` inside `src/main/clojure` resolves to the deepest match.
- Take the path relative to that root, drop the file extension from the last component, join the components with `.`, and replace `_` with `-` in each segment.
- Return `None` if the relative path is empty, any component is not valid UTF-8, or any segment (after the `_`→`-` swap) cannot appear in a namespace symbol. A segment is rejected when it is empty, starts with a digit, or contains whitespace, `.`, or one of `()[]{}"',;@^`` ` ``~\#`. `my file.clj` and `foo.bar.clj` get nothing, which beats writing an `ns` the reader rejects or one that mismatches the path.
- `src/core.clj` → `core`: a single-segment namespace is legal, so return it.

Examples: `src/foo/bar_baz.clj` → `foo.bar-baz`; `test/foo/bar_test.cljc` → `foo.bar-test`; `src/app/main.lg` → `app.main`; `resources/x.clj` with roots `[src, test]` → `None`.

### Source roots

`config::source_paths` is the source of truth that indexing already uses. It covers deps.edn top-level `:paths` plus every alias's `:extra-paths` (so `dev/` from a `:dev` alias counts, and `dev/user.clj` → `(ns user)`), Leiningen `:source-paths`/`:test-paths` across profiles, and lgx `lgx.edn` paths, and it always adds `src` and `test`. An alias's own `:paths` is ignored there, and therefore here too.

### Ordering and concurrency

VS Code creates the file on disk, fires the event, and opens the editor. `didOpen` and `didCreateFiles` can arrive in either order. Either way the file is empty, so the empty check passes. `workspace/applyEdit` works whether the document is already open in an editor or not, because VS Code loads it into a model. The edit leaves the buffer dirty, as clojure-lsp does. The handler awaits `apply_edit` inline, the same way `initialized` awaits `register_capability`. The lint pass that `didOpen` triggers then re-runs on the `didChange` the edit produces.

### Testing

- Unit tests for `ns_for_path` in `src/handlers/new_file.rs` (`#[cfg(test)]`, the pattern the other handler modules use).
- e2e (`tests/test_e2e.rs`): the advertised capability; an empty file under `src/` produces a `workspace/applyEdit` request with the expected edit; a non-empty file and a file outside every root produce none. The harness stashes server→client requests and answers them with `null` (`LspClient::stash`), so `wait_for_notification_where("workspace/applyEdit", …)` sees the request.
- `bb e2e-pulse`: a real VS Code `WorkspaceEdit.createFile` makes the file's text become `(ns …)\n`. That event comes from the same `onDidCreateFiles` path as the Explorer.
- `bb e2e-nvim`: the new capability must not break the Neovim client. Neovim does not send `didCreateFiles`, which is accepted.

### Out of scope

- A `didOpen` fallback for clients that never send `didCreateFiles` (Neovim).
- Test-file templates, requires, and docstrings.
- Moving or renaming files (`workspace/willRenameFiles` is its own Milestone 4 item).
- The Clojure Pulse extension's docs. Its `docs/features.md` gains a line when the extension bumps its bundled server to the release that ships this.

## File Structure

- Create `src/handlers/new_file.rs`: `ns_for_path`, plus `ns_insert_edit(uri, ns) -> WorkspaceEdit` so the edit's shape lives in one place and the unit tests cover it. Unit tests go in the same file.
- Modify `src/handlers/mod.rs`: add `pub mod new_file;`.
- Modify `src/server.rs`: add the `workspace.file_operations` capability in `initialize`, and add `async fn did_create_files` to `impl LanguageServer for Backend`.
- Modify `tests/test_e2e.rs`: e2e tests.
- Modify `scripts/pulse-e2e/tests.js`: one VS Code check.
- Modify `README.md`, `docs/FEATURES.md`, `AGENTS.md`, `docs/ROADMAP.md`: docs (ROADMAP's "Docs accuracy" rule: these agree with `ServerCapabilities`).

### Task 1: Roadmap item

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Add the item.** Under "Milestone 4 — small power features", after the `workspace/willRenameFiles` line, add:
  `- [x] **ns form for new files.** \`workspace/didCreateFiles\`: an empty Clojure file created under a source root gets \`(ns …)\` from its path.` followed by `  Plan: [2026-10-06-1414-ns-for-new-files.md](plans/2026-10-06-1414-ns-for-new-files.md) — in progress`.
- [x] **Step 2: Commit** together with this plan file: `git add docs/ROADMAP.md docs/plans/2026-10-06-1414-ns-for-new-files.md && git commit -m "plan: ns form for new files"`.

### Task 2: `ns_for_path` and the edit

**Files:**
- Create: `src/handlers/new_file.rs`
- Modify: `src/handlers/mod.rs`

- [x] **Step 1: Write the failing unit tests** in `src/handlers/new_file.rs` (`#[cfg(test)] mod tests`), with the module declared in `handlers/mod.rs` and `ns_for_path` stubbed to `None`. Use absolute `PathBuf`s built from a fixed base such as `/p` (`Path::new("/p").join("src")`), so no files are needed. Cases:
  - `src/foo/bar_baz.clj` → `Some("foo.bar-baz")`
  - `test/foo/bar_test.cljc` → `Some("foo.bar-test")`
  - `src/app/main.lg` → `Some("app.main")`
  - `src/core.cljs` → `Some("core")`
  - longest root wins: roots `[/p/src, /p/src/main/clojure]`, path `/p/src/main/clojure/a/b.clj` → `Some("a.b")`
  - outside every root: `/p/resources/x.clj` → `None`
  - a path equal to a root → `None`
  - unusable names → `None`: `src/my file.clj`, `src/foo.bar.clj`, `src/1st/x.clj`
  - `ns_insert_edit` returns one `TextEdit` at range 0:0–0:0 with `new_text` `"(ns foo.bar)\n"` under the given URI.
- [x] **Step 2: Run them and watch them fail.** Run: `cargo test --lib new_file`. Expected: the `ns_for_path` cases FAIL.
- [x] **Step 3: Implement** `ns_for_path` (longest prefix root, `strip_prefix`, file stem for the last component, `.`-join, `_`→`-`) and `ns_insert_edit`, with doc comments in the style of the surrounding modules.
- [x] **Step 4: Run them and watch them pass.** Run: `cargo test --lib new_file`. Expected: PASS.
- [x] **Step 5: Commit.** `git commit -m "new files: derive the ns from a path and its source roots"`

### Task 3: Capability and `did_create_files`

**Files:**
- Modify: `src/server.rs`
- Test: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing e2e tests** in `tests/test_e2e.rs`, following the existing tests (`setup_project()`, `LspClient::start`, `initialize`, `wait_for_log("Indexed")`):
  - `new_file_capability_advertised`: the `initialize` result has `capabilities.workspace.fileOperations.didCreate.filters` with globs for all four extensions.
  - `new_empty_file_gets_ns`: create an empty `src/fresh/new_thing.clj` in the temp project, then `notify("workspace/didCreateFiles", {"files": [{"uri": <file uri>}]})`. `wait_for_notification_where("workspace/applyEdit", …)` returns params whose `edit.changes[<uri>][0]` is `newText: "(ns fresh.new-thing)\n"` at 0:0–0:0.
  - `new_file_with_content_or_outside_roots_untouched`: create an empty `src/fresh/typed.clj` on disk and `did_open_uri` it with the text `(def x 1)`, so the open buffer is non-empty while the disk copy is empty. Then send one notification listing, in this order, a non-empty `src/fresh/copied.clj`, an empty `resources/stray.clj`, `src/fresh/typed.clj`, and an empty `src/fresh/marker.clj`. The handler walks `files` in order within one call, so the marker fences the first two without a sleep and without assuming tower-lsp handles separate notifications in order. The first `workspace/applyEdit` that arrives must name `marker.clj`, and no stashed `applyEdit` may name the other three. `typed.clj` proves the open buffer outranks the disk copy. The handler must therefore process files sequentially (Task 3, Step 4).
- [x] **Step 2: Run them and watch them fail.** Run: `cargo test --test test_e2e new_file`. Expected: FAIL (no capability; `applyEdit` times out).
- [x] **Step 3: Advertise the capability.** In `initialize`'s `ServerCapabilities`, set `workspace: Some(WorkspaceServerCapabilities { workspace_folders: None, file_operations: Some(WorkspaceFileOperationsServerCapabilities { did_create: Some(FileOperationRegistrationOptions { filters }), ..Default::default() }) })`. Build `filters` from the four extensions (`scheme: Some("file")`, `FileOperationPattern { glob: "**/*.<ext>", matches: Some(FileOperationPatternKind::File), options: None }`). Comment why the filters are plain globs rather than a brace glob.
- [x] **Step 4: Implement `did_create_files`** in `impl LanguageServer for Backend` as the Design describes, walking `params.files` sequentially (one `apply_edit` per qualifying file, awaited before the next file): path conversion, `is_clojure_source`, the empty check (the `DocumentStore` snapshot first, then disk), `owning_project` on a clone of the project list taken under the lock (never hold the `std::sync::Mutex` across an `.await`), `config::source_paths`, `ns_for_path`, then `self.client.apply_edit(ns_insert_edit(..)).await`, with logging. Keep the per-file logic in a small `Backend` helper if the handler grows past a screen.
- [x] **Step 5: Run them and watch them pass.** Run: `cargo test --test test_e2e new_file`. Expected: PASS.
- [x] **Step 6: Commit.** `git commit -m "new files: insert the ns form on workspace/didCreateFiles"`

> Deviation: the per-file decision lives in a free fn `ns_for_new_file(documents, project_list, uri)` beside `owning_project` rather than a `Backend` method — it needs no `self` beyond those two, and the project list is cloned once per notification. The empty-file e2e test is named `test_e2e_new_file_empty_gets_ns` so `cargo test new_file` selects all three.

> Deviation (codex review, two rounds): startup stores the project list in a spawned task, so a `didCreateFiles` arriving first saw an empty list and was dropped. A first fix (the workspace root standing in as the only project) could insert a wrong ns in a monorepo, so it was replaced: `Backend.projects_ready` (a `watch` flag set right after startup stores the detected list) is awaited by `did_create_files`, bounded by `PROJECTS_READY_TIMEOUT` (5 s) and skipped when there is no workspace root. It waits for detection only, never `ConfigApplyLock`, which startup holds through stage 3. `test_e2e_new_file_right_after_initialize_gets_ns` fails without the wait.

### Task 4: Clojure Pulse e2e check

**Files:**
- Modify: `scripts/pulse-e2e/tests.js`

- [ ] **Step 1: Add check 11** before the final `failed` tally. Build a `vscode.WorkspaceEdit` that calls `createFile` for `src/fresh/new_thing.clj` under the fixture workspace (`ignoreIfExists: true`), apply it with `vscode.workspace.applyEdit`, then `poll(30000, …)` `vscode.workspace.openTextDocument(uri)` until `getText()` starts with `(ns fresh.new-thing)`. Then `check(...)` with the text as detail. Read `runner.js` first. If the run works on the committed fixture instead of a copy, delete the file afterwards (`vscode.workspace.fs.delete`) so the fixture stays clean. Extend the header comment's list of what the suite covers.
- [ ] **Step 2: Run the gate.** Run: `cargo build && bb e2e-pulse`. Expected: every check passes, including the new one.
- [ ] **Step 3: Commit.** `git commit -m "e2e-pulse: new file gets its ns form"`

### Task 5: Verification gates

- [ ] **Step 1:** Run `bb check`. Expected: fmt, clippy `-D warnings`, and all tests green.
- [ ] **Step 2:** Run `bb e2e`. Expected: PASS.
- [ ] **Step 3:** Run `bb e2e-nvim`. Expected: PASS. The capability is new, and Neovim must still initialize cleanly.

### Task 6: Docs and close-out

**Files:**
- Modify: `docs/FEATURES.md`, `README.md`, `AGENTS.md`, `docs/ROADMAP.md`, this plan

- [ ] **Step 1: FEATURES.md.** Add a bullet after "Code actions": `- **ns form for new files** - an empty .clj/.cljs/.cljc/.lg file created in the editor (Explorer → New File) under a source root gets \`(ns …)\` from its path, e.g. \`src/foo/bar_baz.clj\` → \`(ns foo.bar-baz)\`. Needs a client that sends \`workspace/didCreateFiles\` (VS Code does; Neovim does not).`
- [ ] **Step 2: README.md.** Check whether any feature list or status line enumerates capabilities. If one does, add the feature there. If none does, leave README unchanged and record that in the commit message.
- [ ] **Step 3: AGENTS.md.** Add one invariant bullet: `did_create_files` acts only on files that are empty (the open buffer, else disk) and inside a `config::source_paths` root of the owning project, and it inserts a bare `(ns …)` via `workspace/applyEdit`. A file outside every root is left alone.
- [ ] **Step 4: ROADMAP.md.** Tick the item and set it to `— done`. In "Where we stand", add one sentence on the feature to the shipped paragraph.
- [ ] **Step 5: This plan.** Add `**Status: complete** (<date>, branch <name>).` under the title.
- [ ] **Step 6: Commit.** `git commit -m "docs: ns form for new files"`
