# clj-pulse

A fast-starting, low-memory Clojure language server.

## Highlights

- **Fast startup, low memory.** Navigate your project while dependencies index
  in the background. On the recorded Metabase benchmark, the first project
  definition was available in 3.1 seconds, with 353 MiB of memory once settled.
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

Also available: [Calva](docs/EDITORS.md#vs-code),
[Neovim](docs/EDITORS.md#neovim), [Zed](docs/EDITORS.md#zed), and
[manual downloads](docs/EDITORS.md#manual-download).

For full diagnostics, [install clj-kondo](https://github.com/clj-kondo/clj-kondo/blob/master/doc/install.md).
Create a `.clj-kondo` directory in each project to enable its cross-file cache:

```sh
mkdir -p .clj-kondo
```

See [Linting](docs/LINTING.md) for configuration and troubleshooting.

## Performance

Recorded warm runs on one Linux container (5 cores, 11 GiB RAM), 2026-09-10:
clj-pulse 0.5.0 and clojure-lsp 2026.07.06-14.34.19, both at their defaults.
Warm means a second start using the caches left by the first.

| Project | Metric | clj-pulse | clojure-lsp |
|---|---|---|---|
| Metabase | Time to first project definition | 3.1 s | 60 s |
| Metabase | Memory once settled | 353 MiB | 1,798 MiB |
| clj-kondo | Time to first project definition | 520 ms | 2.4 s |
| clj-kondo | Memory once settled | 90 MiB | 273 MiB |

The servers do different work at startup: clj-pulse makes project navigation
available while dependency indexing continues. clojure-lsp answers definitions
faster once settled in these benchmarks. These results describe the recorded
projects and machine; they are not a guarantee for every workspace.

See [Performance](docs/PERFORMANCE.md) for dependency navigation, request
latency, diagnostics, pinned project commits, and methodology. Reproduce with
`bb bench`.

## Support and status

clj-pulse is under active development; [bug reports](https://github.com/abogoyavlensky/clj-pulse/issues)
and real-world feedback are welcome.

- Clojure Pulse, Calva, and Neovim are the primary editor targets. Zed is
  best effort and does not yet support library JAR navigation.
- ClojureScript is best effort: `.cljs` and `.cljc` files are indexed, but
  `:require-macros` and shadow-cljs classpaths are not supported.
- Java support covers JDK classes, static members, and constructors.
  Instance methods, library classes, and decompilation are not supported yet.
- Whole-document formatting is provided by the editor; Clojure Pulse bundles
  its own formatter. The server provides indent-on-Enter.

See the [roadmap](docs/ROADMAP.md) for planned work.

## Documentation

- [Feature reference](docs/FEATURES.md)
- [Installation and editor setup](docs/EDITORS.md)
- [Settings and monorepo configuration](docs/SETTINGS.md)
- [Linting](docs/LINTING.md)
- [Performance and benchmarks](docs/PERFORMANCE.md)
- [Development and verification](docs/DEV_SETUP.md)

## License

[MIT](LICENSE). Copyright (c) 2026 Andrey Bogoyavlenskiy.
