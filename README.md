# clj-pulse

A fast-starting, low-memory Clojure language server.

## Highlights

- **Fast.** The project index is built in under a second for a typical
  project and within a few seconds for a very large one, so you navigate right
  away. Dependencies become navigable moments later, and warm restarts are
  faster still. Definition answers in
  under a millisecond, and diagnostics follow an edit within a fraction of a
  second. See [Performance](#performance) for details.
- **Integrant navigation.** Jump from a component key in `config.edn` or an
  `#ig/ref` to its `ig/init-key` implementation. Find references and rename
  qualified keywords across indexed source and config files.
- **let-go support.** Navigation, completion, references, and rename for
  [let-go](https://github.com/nooga/let-go) `.lg` files, with dependencies
  resolved from [lgx](https://github.com/abogoyavlensky/lgx) projects.
- **Monorepo support.** Automatically discover projects using `deps.edn`,
  Leiningen, or lgx within one workspace. Navigate across their sources and
  cached dependencies, with [classpath settings per project](docs/SETTINGS.md#projects-and-classpaths).

Everyday tools include: fuzzy completion with auto-require, hover and signature
help, references, rename, keyword completion, symbol search, namespace
quickfixes, and an `ns` form for each new file. Built-in diagnostics work out
of the box; optional clj-kondo adds its full linter set. See the [feature reference](docs/FEATURES.md).

## Quick start

Install the server on macOS or Linux with Homebrew:

```sh
brew install abogoyavlensky/tap/clj-pulse
```

Or with mise:

```sh
mise use -g github:abogoyavlensky/clj-pulse
```

For VS Code, if you use [Calva](https://calva.io/), set its language server path:

```json
{
  "calva.clojureLspPath": "/path/to/clj-pulse"
}
```

Alternatively, install the [Clojure Pulse extension](https://github.com/abogoyavlensky/clojure-pulse-vscode#installation) and open your project folder. The extension includes `clj-pulse` binary. To select a different binary, set:
 
```json
{
  "clojurePulse.server.path": "/path/to/clj-pulse"
}
```

Also available: [Neovim](docs/EDITORS.md#neovim), [Zed](docs/EDITORS.md#zed), and
[manual downloads](docs/EDITORS.md#manual-download).

For full diagnostics, [install clj-kondo](https://github.com/clj-kondo/clj-kondo/blob/master/doc/install.md).
Create a `.clj-kondo` directory in each project to enable its cross-file cache.

See [Linting](docs/LINTING.md) for configuration and troubleshooting.

## Performance

What a user waits for after opening Metabase (1,400+ files), recorded on a
MacBook Pro (M1 Pro, 16 GB, macOS 26.5), 2026-10-27: clj-pulse 0.5.6 and
clojure-lsp 2026.07.06-14.34.19, both at their defaults, with clj-kondo
installed. Cold is the first open of a fresh checkout, one run; warm is the
next open, using the caches the first one left, as the median of three runs.

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First navigation | 2.9 s | 1.8 s | 108 s | 24 s |
| All dependencies navigable | 4.5 s | 1.9 s | 108 s | 24 s |
| clj-kondo finished | 24 s | 16 s | 108 s | 24 s |
| Memory once settled | 555 MiB | 530 MiB | 2,747 MiB | 1,772 MiB |
| Definition (median of 20) | < 1 ms | < 1 ms | 6 ms | 3 ms |
| Keystroke to diagnostics, 251 KiB file | 259 ms | 257 ms | 346 ms | 388 ms |

The servers do different work at startup: clj-pulse makes project navigation
available first, then dependency navigation, and warms clj-kondo last, while
clojure-lsp analyzes everything before it answers. In clj-pulse, clj-kondo is
optional and only adds lint warnings; navigation, completion and the built-in
lints never wait for it. Each server waits briefly after an edit before
linting it (50 ms for clj-pulse, 75 ms for clojure-lsp); the keystroke row
includes that wait. These results
describe the recorded project and machine; they are not a guarantee for every
workspace.

See [Performance](docs/PERFORMANCE.md) for the clj-kondo corpus,
diagnostics latency, pinned project commits, and methodology. Reproduce
with `bb bench`.

## Support and status

[Bug reports](https://github.com/abogoyavlensky/clj-pulse/issues) and
real-world feedback are welcome.

- Clojure Pulse, Calva, and Neovim are the primary editor targets. Zed is
  best effort and does not yet support library JAR navigation.
- Compared with clojure-lsp, the everyday navigation and editing features are
  covered (definition, references, rename, hover, completion, symbols,
  diagnostics, namespace quickfixes), and clj-pulse matches or beats it on
  every benchmarked speed row. Not available yet: refactoring commands
  (extract function, inline symbol, introduce let, and the like), go to
  implementation, code lens, and updating references when a file is renamed.
  Paredit-style structural editing is left to the editor.
- Vars defined in files a namespace `(load …)`s and `(in-ns '…)`s are
  navigable, including `clojure.pprint` and `clojure.core`'s split files. The
  loading namespace's aliases are not known inside a loaded file.
- Java support covers JDK classes, static members, and constructors.
  Instance methods, library classes, and decompilation are not supported yet.
- Whole-document formatting is provided by the editor, not yet by the server.
  Clojure Pulse uses cljfmt compiled from ClojureScript to JavaScript, bundled
  as a library. The server provides indent-on-Enter.

## Documentation

- [Feature reference](docs/FEATURES.md)
- [Installation and editor setup](docs/EDITORS.md)
- [Settings and monorepo configuration](docs/SETTINGS.md)
- [Linting](docs/LINTING.md)
- [Performance and benchmarks](docs/PERFORMANCE.md)
- [Development and verification](docs/DEV_SETUP.md)

## License

[MIT](LICENSE). Copyright (c) 2026 Andrey Bogoyavlenskiy.