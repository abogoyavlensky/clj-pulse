# Settings

Every setting clj-pulse reads, with the default the parsers actually use. Three
places carry configuration:

1. [`.clj-pulse/config.edn`](#clj-pulseconfigedn) at the workspace root — the
   file every client shares.
2. [`initializationOptions`](#initializationoptions) — what the editor sends at
   startup, and pushes again on a settings change.
3. [Environment variables](#environment-variables) — machine-level overrides.

The editor layer wins over the file per key, and the file wins over the
defaults. A key nobody sets keeps its default. Clojure Pulse's own settings
flatten the classpath keys (`classpathEnabled`, `classpathCommand`) and the
extension nests them again before sending them on, so the two columns below
name the same setting.

## `.clj-pulse/config.edn`

```clojure
{:projects [{:path "." :classpath {:enabled true :cmd "clojure -A:dev:test -Spath"}}
            {:path "apps/api" :classpath {:enabled true}}]
 :lint-as {my.lib/defentity clojure.core/def}
 :kondo {:enabled true :path "clj-kondo" :live-max-kb 256}}
```

| Key | Default | Live reload | Clojure Pulse setting |
|---|---|---|---|
| `:projects` | `[]` — every project is detected, no overrides | yes | `clojurePulse.projects` |
| `:projects […] :path` | required; workspace-root-relative, `"."` is the root | yes | `clojurePulse.projects[].path` |
| `:projects […] :classpath :enabled` | `true` for the workspace root, `false` for sub-projects | yes | `clojurePulse.projects[].classpathEnabled` |
| `:projects […] :classpath :cmd` | `clojure -A:dev:test -Spath` (deps.edn), `lein classpath` (Leiningen), none (lgx) | yes | `clojurePulse.projects[].classpathCommand` |
| `:lint-as` | `{}`, over whatever `.clj-kondo/config.edn` declares | yes | — (file only) |
| `:kondo :enabled` | `true` — use clj-kondo when the binary is found | yes | `clojurePulse.kondo.enabled` |
| `:kondo :path` | `clj-kondo`, resolved through PATH plus the well-known install dirs | yes | `clojurePulse.kondo.path` |
| `:kondo :live-max-kb` | `256` — buffers above this skip clj-kondo on keystrokes, never on open or save; `0` means no limit | yes | `clojurePulse.kondo.liveMaxKb` (pending in the extension) |

Notes:

- `:classpath :cmd` runs verbatim in the project's own directory, and its last
  stdout line is the classpath. It is the third and last resort: clj-pulse
  scans each project's `:paths` first, then reads its `.cpcache`, and any
  failure of the command degrades to that cached result.
- `:lint-as` maps a *defining macro's* fully-qualified name to the `def` form
  it behaves like, exactly as clj-kondo does. clj-pulse reads
  `.clj-kondo/config.edn` for the same key and its own file wins per macro.
  A target that names no `def`-family form drops the macro.
- Live reload watches `**/.clj-pulse/config.edn` and `**/.clj-kondo/config.edn`:
  a save re-resolves classpaths, reloads `:lint-as` and re-indexes, and
  re-probes clj-kondo. No restart.

## `initializationOptions`

What a client that does not read `.clj-pulse/config.edn` sends instead. The
shape is the config file's, in JSON, with the same defaults:

```json
{
  "projects": [{ "path": ".", "classpath": { "enabled": true, "cmd": "clojure -Spath" } }],
  "kondo": { "enabled": true, "path": "clj-kondo", "liveMaxKb": 256 },
  "clojuredocs": { "path": "/path/to/clojuredocs-export.json" }
}
```

| Option | Default | Notes |
|---|---|---|
| `projects` | `[]` | The `:projects` entries in JSON: `path`, `classpath.enabled`, `classpath.cmd`. An entry without `path` is ignored. |
| `kondo` | `{}` | `enabled`, `path`, `liveMaxKb` — note the JSON spelling of the last one. |
| `clojuredocs.path` | none | A local [ClojureDocs export](https://clojuredocs.org/clojuredocs-export.json), read on the first request that needs it. Without it, `clojurePulse/clojureDocs` answers with an error. |

A `workspace/didChangeConfiguration` push carries the same object under a
`clojurePulse` section, and replaces the whole editor layer — send every key
you set, since a `projects`-only push erases `kondo`.

## Environment variables

| Variable | Default | Effect |
|---|---|---|
| `CLJ_PULSE_TOOL_DIRS` | unset | PATH-style list of directories to search for child processes (`clj-kondo`, the classpath command), *replacing* the built-in list of well-known install dirs (mise shims, Homebrew, `~/.cargo/bin`, …). Empty means PATH only. |
| `CLJ_PULSE_JDK_SRC` | unset | Path to the JDK's `src.zip`, tried before `JAVA_HOME`, `java` on PATH, and the well-known JDK locations. |
| `CLJ_PULSE_CLOJUREDOCS_EXPORT` | unset | Path to a ClojureDocs export, for the ignored test that parses a real download. |
| `CLJ_PULSE_DISABLE_CLASSPATH_CLI` | unset | Non-empty forces `:classpath {:enabled false}` for every project: no `clojure`/`lein` subprocess, stage-2 `.cpcache` results only. |
| `CLJ_PULSE_DISABLE_KONDO` | unset | Non-empty forces `:kondo {:enabled false}`: the native lints alone, whatever the config says. |
| `CLJ_PULSE_TEST_PANIC` | unset | Test-only. Registers `clojurePulse/__testPanic`, a method that panics on purpose, so the suite can prove a panicking handler does not take the server down. Never set it in normal use. |

## Configuration examples

clj-pulse reads an optional `.clj-pulse/config.edn` at the workspace root and
merges `:lint-as` from `.clj-kondo/config.edn`. The tables above list every
setting; [Linting](LINTING.md) explains the two diagnostic tiers.

### Projects and classpaths

`:projects` controls per-project classpath resolution. clj-pulse detects every
directory holding a `deps.edn`, `project.clj`, or `lgx.edn` (up to four levels
deep, honoring `.gitignore`). It indexes their sources, reads cached
classpaths for deps.edn projects, resolves lgx dependencies internally, and
uses locally available direct dependencies as the Leiningen fallback.
Project discovery needs no configuration. Each deps.edn or Leiningen project
can also run a shell
command that resolves its full classpath, so dependencies declared under
aliases (`:test`, `:dev`, ...) are indexed and navigable too (lgx projects
resolve their dependencies internally and never run a command). The command
runs in the project's directory
and its last stdout line is taken as the classpath; with a warm `.cpcache`
the clojure CLI skips the JVM entirely, and on the first resolve - or after a
deps.edn change - it may download dependencies. By default the command is
enabled only for the workspace root:

```clojure
;; .clj-pulse/config.edn - defaults made explicit
{:projects [{:path "."             ; "." is the workspace root
             :classpath {:enabled true
                         :cmd "clojure -A:dev:test -Spath"}}
            {:path "apps/backend"  ; subprojects default to :enabled false
             :classpath {:enabled false
                         :cmd "clojure -A:dev:test -Spath"}}]}
```

Entries are overrides: every detected project exists whether or not it is
listed, and an entry changes only the keys it names. The default `:cmd` is
`clojure -A:dev:test -Spath` for deps.edn projects and `lein classpath` for
Leiningen ones; change it to select other aliases or a different tool. Set
`:enabled true` on a subproject to resolve its full classpath too, or
`:enabled false` on the root to opt out - a deps.edn project then indexes
only what `.cpcache` already holds (a Leiningen project falls back to the
direct dependency JARs named in `project.clj`; lgx resolution is unaffected).
Listing a path detection skipped (for example a
gitignored checkout with its own `deps.edn`) adds it as a project. Editing
the config applies live, no restart needed.

Editors can also force a full refresh with the custom `clojurePulse/rescan`
request: it re-runs project detection, re-reads the config, and re-resolves
every enabled project's classpath - the way to retry a failed resolution or
pick up a subproject created inside a gitignored directory, where no file
watcher ever fires. The request returns null immediately and the work runs in
the background, emitting `clojurePulse/librariesChanged` as it progresses -
clients should simply re-request on each notification (one is guaranteed at
the end even when nothing changed, so the panel never waits forever). While a
classpath command
runs, clj-pulse reports standard LSP work-done progress
("Resolving classpath: ...") to clients that advertise the
`window.workDoneProgress` capability, so the editor shows why library
navigation isn't ready yet.

### Custom defining macros

`:lint-as` (also read from `.clj-kondo/config.edn`) tells clj-pulse to treat a
custom macro like a built-in `def` form so the name it introduces becomes
navigable:

```clojure
;; .clj-pulse/config.edn  (or .clj-kondo/config.edn)
{:lint-as {my.app/defcomponent clojure.core/def}}
```

With that mapping, go-to-definition, hover, find-references, and the document
outline all resolve a name defined by `(defcomponent thing ...)`. clj-pulse merges
the two files (with `.clj-pulse/config.edn` winning on conflicts) and watches
them, reloading `:lint-as` when either changes, with no restart needed. A
project that
already configures `:lint-as` for clj-kondo works with no extra setup. Only
mappings to `def`-family forms (`def`, `defn`, `defmethod`, ...) take effect;
others (such as `clojure.core/for`) are ignored.

`.clj-pulse/` also holds generated data (`jar-cache/`, `server.log`), so commit
`config.edn` and gitignore the rest.

## Where the defaults live

Change one of these and this page has to change with it
([RELEASE.md](RELEASE.md) checks that):

- `:projects` — `src/projects.rs` (`DEPS_CMD`, `LEIN_CMD`, `resolve`).
- `:kondo` — `src/kondo.rs` (`KondoConfig::default`, `DEFAULT_LIVE_MAX_KB`).
- `:lint-as` — `src/settings.rs`.
- `clojuredocs.path` — `src/clojuredocs.rs`.
- The environment variables — `src/tools.rs`, `src/index/jdk.rs`,
  `src/projects.rs`, `src/kondo.rs`, `src/main.rs`.
