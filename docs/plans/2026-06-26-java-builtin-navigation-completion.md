# Built-in Java Navigation & Completion Implementation Plan

> **Status: 📋 Planned (2026-06-26).** Two phases, pure-Rust static analysis.
> Verification gate: `bb check` + `bb e2e`.

> **For agentic workers:** Use executing-plans to implement this plan
> task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Navigate to and autocomplete **built-in (JDK) Java** classes, static
members, and constructors from Clojure interop — go-to-definition, Javadoc hover,
completion, and signature help — by reading the JDK's local `src.zip` and parsing
`.java` with tree-sitter-java. No JVM, no clj-kondo, no external tools, no network.

**Tech Stack:** Rust; existing `zip` crate (read `src.zip`); **new dependency
`tree-sitter-java = "0.23"`** (pure-Rust grammar, same `tree-sitter-language` ABI
shim as the existing `tree-sitter-clojure`); the existing `jar:` URI content
provider and the stdio e2e harness.

---

## Scope

**In scope (built-in / JDK Java only):**
- Go-to-definition on a **class** (`String`, imported `Instant`, FQN
  `java.util.Date`), a **static member** (`Math/sqrt`, `Instant/now`,
  `String/CASE_INSENSITIVE_ORDER`), a **constructor** (`(StringBuilder.)`), and a
  **class in a `^type-hint`**.
- **Hover** with the signature + Javadoc for those.
- **Completion** of class names (in `:import` / type-hint / `Class.` / `Class/`
  positions) and **static members** after `Class/`.
- **Signature help** for `(Class/staticMethod …)` and `(Class. …)`.

**Explicitly out of scope (deferred to later phases / plans):**
- **Instance methods** `(.method obj)` — both navigation and completion. They
  require the receiver's type (local type inference), which is a separate effort.
  No local-binding/type-hint receiver tracking in this plan.
- **Library Java** (classes from classpath jars, `clojure.lang.*`) — needs a
  `.class` bytecode reader (`cafebabe`); a later phase.
- **Decompilation** of any kind.
- **Downloading** `src.zip` or sources jars — we read only the locally installed
  JDK source; absent ⇒ feature silently off.
- **Cross-class inheritance walking** — unnecessary here, since static members,
  constructors, and class refs need only a class's own declarations.

## Design

### Architecture & data flow

One new module, `src/index/jdk.rs`, owns a `JdkIndex` stored on the shared
`Index` behind a `std::sync::OnceLock` (set once when background discovery
finishes; `None`/unset ⇒ no JDK source found ⇒ feature absent). All other changes
are edits to existing files.

```
startup → spawn background task (alongside library indexing):
            discover src.zip → build class_entries (eager, entry-names only)
            → Index.jdk.set(JdkIndex{ src_zip, class_entries, parsed })
            → log "JDK source indexed: N classes"

editor request (definition / hover / completion / signature)
  → existing Clojure resolution runs first
  → if it yields nothing, resolve_java_at() classifies the interop form
       (Class | Class/member | (Class.) | ^Class)
  → JdkIndex.class(fqn): return cached JavaClassInfo, else lazily read the one
       .java from src.zip and parse it with tree-sitter-java, then cache
  → handler builds the response:
       definition → jar:…/src.zip!/<entry> location at the decl name_range
       hover      → signature + Javadoc
       completion → class names (prefix over class_entries) or static members
       signature  → parameter hints per overload
```

`JdkIndex` fields: `src_zip: PathBuf`, `class_entries: HashMap<String /*fqn*/,
String /*zip entry*/>` (built eagerly from entry names, immutable after),
`parsed: DashMap<String /*fqn*/, Arc<JavaClassInfo>>` (lazy). Java data **never**
enters the Clojure `symbols` map or the jar cache.

### JDK source discovery (`src/index/jdk.rs`)

Locate a `src.zip`, **no process spawn**, first hit wins:
1. `CLJ_PULSE_JDK_SRC` env var pointing directly at a `src.zip` (override; also
   how the hermetic e2e injects a fixture).
