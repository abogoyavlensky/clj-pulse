# clj-pulse

A fast-starting, low-memory Clojure language server.

## Highlights

- **Fast startup, low memory.** Navigate your project while dependencies index
  in the background. On the recorded Metabase benchmark, the first project
  definition was available in 1.4 seconds and every dependency in 1.6, with
  412 MiB of memory once settled.
  See [Performance](#performance) for the comparison and measurement details.
- **Integrant navigation.** Jump from a component key in `config.edn` or an
  `#ig/ref` to its `ig/init-key` implementation. Find references and rename
  qualified keywords across indexed source and config files.
- **let-go support.** Navigation, completion, references, and rename for
  [let-go](https://github.com/nooga/let-go) `.lg` files, with dependencies
  resolved from [lgx](https://github.com/abogoyavlensky/lgx) projects.
- **Monorepo support.** Automatically discover projects using `deps.edn`,
  Leiningen, or lgx within one workspace. Navigate across their sources and
  cached dependencies, with [classpath settings per project](docs/SETTINGS.md#projects-and-classpaths).

Everyday tools include fuzzy completion with auto-require, hover and signature
help, references, rename, keyword completion, symbol search, and namespace
quickfixes. Built-in diagnostics work out of the box; optional clj-kondo adds
its full linter set. See the [feature reference](docs/FEATURES.md).

## Quick start

Install the server on macOS or Linux with Homebrew:

```sh
brew install abogoyavlensky/tap/clj-pulse
```

Or with mise:

```sh
mise use -g github:abogoyavlensky/clj-pulse
```

For VS Code, install the [Clojure Pulse extension](https://github.com/abogoyavlensky/clojure-pulse-vscode#installation)
and open your project folder. It finds `clj-pulse` on your `PATH`. To select a
different binary, set:

```json
{
  "clojurePulse.server.path": "/path/to/clj-pulse"
}
```

If you use [Calva](https://calva.io/), set its language server path:

```json
{
  "calva.clojureLspPath": "/path/to/clj-pulse"
}
```

Also available: [Neovim](docs/EDITORS.md#neovim), [Zed](docs/EDITORS.md#zed), and
[manual downloads](docs/EDITORS.md#manual-download).

For full diagnostics, [install clj-kondo](https://github.com/clj-kondo/clj-kondo/blob/master/doc/install.md).
Create a `.clj-kondo` directory in each project to enable its cross-file cache:

```sh
mkdir -p .clj-kondo
```

See [Linting](docs/LINTING.md) for configuration and troubleshooting.

## Performance

What a user waits for after opening Metabase (1,400+ files), recorded on a
MacBook Pro (M1 Pro, 16 GB, macOS 26.5), 2026-09-22: clj-pulse 0.5.4 and
clojure-lsp 2026.07.06-14.34.19, both at their defaults, with clj-kondo
installed. Cold is the first open of a fresh checkout, one run; warm is the
next open, using the caches the first one left, as the median of three runs.

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First navigation | 2.1 s | 1.4 s | 106 s | 28 s |
| All dependencies navigable | 3.9 s | 1.6 s | 106 s | 28 s |
| clj-kondo finished | 25 s | 16 s | 106 s | 28 s |
| Memory once settled | 427 MiB | 412 MiB | 2,705 MiB | 2,151 MiB |
| Definition (median of 20) | 11 ms | 11 ms | 6 ms | 8 ms |

The servers do different work at startup: clj-pulse makes project navigation
available first, then dependency navigation, and warms clj-kondo last, while
clojure-lsp analyzes everything before it answers. clojure-lsp answers a
definition faster once settled in these benchmarks. These results describe the
recorded project and machine; they are not a guarantee for every workspace.

See [Performance](docs/PERFORMANCE.md) for the same run on a Linux
container, the clj-kondo corpus, diagnostics latency, pinned project
commits, and methodology. Reproduce with `bb bench`.

## Support and status

clj-pulse is under active development; [bug reports](https://github.com/abogoyavlensky/clj-pulse/issues)
and real-world feedback are welcome.

- Clojure Pulse, Calva, and Neovim are the primary editor targets. Zed is
  best effort and does not yet support library JAR navigation.
- ClojureScript is best effort: `.cljs` and `.cljc` files are indexed, but
  `:require-macros` and shadow-cljs classpaths are not supported.
- Java support covers JDK classes, static members, and constructors.
  Instance methods, library classes, and decompilation are not supported yet.
- Whole-document formatting is provided by the editor. Clojure Pulse uses
  cljfmt compiled from ClojureScript to JavaScript, bundled as a library.
  The server provides indent-on-Enter.

## Documentation

- [Feature reference](docs/FEATURES.md)
- [Installation and editor setup](docs/EDITORS.md)
- [Settings and monorepo configuration](docs/SETTINGS.md)
- [Linting](docs/LINTING.md)
- [Performance and benchmarks](docs/PERFORMANCE.md)
- [Development and verification](docs/DEV_SETUP.md)

## License

[MIT](LICENSE). Copyright (c) 2026 Andrey Bogoyavlenskiy.