# Homebrew Formula Auto-Publish Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On every clj-pulse release (tag push), regenerate `Formula/clj-pulse.rb` from the release checksums and push it to `abogoyavlensky/homebrew-tap`, so `brew install abogoyavlensky/tap/clj-pulse` always tracks the latest release.

**Tech Stack:** GitHub Actions, Bash, Homebrew formula DSL. Mirrors the existing `wtr` setup (`../wtr/scripts/generate-formula.sh` + `../wtr/.github/workflows/release.yml`).

---

## Design

### Approach

Two reusable pieces live in the **clj-pulse** repo (matching wtr's layout):

1. `scripts/generate-formula.sh VERSION CHECKSUMS_FILE` — prints a complete Homebrew formula to stdout, reading sha256 sums from a release `checksums.txt`.
2. A new `homebrew` job in `.github/workflows/release.yml` (`needs: [release]`) that downloads the published release's `checksums.txt`, runs the script, and commits + pushes the regenerated formula into the tap repo.

A docs section is added to the **homebrew-tap** repo's `README.md`. The formula file itself (`Formula/clj-pulse.rb`) is **not** seeded by hand — it is created by the first release run that executes the new job (the next release, alpha-3).

### Artifact-name mapping (the one real adaptation from wtr)

clj-pulse's release artifacts use **Rust target triples with no version in the filename**:

| Homebrew slot      | Artifact                                       |
|--------------------|------------------------------------------------|
| `on_macos on_intel`| `clj-pulse-x86_64-apple-darwin.tar.gz`         |
| `on_macos on_arm`  | `clj-pulse-aarch64-apple-darwin.tar.gz`        |
| `on_linux on_intel`| `clj-pulse-x86_64-unknown-linux-gnu.tar.gz`    |
| `on_linux on_arm`  | `clj-pulse-aarch64-unknown-linux-gnu.tar.gz`   |

The Windows `.zip` artifact is built but ignored (Homebrew has no Windows support). The version appears only in the download **URL path** (the `vX` tag), never in the filename — unlike wtr's `wtr_${version}_darwin_amd64.tar.gz`.

`checksums.txt` is produced by the release job as `sha256sum clj-pulse-* > checksums.txt`, so each line is `<sha>  clj-pulse-<triple>.tar.gz` (two-space `sha256sum` text format). The generator matches the filename in column 2.

### Formula shape

- Class `CljPulse` (Homebrew derives `clj-pulse` → `CljPulse`).
- `desc "Language server for Clojure"`, `homepage` the repo URL, `license "MIT"`.
- `livecheck do skip "Formula is updated by clj-pulse release CI" end`.
- `def install; bin.install "clj-pulse"; end` — the binary sits at the tar root.
- `test do assert_match version.to_s, shell_output("#{bin}/clj-pulse --version") end` — confirmed: `clj-pulse --version` prints `clj-pulse <version>` (`src/main.rs:24`).

### CI job

`homebrew` job, `needs: [release]`, on `ubuntu-latest`:

1. `actions/checkout@v5` — to get `scripts/generate-formula.sh`.
2. Download checksums: `gh release download "$TAG" --repo "$GITHUB_REPOSITORY" -p checksums.txt -O checksums.txt` (auth via built-in `GITHUB_TOKEN`).
3. Clone the tap with a write-capable token: `git clone https://x-access-token:${TAP_TOKEN}@github.com/abogoyavlensky/homebrew-tap.git tap`.
4. Generate `tap/Formula/clj-pulse.rb` from `version="${TAG#v}"` + `checksums.txt`.
5. Commit as `github-actions[bot]` and push — **no-op cleanly if unchanged** (`git diff --cached --quiet`).

Runs on every release, including alpha prereleases (all current tags are alphas).

### Cross-repo auth (manual prerequisite — not automatable from CI)

The job needs a **`HOMEBREW_TAP_TOKEN`** secret on the clj-pulse repo: a PAT (classic with `repo`, or fine-grained with **Contents: write** on `abogoyavlensky/homebrew-tap`). The default `GITHUB_TOKEN` cannot push to another repo. The same token used by the wtr/lgx tap automation can be reused — just add it as a secret on this repo. Documented in `docs/RELEASE.md`; the maintainer creates it.

### Testing strategy

- `generate-formula.sh` is verified locally by running it against a synthetic `checksums.txt` and asserting the output contains the four expected URLs/sha256 lines and the `CljPulse` class. (No shell test framework in this repo; verification is a scripted run with expected output.)
- The `release.yml` change cannot be fully exercised without an actual tag push; verification is a YAML parse/lint plus careful review. The real end-to-end check is the next release (alpha-3), called out in `docs/RELEASE.md`.

## File Structure

**clj-pulse repo (primary working dir):**
- Create: `scripts/generate-formula.sh` — formula generator (executable).
- Modify: `.github/workflows/release.yml` — add the `homebrew` job.
- Modify: `docs/RELEASE.md` — document the brew step + `HOMEBREW_TAP_TOKEN` prerequisite.
- Modify: `AGENTS.md` — one line in the `## Releasing` section noting brew auto-publish.

**homebrew-tap repo (`../homebrew-tap`, separate commit/push):**
- Modify: `README.md` — add a `clj-pulse` section mirroring the `wtr` section.

---

## Task 1: Formula generator script

**Files:**
- Create: `scripts/generate-formula.sh`

- [ ] **Step 1: Write `scripts/generate-formula.sh`**
  Bash script, `set -euo pipefail`, with an `err()` helper. Validate it received exactly 2 args (`VERSION CHECKSUMS_FILE`) and that the checksums file exists. Define `sha_for <triple>` that runs `awk -v f="clj-pulse-${triple}.tar.gz" '$2 == f { print $1 }' "$checksums"` and errors if empty. Resolve the four triples:
  - `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`.
  Then `cat <<EOF` the formula: leading `# This file is generated by scripts/generate-formula.sh in https://github.com/abogoyavlensky/clj-pulse. DO NOT EDIT.` comment, `class CljPulse < Formula`, `desc "Language server for Clojure"`, `homepage "https://github.com/abogoyavlensky/clj-pulse"`, `license "MIT"`, the `livecheck`/`skip` block, `on_macos`/`on_linux` blocks with `url ".../releases/download/v${version}/clj-pulse-<triple>.tar.gz"` + `sha256 "${...}"`, `def install; bin.install "clj-pulse"; end`, and the `test` block asserting `#{bin}/clj-pulse --version`. Use `../wtr/scripts/generate-formula.sh` as the template, adapting the artifact names per the design table.

- [ ] **Step 2: Make it executable**
  Run: `chmod +x scripts/generate-formula.sh`

- [ ] **Step 3: Verify against a synthetic checksums file**
  Create a temp checksums file with the four `<sha>  clj-pulse-<triple>.tar.gz` lines (plus a windows `.zip` line to confirm it is ignored), e.g.:
  Run: `printf '%s\n' 'aaaa  clj-pulse-x86_64-apple-darwin.tar.gz' 'bbbb  clj-pulse-aarch64-apple-darwin.tar.gz' 'cccc  clj-pulse-x86_64-unknown-linux-gnu.tar.gz' 'dddd  clj-pulse-aarch64-unknown-linux-gnu.tar.gz' 'eeee  clj-pulse-x86_64-pc-windows-msvc.zip' > /tmp/cs.txt && bash scripts/generate-formula.sh 0.0.1-alpha-3 /tmp/cs.txt`
  Expected: valid formula text containing `class CljPulse < Formula`, all four `download/v0.0.1-alpha-3/clj-pulse-<triple>.tar.gz` URLs, and `sha256 "aaaa"`/`"bbbb"`/`"cccc"`/`"dddd"`. No windows entry.

- [ ] **Step 4: Verify the missing-checksum error path**
  Run: `bash scripts/generate-formula.sh 9.9.9 /tmp/cs.txt; echo "exit=$?"`
  Expected: fails with `error: no checksum for clj-pulse-x86_64-apple-darwin.tar.gz ...` and non-zero exit (the synthetic file has no `9.9.9`-matching lines — note clj-pulse filenames have no version, so this confirms the file-not-found / empty-sha guards; if the generator finds the version-less names it will still succeed, so instead test the missing-file guard: `bash scripts/generate-formula.sh 0.0.1 /tmp/nope.txt; echo "exit=$?"` → `error: checksums file not found` + non-zero).

- [ ] **Step 5: Commit**
  `git commit -m "Add Homebrew formula generator script"`

## Task 2: Homebrew job in release CI

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Add the `homebrew` job**
  Append a `homebrew` job after `release`, with `needs: [release]`, `runs-on: ubuntu-latest`. Step 1 `uses: actions/checkout@v5`. Step 2 "Update Homebrew formula" with `env: { GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}, TAP_TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}, TAG: ${{ github.ref_name }} }` running a `set -euo pipefail` script that: computes `version="${TAG#v}"`; `gh release download "$TAG" --repo "$GITHUB_REPOSITORY" -p checksums.txt -O checksums.txt`; clones the tap via `https://x-access-token:${TAP_TOKEN}@github.com/abogoyavlensky/homebrew-tap.git tap`; `mkdir -p tap/Formula`; `bash scripts/generate-formula.sh "$version" checksums.txt > tap/Formula/clj-pulse.rb`; `cd tap`; sets `git config user.name "github-actions[bot]"` / `user.email "github-actions[bot]@users.noreply.github.com"`; `git add Formula/clj-pulse.rb`; `git diff --cached --quiet && { echo "formula unchanged"; exit 0; }`; `git commit -m "clj-pulse ${version}"`; `git push`. Mirror `../wtr/.github/workflows/release.yml` lines 88-113.

- [ ] **Step 2: Validate the workflow YAML parses**
  Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('ok')"`
  Expected: `ok`

- [ ] **Step 3: Sanity-check job wiring**
  Confirm the new job has `needs: [release]`, references `secrets.HOMEBREW_TAP_TOKEN`, and writes `tap/Formula/clj-pulse.rb`. (The full path is only exercised by a real tag push — see Task 5.)

- [ ] **Step 4: Commit**
  `git commit -m "Publish Homebrew formula from release CI"`

## Task 3: Document the release/brew flow

**Files:**
- Modify: `docs/RELEASE.md`
- Modify: `AGENTS.md`

- [ ] **Step 1: Update `docs/RELEASE.md`**
  Add a "Homebrew" subsection under "What the CI does": after the release is published, the `homebrew` job regenerates `Formula/clj-pulse.rb` (via `scripts/generate-formula.sh`) and pushes it to `abogoyavlensky/homebrew-tap`, so `brew install abogoyavlensky/tap/clj-pulse` and `brew upgrade clj-pulse` track releases. Add a "Prerequisites" note: the clj-pulse repo must have a `HOMEBREW_TAP_TOKEN` secret — a PAT with write (Contents) access to `homebrew-tap`; the default `GITHUB_TOKEN` cannot push cross-repo; the wtr/lgx tap token may be reused. Note the first formula appears after the next release. Use /writing-clearly.

- [ ] **Step 2: Update `AGENTS.md` Releasing section**
  Add one sentence to `## Releasing` noting that release CI also pushes an updated Homebrew formula to the tap (`brew install abogoyavlensky/tap/clj-pulse`), pointing to `docs/RELEASE.md`.

- [ ] **Step 3: Commit**
  `git commit -m "Document Homebrew publish in release docs"`

## Task 4: Tap README section

**Files:**
- Modify: `../homebrew-tap/README.md`

- [ ] **Step 1: Add a `clj-pulse` section**
  Mirror the existing `wtr` section in `../homebrew-tap/README.md`: a `## [clj-pulse](https://github.com/abogoyavlensky/clj-pulse)` heading with `### Install` (`brew install abogoyavlensky/tap/clj-pulse`), `### Upgrade` (`brew upgrade clj-pulse`), and `### How this tap is updated` pointing to `scripts/generate-formula.sh` in the clj-pulse repo (regenerated and pushed by release CI; don't edit by hand).

- [ ] **Step 2: Commit and push the tap repo**
  In `../homebrew-tap`: `git -C ../homebrew-tap add README.md && git -C ../homebrew-tap commit -m "Add clj-pulse"`. Confirm with the maintainer before pushing (separate repo).

## Task 5: Manual prerequisite & end-to-end verification

**Files:** none (operational)

- [ ] **Step 1: Maintainer creates the `HOMEBREW_TAP_TOKEN` secret**
  On the clj-pulse GitHub repo (Settings → Secrets and variables → Actions), add `HOMEBREW_TAP_TOKEN` = a PAT with write access to `homebrew-tap`. Cannot be done from CI; this is a maintainer action. Until it exists, the `homebrew` job will fail at `git push`.

- [ ] **Step 2: End-to-end check on next release**
  After cutting the next release (`bb tag` for alpha-3), confirm the `homebrew` job succeeds and that `abogoyavlensky/homebrew-tap` has a new commit creating/updating `Formula/clj-pulse.rb` with the correct version and four sha256 sums. Optionally verify `brew install abogoyavlensky/tap/clj-pulse` on macOS/Linux.