2. `$JAVA_HOME/lib/src.zip`.
3. `java` resolved on `PATH` → real JDK home → `lib/src.zip`.

Whichever JDK `JAVA_HOME` points at is the one used (matches the project's
configured JDK). None found ⇒ leave `Index.jdk` unset, log once at debug, Clojure
behavior unchanged. **Known limitation (documented):** under shim-based tool
managers (mise/asdf) `java` on `PATH` resolves to the shim, not the JDK, so
`JAVA_HOME` (or the override) is the reliable signal there.

### Class → entry map (eager, cheap)

Enumerate the `src.zip` central-directory entry **names** only (no decompression;
milliseconds for ~8k entries). JDK 9+ entries are
`<module>/<pkg>/<Class>.java` (e.g. `java.base/java/lang/String.java`): strip the
leading module segment, convert the package path to dots →
`java.lang.String → java.base/java/lang/String.java`. The pre-9 layout (no module
prefix) falls out of the same logic. This map also **is** the class-name set that
completion draws from. Nested classes (`Map.Entry`) map to the outer file
(`…/Map.java`); best-effort, navigation lands on the file.

### `:import` parsing → `NsMeta.imports` (`src/index/extractor.rs`)

Add `imports: HashMap<String /*simple*/, String /*fqn*/>` to `NsMeta`. In
`extract_ns` (today scans `:require` only, ~line 341) add an `:import` branch with
a `process_import_spec` mirroring `process_require_spec`, handling all three
forms:
- `(:import [java.util Date List])` — package vector
- `(:import (java.util Date List))` — package list
- `(:import java.util.Date)` — fully-qualified symbol

`java.lang.*` needs no import: a bare simple name with no import-map hit is probed
against `class_entries` as `java.lang.<Simple>`. Adding a field to `NsMeta` changes
its serialized layout ⇒ **bump `CACHE_FORMAT_VERSION` 9 → 10**
(`src/index/jar_cache.rs:17`).

### `JavaClassInfo` + lazy parse (tree-sitter-java)

On first access to a class FQN, open `src.zip`, read its one `.java`, parse with
tree-sitter-java into:

```
JavaClassInfo {
  fqn, entry, decl_name_range,
  extends: Option<String>, implements: Vec<String>,   // for hover display only
  methods: Vec<JavaMember>,   // {name, params: Vec<String>, return_type, static, name_range, javadoc}
  fields:  Vec<JavaMember>,
  ctors:   Vec<JavaCtor>,     // {params, name_range, javadoc}
}
```

Cache `Arc<JavaClassInfo>` in `parsed`. Javadoc is the `/** … */` block
immediately preceding a declaration. **No cross-file inheritance walking** —
`extends`/`implements` are captured only to render on hover.

### Resolution + precedence (`src/handlers/`)

A new `resolve_java_at(documents, index, uri, position) -> Option<JavaTarget>`
where `JavaTarget = { class_fqn, member: Option<String>, kind: Class|StaticMember|Ctor }`.
It classifies the symbol under the cursor:
- **`Class/staticMember`** — the grammar already splits to `ns=Class`,
  `name=member`; resolve `Class`, member = the name half.
- **bare `Class` / FQN `pkg.Class`** — resolve via the file's import map, then
  `java.lang.<Simple>`, then a dotted FQN present in `class_entries`.
- **`(Class. …)`** — head `sym_lit` ending in `.`; strip the dot, resolve the
  class, `kind = Ctor`.
- **`^Class` hint** — class ref.

**Precedence (non-regression):** each handler runs its **existing Clojure
resolution first**; `resolve_java_at` is consulted **only when Clojure resolution
returns nothing**. `str/join` resolves Clojure-side and never reaches the Java
path; `Math/sqrt` finds no Clojure alias/ns/var and falls through to Java. The
import map is used *inside* `resolve_java_at` to expand simple names — it is not a
competing precedence signal. This keeps every existing test green.

