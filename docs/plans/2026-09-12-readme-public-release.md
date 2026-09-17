# README for the public release

Status: done

## Agreed direction

Open with "A fast-starting, low-memory Clojure language server." Keep the
README around 100-150 lines and do not mention Rust. Highlight performance,
Integrant navigation, let-go/lgx, and monorepo support. Keep monorepos out of
the opening sentence.

## Work

- [x] Rewrite the README with the four highlights, a quick start, a small
      benchmark table, support boundaries, and links to detailed docs.
- [x] Preserve the feature reference, editor setup, linting, configuration,
      performance details, and development commands in linked guides.
- [x] Update the release checklist and documentation map in AGENTS.md.
- [x] Check local links and anchors, Markdown structure, factual claims,
      the absence of Rust in README, and run `bb check`.

No server or editor behavior changes. The editor, benchmark, and soak gates
are not required for this documentation change; existing benchmark results
retain their dates, versions, measurement definitions, and caveats.

## Verification

- README reduced from 570 lines to 112, with the agreed opening and no Rust
  mention. All original feature bullets are preserved in `docs/FEATURES.md`.
- Checked 70 local links and anchors across all 11 changed Markdown files,
  balanced code fences, and valid JSON snippets. The copied Neovim handler's
  executable lines match `editors/nvim/jar.lua`.
- `mise exec -- bb check` passed: formatting, clippy, and all non-ignored
  tests. Used mise because `bb` was not on the shell's PATH.
- `git diff --check` passed.
