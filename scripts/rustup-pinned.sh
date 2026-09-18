#!/usr/bin/env bash
# Install and select the Rust toolchain .mise.toml pins, through rustup alone.
#
# CI's Windows jobs use this instead of mise-action: mise on Windows installs
# every tool in .mise.toml whatever `install_args` says, and the clojure
# plugin's post-install hook is a Unix shell script, so the step dies before
# rust is even considered. The Windows runners ship rustup, and the version
# still comes from .mise.toml, so the toolchain stays pinned in one place.
set -euo pipefail

version=$(sed -nE 's/^rust = \{ version = "([^"]+)".*/\1/p' .mise.toml)
[ -n "$version" ] || { echo "no rust version in .mise.toml" >&2; exit 1; }

rustup toolchain install "$version" --profile minimal --component clippy
rustup default "$version"
rustc --version
