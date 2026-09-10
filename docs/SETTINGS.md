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

## Where the defaults live

Change one of these and this page has to change with it
([RELEASE.md](RELEASE.md) checks that):

- `:projects` — `src/projects.rs` (`DEPS_CMD`, `LEIN_CMD`, `resolve`).
- `:kondo` — `src/kondo.rs` (`KondoConfig::default`, `DEFAULT_LIVE_MAX_KB`).
- `:lint-as` — `src/settings.rs`.
- `clojuredocs.path` — `src/clojuredocs.rs`.
- The environment variables — `src/tools.rs`, `src/index/jdk.rs`,
  `src/projects.rs`, `src/kondo.rs`, `src/main.rs`.
