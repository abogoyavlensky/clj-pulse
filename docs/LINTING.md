# Linting

[Back to README](../README.md)

clj-pulse lints in two tiers.

The **native** tier always runs. It is built into the server, needs nothing
installed, and reports five things: `unresolved-namespace`,
`unused-namespace`, `duplicate-require`, `unused-binding`, and
`unused-private-var`. It also powers the "Add require" and
"Clean namespace" quickfixes.

The **clj-kondo** tier runs when a `clj-kondo` binary is on your `PATH`. Then
clj-pulse spawns it once per lint pass, feeds it the unsaved buffer, and
publishes its findings alongside the native ones with `source: "clj-kondo"`.
That buys you clj-kondo's whole linter set (unresolved symbols, arities, syntax
errors, unused bindings, and the rest) and your existing
`.clj-kondo/config.edn`: linter levels, `:lint-as`, and excludes all apply
exactly as they do on the command line. The config is resolved from the file
being linted, so in a monorepo each subproject's own `.clj-kondo/config.edn`
wins over the workspace root's. One difference from the command line: clj-pulse
asks clj-kondo to report every occurrence of an unresolved namespace, symbol,
or var, where the CLI reports only the first in each file.

When a clj-kondo run succeeds it owns the five codes above, and the native
copies are dropped for that pass so no squiggle appears twice. When clj-kondo
is missing, disabled, slow, or broken, the native diagnostics are published
unchanged. Losing the binary never loses your diagnostics.

Install clj-kondo from [its own instructions](https://github.com/clj-kondo/clj-kondo/blob/master/doc/install.md),
then change the clj-pulse configuration or restart the server so it
checks for the binary again.

clj-pulse looks for `clj-kondo` (and the `clojure` CLI it runs for classpath
resolution) on `PATH` first, then in the usual install directories: mise shims
(`~/.local/share/mise/shims`), Homebrew (`/opt/homebrew/bin`,
`/usr/local/bin`, Linuxbrew), `~/.cargo/bin`, `~/.local/bin` and `~/bin`. That
covers an editor started from the Dock or an app menu, whose `PATH` lacks what
your shell adds. A mise shim picks the version the project's mise config pins,
because clj-pulse runs it from the file's own directory. When clj-kondo is not
found, the log line (and the extension's lint status) says where it looked.

## Cross-file linters need a `.clj-kondo` directory

clj-kondo's cross-file linters (`invalid-arity`, `unresolved-var`) read a cache
of the signatures your project and its dependencies define. It writes that
cache into a `.clj-kondo` directory, and it never creates one itself. So run
this once per project:

```bash
mkdir -p .clj-kondo
```

With the directory present, clj-pulse scans your resolved classpath in the
background the first time it indexes the project, so library arities are known
without opening a single file. Editors that support work-done progress show
this as "Linting classpath (clj-kondo)". Without the directory, buffer linting
still works; only the cross-file linters stay quiet.

## Settings

```clojure
;; .clj-pulse/config.edn - defaults made explicit
{:kondo {:enabled true
         :path "clj-kondo"
         :live-max-kb 256}}
```

`:enabled` means "use clj-kondo when it is found", not "require it". Set it to
`false` to stay on native lints only; clj-pulse then never probes for the
binary or spawns it. `:path` names a program, not a command line: a bare name
is resolved through `PATH` and the install directories above, and an absolute
path is used verbatim. `mise exec -- clj-kondo` cannot work there; use the
path `mise which clj-kondo` prints, or the shim. All three keys apply live,
with no restart.

See [Settings](SETTINGS.md) for these three keys beside every
other setting, with the initialization options and environment variables.

`:live-max-kb` keeps clj-kondo off the keystroke path for very large files.
While you type in a buffer larger than this many KiB, each pass publishes the
native tier alone; opening and saving the file still run clj-kondo, so its full
findings are never more than a save away. `0` removes the limit. clj-kondo
takes close to a second on a 450 KiB file, and that time is its own, so this
is the one lever over when it runs.

The VS Code extension exposes `clojurePulse.kondo.enabled` and
`clojurePulse.kondo.path`, and shows which tier is active in its status-bar
tooltip. `clojurePulse.kondo.liveMaxKb` is pending in the extension; until it
ships, set `:live-max-kb` in `.clj-pulse/config.edn`.
