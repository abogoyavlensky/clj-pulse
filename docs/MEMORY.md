# Memory

Durable findings about clj-pulse worth keeping in one place: known gaps, their
root causes in the code, and what a fix would involve. Complements the
forward-looking [ROADMAP.md](ROADMAP.md).

## Leiningen indexes only direct dependencies

### How deep each project type goes

clj-pulse resolves dependencies differently per project type, so the transitive
depth it indexes varies:

| Project type | Resolver | Transitive depth |
|---|---|---|
| `deps.edn` | reads `.cpcache/*.cp` (`src/classpath.rs`) | Full closure - the Clojure CLI already flattened it |
| let-go `lgx.edn` | `lgx::resolve` (`src/lgx.rs`) | Full transitive - breadth-first walk of each dep's own `:deps` |
| Leiningen `project.clj` | `leiningen::resolve` (`src/leiningen.rs`) | Direct deps only |

For `deps.edn`, clj-pulse does no resolution of its own. It reads the classpath
that `clojure -Spath` already wrote to `.cpcache`, which is the full transitive
set, and indexes every entry. For let-go, `lgx::resolve` walks each
dependency's own `:deps` until the queue drains, so depth is unbounded.

### The gap

Leiningen is the exception. `leiningen::resolve` reads `project.clj` as text and
maps only the direct `:dependencies` to JARs under `~/.m2`. Within that, it
skips any dependency that:

- declares no inline string version - `coord_from` in `src/leiningen.rs`
  requires the `[group/artifact "version"]` shape, or
- is not already downloaded to `~/.m2`.

It never reads a JAR's `pom.xml`, so it cannot discover transitive
dependencies. It never reads `:managed-dependencies` or a `lein-parent`
`:parent-project`, so versions inherited from a parent stay unknown. This is
deliberate: the module inspects `project.clj` only and never shells out to
`lein classpath`, which avoids JVM startup at the cost of completeness.

### Symptom

In a Leiningen project, go-to-definition fails for any symbol whose namespace
lives in a transitive or version-less dependency, because that JAR is never
indexed.

Example, from the `flockman` project: `(defcomponent ...)` uses the
`defcomponent` macro from `defcomponent-0.2.2.jar`. That JAR is a transitive
dependency (pulled in by a `com.flocktory/staff.*` library, absent from
`project.clj`), so it is never indexed and `lookup("defcomponent/defcomponent")`
returns nothing. The same applies to direct deps declared without a version,
such as `[com.flocktory/staff.guards]`, whose version comes from the parent.

This is not specific to macros. A macro is indexed like any other var
(`DefKind::Defmacro`), and a macro call resolves through `:refer` or an alias
exactly like a function call. The symbol is missing only because its JAR sits
off the resolved classpath.

### Stance: best effort, and never a JVM at startup

Leiningen is not a primary target for clj-pulse - `deps.edn` and let-go come
first - so its dependency support stays best effort. One principle is fixed:
**clj-pulse will not start a JVM.** That rules out shelling out to
`lein classpath`, which is the only fully accurate way to get Leiningen's
transitive and parent-inherited deps. Startup must stay fast and self-contained,
so we accept the gap rather than pay JVM cost.

Best-effort directions that respect the no-JVM rule, none urgent:

1. **Resolve version-less direct deps from `~/.m2`.** When a direct dep declares
   no version (the version comes from the parent project), look under
   `~/.m2/repository/<group>/<artifact>/` and take the only, or newest,
   downloaded version. Cheap and subprocess-free; covers parent-managed direct
   deps, but still not transitive ones.
2. **Reimplement enough Maven resolution in Rust.** Parse each dep's `pom.xml`
   and the parent's `:managed-dependencies`, then walk the tree. No subprocess,
   but a large and fragile effort: version ranges, exclusions, and profiles all
   apply.

For complete, accurate Leiningen support, clojure-lsp (which embeds clj-kondo
and resolves the real classpath) remains the better tool. See also the ROADMAP
entry "Leiningen classpath ... Transitive deps deferred".
