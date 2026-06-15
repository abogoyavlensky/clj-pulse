# Release flow

Releases are **git-tag-driven**. Pushing a tag is the only trigger; there is no
manual artifact upload.

## Source of truth

The binary reports its version from `env!("CARGO_PKG_VERSION")`
(`src/server.rs`), so the `version` in **`Cargo.toml` is the single source of
truth**. The release CI refuses to publish if the tag and `Cargo.toml` disagree.

## Cutting a release

1. **Bump `version` in `Cargo.toml`** (let `Cargo.lock` update) and commit it.
2. **Verify** locally: `bb check` (fmt + clippy `-D warnings` + tests).
3. **Tag and push:**

   ```sh
   bb tag
   ```

   `bb tag` reads the version from `Cargo.toml`, creates the tag `v<version>`,
   and pushes it to `origin`. (Equivalent to `git tag v0.0.1-alpha-3 &&
   git push origin v0.0.1-alpha-3`.) Git itself guards against re-tagging: the
   command fails if the tag already exists locally or on the remote.

The leading `v` is required — the release workflow strips it and compares the
remainder to `Cargo.toml`.

## What the CI does

Pushing the tag triggers `.github/workflows/release.yml`:

1. **`version-check`** — fails fast if `${TAG#v}` ≠ `Cargo.toml` version, so an
   artifact can never claim a version different from its tag.
2. **`build`** — cross-compiles a 5-target matrix and packages each binary:
   - `aarch64-apple-darwin`, `x86_64-apple-darwin`
   - `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`
   - `x86_64-pc-windows-msvc`

   Unix targets are packaged as `.tar.gz`, Windows as `.zip`.
3. **`release`** — generates `sha256` checksums (`checksums.txt`) and publishes a
   GitHub Release via `softprops/action-gh-release` with **auto-generated release
   notes** and all artifacts attached.

Users install by downloading the archive for their platform from the
[releases page](https://github.com/abogoyavlensky/clj-pulse/releases) (verifying
against `checksums.txt`) or via mise — see the README.

## Notes

- **No `CHANGELOG`** — release notes are generated from commit/PR history
  (`generate_release_notes: true`), so keep PR titles clean.
- `bb release` is just a local `cargo build --release`; it is **not** part of
  publishing. Use `bb tag` to cut a release.
- Versions so far are alpha prereleases (`0.0.1-alpha-N`), which are valid
  semver prerelease identifiers and accepted by Cargo unchanged.