`definition` builds the location from the resolved `JavaClassInfo`: virtual path
`<src_zip>!/<entry>`, range = the member's `name_range` (or `decl_name_range` for a
class/ctor), then `uri::from_index_path` emits `jar:file://…/src.zip!/…java`.

### URI plumbing (`src/uri.rs`)

`split_jar_virtual_path` hard-codes the `.jar!/` boundary (line 54:
`path.find(".jar!/")`). Generalize it to also recognize `.zip!/` (match the first
`!/` that follows a `.jar` or `.zip` archive segment). Scheme stays `jar`
(`jar:file://…/src.zip!/…`), so `server.rs` scheme registration is unchanged, and
`jar_content::extract_content` already reads any zip — it serves the `.java` text
to the editor unchanged. `parse_jar_uri` already splits on `!/` (not `.jar!/`), so
inbound `src.zip` URIs already parse.

### Handler features (Phase B)

- **Completion** (`src/handlers/completion.rs`): in `:import` / type-hint /
  `Class.` / `Class/` positions, complete **class names** by prefix over
  `class_entries` (simple name and, in `:import`, package-qualified). After
  `Class/`, complete the resolved class's **static** methods/fields. No instance
  completion.
- **Signature help** (`src/handlers/signature.rs`): `(Class/staticMethod ▏…)` and
  `(Class. ▏…)` → one `SignatureInformation` per overload, params from
  `JavaClassInfo`.

### Error handling & degradation

