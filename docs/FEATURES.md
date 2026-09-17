# Feature reference

[Back to README](../README.md)

Language features:

- **Go to definition** - across project source, library JARs (via `jar:` URIs),
  and source-directory deps (git deps in `~/.gitlibs`, `:local/root`).
- **Autocomplete** - locals, project symbols, `:refer`red and alias-qualified
  vars (including `:refer :all` and `:use`), namespace and alias names,
  `clojure.core`, special forms, and JDK classes. Names match fuzzily (exact,
  prefix, substring, subsequence) and rank by match quality first, then by how
  local the name is. Docstrings load per item through `completionItem/resolve`,
  so a long list stays cheap.
- **Keyword completion** - typing `:` or `::` offers the keywords the project
  already uses, in the notation being typed: `::name` and `::alias/name` under
  `::`, `:name` and `:ns/name` under a single colon. The current namespace's
  keywords come first, then the most-used ones.
- **Auto-require on accept** - completion also offers vars from namespaces the
  file has not required yet, labelled `alias/name`; accepting one inserts the
  `:require` along with the name. The pool is every project namespace, aliased
  by its last segment, plus the conventional aliases (`str`, `set`, `io`,
  `edn`, `walk`, `pp`, `async`, `sh`). An alias the file has already bound to
  something else is never proposed.
- **Hover** - docstrings and signatures for the symbol under the cursor.
- **ClojureDocs** - the `clojurePulse/clojureDocs` request returns the
  [ClojureDocs](https://clojuredocs.org) entry (docstring, arglists, community
  examples, see-alsos) for the symbol at a position, resolved through the same
  alias-aware lookup as hover, or for a given `ns/name`. Served from a local
  export file the editor points at, never the network - see
  [ClojureDocs data](EDITORS.md#clojuredocs-data).
- **Signature help** - argument hints while typing a call (after `(` and spaces).
- **Find references** - locate every usage of a symbol across the project.
- **Rename** - rename a project symbol and all of its references, or a local
  binding (params, `let`/`loop`/`for` bindings, destructured names) within
  its scope. The editor's rename box opens on the exact token that will change,
  and names that cannot be renamed - library and built-in symbols,
  `:keys`-destructured bindings - are refused up front with a reason.
- **Keyword rename** - rename a qualified keyword across the project. Each site
  keeps the notation it was written in, because only the name at the end of the
  token is replaced: `::db`, `::alias/db` and `:my.app/db` all become `::store`,
  `::alias/store` and `:my.app/store`. Integrant `config.edn` files are rewritten
  with the sources. Unqualified keywords, keywords of a library namespace, and
  keywords read through `{::keys [db]}` destructuring (where the name is also the
  binding) are refused rather than half-renamed.
- **Keyword navigation** - go to definition and find references on namespaced
  keywords, including Integrant component keys: jump from `:my.app/db` in a
  `config.edn` system map (or an `#ig/ref`) to its `(defmethod ig/init-key ::db ...)`.
- **Java interop (built-in/JDK)** - go to definition, Javadoc hover, completion,
  and signature help for JDK classes, static members, and constructors. (Instance methods
  (`(.foo obj)`), library classes, and decompilation aren't supported yet.)
- **Highlight occurrences** - the editor underlines every occurrence of the
  symbol under the cursor in the current buffer, marking its definition as a
  write and each usage as a read. Locals resolve by scope, so a parameter that
  shadows a var highlights only itself.
- **Expand selection** - `textDocument/selectionRange` grows the selection one
  form at a time along the parse tree: the name half of `str/join`, then the
  whole symbol, then the call, then the enclosing `defn`.
- **Document symbols** - outline of the definitions in the current file.
- **Workspace symbols** - fuzzy symbol search across the whole project.
- **Code actions** - "Add require" quickfix for a qualified symbol whose
  namespace isn't required yet, and "Clean namespace" (`source.organizeImports`)
  that drops unused and duplicate requires.
- **Diagnostics** - unresolved-namespace, unused-namespace, duplicate-require,
  unused-binding, and unused-private-var warnings, updated live as you type;
  clj-kondo's full linter set as well when the binary is installed (see
  [Linting](LINTING.md)).
- **Indent-on-Enter** - pressing Enter indents the new line to the structurally
  correct column (`textDocument/onTypeFormatting`): vectors, maps, and
  non-symbol-headed lists align to their first element; symbol-headed lists get
  a 2-space body indent. For clients using this server capability, enable
  on-type formatting.
  Clojure Pulse handles indentation itself and disables server on-type
  formatting. With Parinfer in Paren/Smart mode, let Parinfer manage
  indentation; its Indent Mode can complement server indentation.
- **Ignored-form dimming** - the server reports the ranges of `#_` discard
  forms and `(comment ...)` blocks over a `clojurePulse/ignoredForms` request; the
  editor extension dims them (brackets included, nested and multi-line) with a
  decoration a syntax grammar can't produce. No theme configuration needed.

Clojure & project support:

- **File types:** `.clj`, `.cljs`, `.cljc`, `.lg`.
- **ns forms:** `:as`, `:as-alias`, `:refer` (including `:refer :all` and
  `(:use ns)`), `:rename`, `:refer-clojure :exclude` / `:rename`, `:import`,
  reader conditionals, and legacy prefix lists `(clojure [set :as s] string)`.
  `declare` is indexed too, so a name that is only declared still navigates.
- **Project types:** `deps.edn` (resolved from the `.cpcache` classpath),
  Leiningen `project.clj`, and let-go `.lg` projects, whose lgx dependencies at `lgx.edn`
  (git and `:local/root` deps under `~/.lgx/gitlibs`) are indexed and navigable.
- **Library indexing:** symbols from JAR dependencies and source-directory deps
  are indexed and navigable, with project symbols always taking precedence.
- **Live index:** incremental edits, re-index on save, and file watching keep the
  index fresh across git pulls and branch switches; files outside the project's
  `:paths` are indexed when opened.

> [!NOTE]
> **Dependency depth:** every project type indexes the full transitive
> dependency tree - deps.edn from the resolved classpath, let-go from
> `lgx.edn`, and Leiningen from `lein classpath`, run in the background and
> enabled by default for the workspace root. Where that command is turned off
> or fails, a Leiningen project falls back to the direct dependencies that name
> an explicit version and already live in `~/.m2`. See
> [Settings](SETTINGS.md#projects-and-classpaths).