No flag/config — auto-detect (clj-pulse's zero-config ethos). No `src.zip` ⇒
feature absent. Parse failure ⇒ log warning, skip (the extractor's existing "never
crash" discipline). Class/member not found ⇒ no navigation (same as any unindexed
symbol). Java resolution never alters Clojure resolution and never panics.

### Testing strategy

**Hermetic fixture**, mirroring the `LGX_HOME` pattern: commit fixture `.java`
files as text under `tests/fixtures/jdk_src/` and build a temporary `src.zip` from
them at test time with the `zip` crate's `ZipWriter`; point discovery at it via
`CLJ_PULSE_JDK_SRC` (e2e uses the existing `start_with_env`). The fixture exercises
both resolution paths and module-prefix stripping:
- `java.base/demo/lib/Greeter.java` — package `demo.lib`, a `static String
  greet(String name)` with Javadoc, a `Greeter(int seed)` constructor, a `static
  final int VERSION` field. Reached via `(:import [demo.lib Greeter])`.
- `java.base/java/lang/Sample.java` — package `java.lang`, a `static Sample
  of(long n)`; reached **without** import (auto-`java.lang`).

No e2e depends on the CI box's JDK. An optional `#[ignore]` test may run against
the real Temurin `src.zip` (in the spirit of `bb e2e-real`).

## File Structure

**Create:**
- `src/index/jdk.rs` — `JdkIndex` (discovery, `class_entries`, lazy `parsed`),
  `JavaClassInfo`/`JavaMember`/`JavaCtor`, tree-sitter-java parsing, the
  `class(fqn)` / `class_names_with_prefix(prefix)` API. Inline `#[cfg(test)]`
  unit tests (discovery, module-prefix stripping, member extraction).
- `tests/fixtures/jdk_src/java.base/demo/lib/Greeter.java` — fixture source.
- `tests/fixtures/jdk_src/java.base/java/lang/Sample.java` — fixture source.
- `tests/test_java.rs` — Index-level integration tests (resolution precedence,
  hover content, completion lists, signature) using a temp `src.zip` built from
  the fixtures.

**Modify:**
- `Cargo.toml` — add `tree-sitter-java = "0.23"`.
- `src/index/mod.rs` — `NsMeta.imports` field; `Index.jdk: OnceLock<JdkIndex>` +
  a `set_jdk`/getter; register `jdk` module.
- `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` 9 → 10 (+ comment: line 11
  layout note).
- `src/index/extractor.rs` — `:import` parsing in `extract_ns` +
  `process_import_spec`.
- `src/uri.rs` — generalize `split_jar_virtual_path` to `.jar!/` **or** `.zip!/`;
  add a unit test.
- `src/server.rs` — spawn JDK discovery as a startup background task; log
  `"JDK source indexed: N classes"`.
- `src/handlers/mod.rs` (or `references.rs`) — `resolve_java_at` + `JavaTarget`.
- `src/handlers/definition.rs` — Java fallback → src.zip location.
- `src/handlers/hover.rs` — Java hover (signature + Javadoc).
- `src/handlers/completion.rs` — class-name + static-member completion.
- `src/handlers/signature.rs` — Java signature help.
- `tests/test_extractor.rs` — `:import` parsing unit tests.
- `tests/test_e2e.rs` — real-process nav/hover/completion/signature, fixture
  `src.zip` via `start_with_env`.
- `docs/ROADMAP.md` — mark built-in Java nav/completion progress (line 99 item).

---

## Tasks — Phase A: navigation + Javadoc hover

### Task A1: Add `tree-sitter-java` + ABI smoke test

**Files:** Modify `Cargo.toml`; create a throwaway test in `src/index/jdk.rs`.

- [ ] **Step 1:** `~/.cargo/bin/cargo add tree-sitter-java@0.23` (writes
  `Cargo.toml`). Create `src/index/jdk.rs` with an empty module and register it in
  `src/index/mod.rs` (`pub mod jdk;`).
- [ ] **Step 2 (failing test):** In `jdk.rs` `#[cfg(test)]`, write `parses_java`:
  build a `tree_sitter::Parser`, `set_language(&tree_sitter_java::LANGUAGE.into())`,
  parse `"class A { static int f(int x){return x;} }"`, assert the root node has no
  error and contains a `method_declaration`.
- [ ] **Step 3:** Run `~/.cargo/bin/cargo test --lib jdk::tests::parses_java`.
  Expected: PASS (this confirms the residual ABI/0.25-compat risk at runtime).
- [ ] **Step 4:** `bb check` → PASS (fmt/clippy clean with the new dep).
- [ ] **Step 5:** `git commit -m "Add tree-sitter-java dependency"`

### Task A2: `:import` parsing → `NsMeta.imports` (+ cache bump)

**Files:** Modify `src/index/mod.rs`, `src/index/extractor.rs`,
`src/index/jar_cache.rs`; Test `tests/test_extractor.rs`.

- [ ] **Step 1 (failing test):** In `test_extractor.rs`, `imports_all_forms`:
  extract a ns with `(:import [java.util Date List] (java.time Instant)
  java.io.File)` and assert `ns_meta.imports` maps `Date→java.util.Date`,
  `List→java.util.List`, `Instant→java.time.Instant`, `File→java.io.File`.
- [ ] **Step 2:** Run `cargo test --test test_extractor imports_all_forms`.
  Expected: FAIL (no `imports` field).
- [ ] **Step 3:** Add `imports: HashMap<String,String>` to `NsMeta` (init empty in
  all constructors). Add the `:import` branch to `extract_ns` + `process_import_spec`
  handling the three forms. Bump `CACHE_FORMAT_VERSION` to 10 with a comment
  ("10 = NsMeta.imports").
- [ ] **Step 4:** Run `cargo test --test test_extractor imports_all_forms` → PASS;
  `cargo test --lib` → PASS (jar_cache version test still valid).
- [ ] **Step 5:** `git commit -m "Parse :import into NsMeta.imports; bump jar cache to v10"`

### Task A3: `JdkIndex` — discovery + class→entry map

**Files:** Modify `src/index/jdk.rs`, `src/index/mod.rs`.

- [ ] **Step 1 (failing test):** In `jdk.rs` tests, `class_map_strips_module`:
  write a helper `make_src_zip(&[(&str,&str)]) -> tempfile::NamedTempFile` (uses
  `zip::ZipWriter`), build one with entry
  `java.base/java/lang/String.java` → `"class String {}"`, call
  `JdkIndex::discover_from(path)`, assert `class_entries["java.lang.String"]`
  equals the full entry, and that `class_names_with_prefix("Strin")` contains
  `String`.
- [ ] **Step 2:** Run `cargo test --lib jdk::tests::class_map_strips_module`.
  Expected: FAIL.
- [ ] **Step 3:** Implement `JdkIndex { src_zip, class_entries, parsed }`,
  `discover()` (env override → `$JAVA_HOME/lib/src.zip` → `PATH`), a
  `discover_from(path)` test seam that builds `class_entries` by enumerating entry
  names and stripping the leading module segment, and `class_names_with_prefix`.
  Add `Index.jdk: OnceLock<JdkIndex>` + setter/getter (no parse yet).
- [ ] **Step 4:** Run `cargo test --lib jdk::` → PASS.
- [ ] **Step 5:** `git commit -m "JdkIndex: discover src.zip, build class->entry map"`

### Task A4: `JavaClassInfo` + lazy parse

**Files:** Modify `src/index/jdk.rs`.

- [ ] **Step 1 (failing test):** `parses_members`: `make_src_zip` with a
  `Greeter.java` containing Javadoc + `static String greet(String name)`, a
  `Greeter(int seed)` ctor, a `static final int VERSION` field; call
  `jdk.class("demo.lib.Greeter")`; assert one static method `greet` with one param
  and Javadoc, one ctor with one param, one field `VERSION`.
- [ ] **Step 2:** Run `cargo test --lib jdk::tests::parses_members`. Expected: FAIL.
- [ ] **Step 3:** Implement `JavaClassInfo`/`JavaMember`/`JavaCtor` and
  `JdkIndex::class(fqn)`: return cached, else read the entry from `src.zip`, parse
  with tree-sitter-java, extract decl name range, methods (name/params/return/
  `static`/name_range/preceding-Javadoc), fields, ctors; cache `Arc`. Capture
  `extends`/`implements` strings for display.
- [ ] **Step 4:** Run `cargo test --lib jdk::` → PASS.
- [ ] **Step 5:** `git commit -m "JdkIndex: lazily parse .java into JavaClassInfo"`

### Task A5: Background discovery at startup

**Files:** Modify `src/server.rs`, `src/index/mod.rs`.

- [ ] **Step 1:** In server startup, spawn a background task (alongside library
  indexing) that runs `JdkIndex::discover()` and, on success, `index.jdk.set(...)`
  and logs `"JDK source indexed: {N} classes"`; on no-source, logs once at debug.
- [ ] **Step 2:** `bb check` → PASS. (Covered end-to-end by Task A9's
  `wait_for_log("JDK source indexed")`; no separate unit test.)
- [ ] **Step 3:** `git commit -m "Discover JDK source at startup (background task)"`

### Task A6: URI plumbing for `src.zip`

**Files:** Modify `src/uri.rs`.

- [ ] **Step 1 (failing test):** Add `zip_virtual_path_roundtrips` next to the
  existing uri tests: `from_index_path("/j/lib/src.zip!/java.base/java/lang/String.java")`
  yields `jar:file:///j/lib/src.zip!/java.base/java/lang/String.java`, and
  `to_index_path` of that URI returns the original virtual path.
- [ ] **Step 2:** Run `cargo test --lib uri::tests`. Expected: FAIL (`.zip!/`
  unmatched).
- [ ] **Step 3:** Generalize `split_jar_virtual_path` to find a `.jar!/` **or**
  `.zip!/` boundary; verify `from_index_path`/`to_index_path` use it consistently.
- [ ] **Step 4:** Run `cargo test --lib uri::tests` → PASS.
- [ ] **Step 5:** `git commit -m "uri: treat src.zip virtual paths like jars"`

### Task A7: Resolver + definition (class, static, ctor, FQN, hint)

**Files:** Create/modify `src/handlers/mod.rs` (or `references.rs`); Modify
`src/handlers/definition.rs`; Test `tests/test_java.rs`.

- [ ] **Step 1 (failing tests):** In `test_java.rs`, build an `Index` with a fixture
  `JdkIndex` and a project file `(ns app (:import [demo.lib Greeter]))` using
  `Greeter`, `Greeter/greet`, `(Greeter.)`, `Sample/of` (auto-java.lang). Assert
  `resolve_java_at` returns the right `JavaTarget` for each, that definition
  locations point into `…/src.zip!/…` at the right ranges, and a **non-regression**
  case: `str/join` with `[clojure.string :as str]` resolves Clojure-side (Java path
  not taken).
- [ ] **Step 2:** Run `cargo test --test test_java`. Expected: FAIL.
- [ ] **Step 3:** Implement `resolve_java_at` + `JavaTarget` (classify
  `Class/member`, bare/FQN class, `(Class.)`, `^Class`; expand simple names via the
  import map then `java.lang` then `class_entries`). Wire `definition.rs` to call it
  **only when Clojure resolution is empty**, then build the src.zip location via
  `JdkIndex::class` + `uri::from_index_path`.
- [ ] **Step 4:** Run `cargo test --test test_java` → PASS; `cargo test` (all) →
  PASS (no regressions).
- [ ] **Step 5:** `git commit -m "Resolve + navigate to JDK classes, statics, constructors"`

### Task A8: Hover (signature + Javadoc)

**Files:** Modify `src/handlers/hover.rs`; Test `tests/test_java.rs`.

- [ ] **Step 1 (failing test):** `hover_java_member`/`hover_java_class`: hover on
  `Greeter/greet` shows a signature line (`static String greet(String name)`) and
  the Javadoc; hover on `Greeter` shows the class decl + Javadoc.
- [ ] **Step 2:** Run `cargo test --test test_java hover_java`. Expected: FAIL.
- [ ] **Step 3:** In `hover.rs`, when Clojure hover is empty, consult
  `resolve_java_at` → `JdkIndex::class` → render a markdown hover (signature +
  Javadoc, plus `extends`/`implements` for a class).
- [ ] **Step 4:** Run `cargo test --test test_java hover_java` → PASS.
- [ ] **Step 5:** `git commit -m "Hover Javadoc + signatures for JDK classes/members"`

### Task A9: e2e — navigation + hover (hermetic)

**Files:** Create fixture `.java` files; Modify `tests/test_e2e.rs`.

- [ ] **Step 1:** Add `tests/fixtures/jdk_src/java.base/demo/lib/Greeter.java` and
  `tests/fixtures/jdk_src/java.base/java/lang/Sample.java`. Add an e2e helper that
  zips `tests/fixtures/jdk_src/**` into a temp `src.zip`.
- [ ] **Step 2 (e2e test):** `test_e2e_java_definition_and_hover`: build the temp
  `src.zip`, `start_with_env(project, &[("CLJ_PULSE_JDK_SRC", &src_zip)])`,
  `initialize`, `wait_for_log("JDK source indexed")`, `did_open` a file importing
  `demo.lib.Greeter`. Assert goto-def on `Greeter` and on `Greeter/greet` returns a
  `jar:…/src.zip!/…Greeter.java` URI at the expected line; hover shows the Javadoc.
- [ ] **Step 3:** Run `cargo test --test test_e2e java` → PASS, then `bb check &&
  bb e2e` → PASS.
- [ ] **Step 4:** `git commit -m "e2e: navigate + hover into JDK src.zip"`

---

## Tasks — Phase B: completion + signature help

### Task B1: Completion — class names + static members

**Files:** Modify `src/handlers/completion.rs`; Test `tests/test_completion.rs`.

- [ ] **Step 1 (failing tests):** In `test_completion.rs`: completion at
  `Greeter/gr|` offers `greet`; at `(:import [demo.lib Gr|` offers `Greeter`; at
  `Sam|` (auto-java.lang position) offers `Sample`. Each item carries an
  appropriate kind (Method/Field/Class).
- [ ] **Step 2:** Run `cargo test --test test_completion java`. Expected: FAIL.
- [ ] **Step 3:** In `completion.rs`, detect the position class (`:import` /
  type-hint / `Class.` / `Class/`); offer class names by prefix over
  `class_entries`, and static members of the resolved class after `Class/` (via
  `JdkIndex::class`). No instance completion.
- [ ] **Step 4:** Run `cargo test --test test_completion java` → PASS; full
  `cargo test` → PASS.
- [ ] **Step 5:** `git commit -m "Complete JDK class names and static members"`

### Task B2: Signature help — static methods + constructors

**Files:** Modify `src/handlers/signature.rs`; Test `tests/test_java.rs`.

- [ ] **Step 1 (failing tests):** Signature help inside `(Greeter/greet ▏)` shows
  `greet(String name)`; inside `(Greeter. ▏)` shows `Greeter(int seed)`; overloads
  produce multiple `SignatureInformation`.
- [ ] **Step 2:** Run `cargo test --test test_java signature`. Expected: FAIL.
- [ ] **Step 3:** In `signature.rs`, when the call head is a JDK static method or a
  constructor, emit one `SignatureInformation` per overload from `JavaClassInfo`.
- [ ] **Step 4:** Run `cargo test --test test_java signature` → PASS.
- [ ] **Step 5:** `git commit -m "Signature help for JDK static methods + constructors"`

### Task B3: e2e — completion + signature (hermetic)

**Files:** Modify `tests/test_e2e.rs`.

- [ ] **Step 1 (e2e test):** `test_e2e_java_completion_and_signature`: same fixture
  `src.zip` + `start_with_env`. Assert `completion` after `Greeter/` includes
  `greet`; `completion` in `:import` includes `Greeter`; `signatureHelp` inside
  `(Greeter/greet ` returns the parameter label.
- [ ] **Step 2:** Run `cargo test --test test_e2e java` → PASS, then `bb check &&
  bb e2e` → PASS.
- [ ] **Step 3:** `git commit -m "e2e: JDK completion + signature help"`

### Task B4: ROADMAP update

**Files:** Modify `docs/ROADMAP.md`.

- [ ] **Step 1:** Update the Java interop item (line 99) to note built-in/JDK class
  navigation, static-member navigation/completion, constructor signature help, and
  Javadoc hover via `src.zip` (libraries + decompilation still pending). Use the
  /writing-clearly skill.
- [ ] **Step 2:** `bb check && bb e2e` → PASS (final full gate).
- [ ] **Step 3:** `git commit -m "Roadmap: built-in Java navigation + completion"`

---

## Notes & limitations

- **Instance methods deferred.** `(.method obj)` is not navigated or completed;
  it needs receiver-type inference (a separate effort that also benefits the
  library phase). Static members, constructors, and class refs cover the bulk of
  interop without it.
- **Libraries deferred.** Classpath jar classes and `clojure.lang.*` are `.class`
  bytecode, out of scope here; a later phase adds a `cafebabe` reader.
- **No `src.zip` ⇒ feature absent.** JREs and stripped JDKs omit `lib/src.zip`;
  we never download. Clojure behavior is unaffected.
- **Cache version.** `CACHE_FORMAT_VERSION` 9 → 10 because `NsMeta` gains
  `imports`. JDK data itself is never persisted (lazy, in-memory; the eager
  class-name map rebuilds in milliseconds each startup).
- **Non-regression.** Java resolution is a strict fallback after Clojure
  resolution; `str/join`-style aliases never reach it. The full `cargo test` /
  `bb e2e` suite must stay green at every task.
- **Server behavior is not done until `bb e2e` passes** (per CLAUDE.md); the
  client-visible `jar:`/`src.zip` content path reuses the existing provider, so no
  new `bb e2e-nvim` protocol surface is introduced.
