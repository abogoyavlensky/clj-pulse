//! The clj-kondo boundary: config compatibility and the diagnostics bridge.
//!
//! Two jobs live here. The first is configuration compatibility: reading the
//! subset of `.clj-kondo/config.edn` that clj-pulse understands (only
//! `:lint-as`, the map clj-kondo uses to treat a custom macro like a known one,
//! `{defcomponent/defcomponent clojure.core/def}`). We read the project's
//! `.clj-kondo/config.edn` only - the `config/` directory, JAR-exported
//! configs, and the `~/.config` global are not consulted yet. That half returns
//! raw `(macro-fqn, target-fqn)` string pairs and makes no decision about which
//! targets are meaningful; `settings` owns the merge and the mapping to
//! `DefKind`.
//!
//! The second is the diagnostics bridge: turning clj-kondo's JSON findings into
//! LSP diagnostics. Everything that knows clj-kondo's wire format lives here, so
//! the rest of the server sees only `Vec<Diagnostic>`.

use std::path::Path;
use std::time::Duration;

use edn_format::Value;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString, Range};

use crate::edn::{get, kw};

/// A `Value::Symbol` rendered as `ns/name` (or `name` when unqualified), else
/// `None` for any non-symbol value.
fn sym_to_string(value: &Value) -> Option<String> {
    let Value::Symbol(sym) = value else {
        return None;
    };
    Some(match sym.namespace() {
        Some(ns) => format!("{}/{}", ns, sym.name()),
        None => sym.name().to_string(),
    })
}

/// Parses the top-level `:lint-as` map of a clj-kondo config, returning each
/// `(macro-fqn, target-fqn)` pair as fully-qualified symbol strings. Returns an
/// empty vec when the input is not a map, has no `:lint-as` map, or that map is
/// empty; non-symbol keys/values are skipped.
pub(crate) fn parse_lint_as(edn: &str) -> Vec<(String, String)> {
    let Ok(Value::Map(top)) = edn_format::parse_str(edn) else {
        return vec![];
    };
    let Some(Value::Map(map)) = get(&top, kw("lint-as")) else {
        return vec![];
    };
    map.iter()
        .filter_map(|(k, v)| Some((sym_to_string(k)?, sym_to_string(v)?)))
        .collect()
}

/// Reads `root/.clj-kondo/config.edn` and returns its `:lint-as` pairs. Missing
/// or unparseable files yield an empty vec - clj-kondo config is optional.
pub(crate) fn lint_as(root: &Path) -> Vec<(String, String)> {
    let path = root.join(".clj-kondo").join("config.edn");
    match std::fs::read_to_string(&path) {
        Ok(src) => parse_lint_as(&src),
        Err(_) => vec![],
    }
}

/// The `source` every diagnostic bridged from clj-kondo carries, so editors
/// (and our own ownership merge) can tell them from clj-pulse's native lints.
pub const SOURCE: &str = "clj-kondo";

/// Parses clj-kondo's `{:output {:format :json}}` stdout into LSP diagnostics.
///
/// `None` means "this is not clj-kondo output" — an empty buffer, a Java stack
/// trace, JSON without a `findings` array. Callers treat that as a failed run
/// and keep the native diagnostics. `Some(vec![])` is a *successful* clean
/// lint, and does cede ownership.
///
/// Individual malformed findings are skipped rather than failing the batch: a
/// future clj-kondo key we mis-read must not blank out the findings beside it.
pub fn parse_findings(json: &str) -> Option<Vec<Diagnostic>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let findings = value.get("findings")?.as_array()?;
    Some(findings.iter().filter_map(finding_to_diagnostic).collect())
}

/// One JSON finding as an LSP diagnostic, or `None` when it lacks the fields a
/// squiggle needs (`type`, `message`, `row`, `col`).
fn finding_to_diagnostic(finding: &serde_json::Value) -> Option<Diagnostic> {
    let code = finding.get("type")?.as_str()?;
    let message = finding.get("message")?.as_str()?;
    let row = finding.get("row")?.as_u64()?;
    let col = finding.get("col")?.as_u64()?;

    Some(Diagnostic {
        range: finding_range(finding, row, col),
        severity: Some(severity(finding.get("level").and_then(|l| l.as_str()))),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some(SOURCE.to_string()),
        message: message.to_string(),
        tags: tags(code),
        ..Default::default()
    })
}

/// clj-kondo positions are 1-based and its `end-col` is exclusive, so both
/// ends simply lose one. `end-row`/`end-col` are optional (bracket-mismatch
/// `syntax` findings omit them); without them the squiggle covers one
/// character, which in 0-based terms is `col - 1 .. col`.
fn finding_range(finding: &serde_json::Value, row: u64, col: u64) -> Range {
    let start = position(row, col);
    let end = match (
        finding.get("end-row").and_then(|v| v.as_u64()),
        finding.get("end-col").and_then(|v| v.as_u64()),
    ) {
        (Some(end_row), Some(end_col)) => position(end_row, end_col),
        _ => tower_lsp::lsp_types::Position::new(start.line, start.character + 1),
    };
    Range { start, end }
}

/// A 1-based (row, col) pair as a 0-based LSP position, saturating so a
/// malformed `0` can never underflow.
fn position(row: u64, col: u64) -> tower_lsp::lsp_types::Position {
    tower_lsp::lsp_types::Position::new(
        row.saturating_sub(1).min(u32::MAX as u64) as u32,
        col.saturating_sub(1).min(u32::MAX as u64) as u32,
    )
}

/// clj-kondo's three levels. An unknown level is a warning — visible, but not
/// escalated into an error on our guess.
fn severity(level: Option<&str>) -> DiagnosticSeverity {
    match level {
        Some("error") => DiagnosticSeverity::ERROR,
        Some("info") => DiagnosticSeverity::INFORMATION,
        _ => DiagnosticSeverity::WARNING,
    }
}

/// Editors fade `UNNECESSARY` and strike through `DEPRECATED`. clj-kondo names
/// its dead-code linters `unused-*` uniformly, so the prefix covers every one
/// of them (including linters added after this was written).
fn tags(code: &str) -> Option<Vec<DiagnosticTag>> {
    if code.starts_with("unused-") {
        Some(vec![DiagnosticTag::UNNECESSARY])
    } else if code == "deprecated-var" {
        Some(vec![DiagnosticTag::DEPRECATED])
    } else {
        None
    }
}

/// How long one buffer lint may take before it is abandoned. Normal files
/// finish in 20-70 ms and a 4000-line file in ~0.5 s, so 2 s is slack for a
/// cold JVM-less start under load — and short enough that a wedged binary
/// never stalls the squiggles behind it.
pub const LINT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the `--version` discovery probe may take. Runs on `initialize`
/// and on every config change, so it must fail fast.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The `--config` argument turning clj-kondo's output into the JSON
/// [`parse_findings`] reads. Passed as one argv entry — we exec the binary
/// directly, so it needs no shell quoting.
const JSON_OUTPUT_CONFIG: &str = "{:output {:format :json}}";

/// Lints `source` as if it were the contents of `abs_path`, returning the
/// findings as LSP diagnostics.
///
/// The buffer goes in on stdin, so the *unsaved* text is what gets linted;
/// `--filename` still carries the real absolute path, which is what clj-kondo
/// resolves the owning `.clj-kondo` config/cache dir from (walking up from the
/// file, not from cwd) and what `namespace-name-mismatch` keys on.
///
/// `Err` is any reason we have no findings to trust — spawn failure, timeout,
/// a crash, unparseable stdout. Callers keep their native diagnostics on
/// `Err`; only `Ok` cedes ownership.
pub async fn lint(
    bin: &str,
    source: &str,
    abs_path: &Path,
    timeout: Duration,
) -> Result<Vec<Diagnostic>, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--lint")
        .arg("-")
        .arg("--filename")
        .arg(abs_path)
        .arg("--config")
        .arg(JSON_OUTPUT_CONFIG);
    // clj-kondo derives the dialect from the extension, and `.bb` is not one
    // it knows — without this every babashka script lints as an unknown lang.
    if abs_path.extension().is_some_and(|e| e == "bb") {
        cmd.arg("--lang").arg("clj");
    }

    let output = run(&mut cmd, bin, Some(source), timeout).await?;

    // Exit 0 (clean), 2 (warnings) and 3 (errors) are all successful runs;
    // only 1 is a crash. Rather than enumerate codes, trust the output: a
    // crash writes a stack trace to stderr and nothing parseable to stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_findings(&stdout).ok_or_else(|| {
        format!(
            "`{bin}` produced unparseable output ({}): {}",
            output.status,
            stderr_snippet(&output)
        )
    })
}

/// The head of a failed run's stderr, for a log line the user can act on.
fn stderr_snippet(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .trim()
        .chars()
        .take(500)
        .collect()
}

/// Populates clj-kondo's cache from a resolved classpath without producing
/// findings, so the cross-file linters (`invalid-arity`, `unresolved-var`)
/// have library signatures the first time a buffer is linted.
///
/// `cwd` is the project dir, which is where clj-kondo writes `.clj-kondo/.cache`
/// from. Callers gate on that dir existing — clj-kondo never creates it.
pub async fn warm(
    bin: &str,
    classpath: &str,
    project_dir: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--lint")
        .arg(classpath)
        .arg("--dependencies")
        .arg("--parallel")
        .current_dir(project_dir);
    let output = run(&mut cmd, bin, None, timeout).await?;
    // Unlike a buffer lint, `--dependencies` reports no findings — so here a
    // non-zero exit really is a failure (an unreadable JAR, a bad config) and
    // must not be reported as a warmed cache.
    if !output.status.success() {
        return Err(format!(
            "`{bin} --dependencies` failed ({}): {}",
            output.status,
            stderr_snippet(&output)
        ));
    }
    Ok(())
}

/// Runs `<bin> --version` and returns the version it reports.
///
/// `None` covers every "no usable clj-kondo here" case: the binary is absent
/// (bare names are resolved through PATH by `Command`, so this doubles as
/// discovery), it fails, it times out, or it is some *other* tool whose
/// `--version` we would otherwise happily accept.
pub async fn probe_version(bin: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--version");
    let output = run(&mut cmd, bin, None, PROBE_TIMEOUT).await.ok()?;
    // A wrapper script that prints a version banner and then fails is not a
    // clj-kondo we can lint with; treat it as absent rather than spawn it once
    // per keystroke.
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("clj-kondo "))
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty())
}

/// Spawns `cmd`, optionally feeding `stdin_data` to it, and collects its
/// output under `timeout`.
///
/// Mirrors `classpath::resolve_via_cmd`'s process handling — own process
/// group, `kill_on_drop`, group kill on timeout — because the failure it
/// prevents is the same: a dropped future must not orphan a child. The
/// difference is the stdin feed, and that a non-zero exit is not by itself an
/// error here (clj-kondo exits 2/3 with perfectly good findings), so the
/// status is handed back for the caller to judge.
async fn run(
    cmd: &mut tokio::process::Command,
    bin: &str,
    stdin_data: Option<&str>,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(match stdin_data {
        Some(_) => std::process::Stdio::piped(),
        None => std::process::Stdio::null(),
    })
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run `{bin}`: {e}"))?;
    let pid = child.id();

    // Feed stdin from its own task: writing inline would deadlock on a buffer
    // large enough to fill the pipe before we start draining stdout. Dropping
    // the handle at the end is what signals EOF, so clj-kondo starts linting.
    if let Some(data) = stdin_data {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("`{bin}` stdin unavailable"))?;
        let data = data.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            // A child that exits early (crash) breaks the pipe mid-write;
            // that shows up as a bad exit status, not as an error here.
            let _ = stdin.write_all(data.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| format!("failed to run `{bin}`: {e}")),
        Err(_elapsed) => {
            kill_group(pid);
            Err(format!("`{bin}` timed out after {timeout:?}"))
        }
    }
}

/// Kills a timed-out child and everything it spawned. `kill_on_drop` reaps
/// only the direct child; the group kill is what stops its descendants.
fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL)
    };
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    /// Generous enough that a loaded CI box never flakes.
    #[allow(dead_code)]
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    fn has(pairs: &[(String, String)], macro_fqn: &str, target: &str) -> bool {
        pairs.iter().any(|(m, t)| m == macro_fqn && t == target)
    }

    #[test]
    fn parses_lint_as_pairs() {
        let edn = r#"{:linters {:foo {:level :off}}
                      :lint-as {defcomponent/defcomponent clojure.core/def
                                plumbing.core/for-map clojure.core/for}}"#;
        let pairs = parse_lint_as(edn);
        assert_eq!(pairs.len(), 2);
        assert!(has(&pairs, "defcomponent/defcomponent", "clojure.core/def"));
        assert!(has(&pairs, "plumbing.core/for-map", "clojure.core/for"));
    }

    #[test]
    fn missing_lint_as_yields_empty() {
        assert!(parse_lint_as(r#"{:linters {:foo {:level :off}}}"#).is_empty());
    }

    #[test]
    fn non_map_input_yields_empty() {
        assert!(parse_lint_as("123").is_empty());
        assert!(parse_lint_as("not edn (((").is_empty());
    }

    #[test]
    fn lint_as_value_not_a_map_yields_empty() {
        assert!(parse_lint_as(r#"{:lint-as :nope}"#).is_empty());
    }

    #[test]
    fn unqualified_symbols_are_kept_as_bare_names() {
        // Rare, but a bare symbol key/value should round-trip without a slash.
        let pairs = parse_lint_as(r#"{:lint-as {defthing def}}"#);
        assert!(has(&pairs, "defthing", "def"));
    }

    // --- JSON findings -> LSP diagnostics -------------------------------

    fn finding_diags(json: &str) -> Vec<Diagnostic> {
        parse_findings(json).expect("well-formed kondo output should parse")
    }

    #[test]
    fn parses_a_finding_into_a_diagnostic() {
        // The exact shape clj-kondo emits for an unresolved namespace:
        // `other/thing` starting at col 6, end-col 17 (1-based, exclusive).
        let json = r#"{"findings":[{"type":"unresolved-namespace","level":"warning",
                       "filename":"/p/src/a.clj","row":3,"col":6,"end-row":3,"end-col":17,
                       "langs":[],"message":"Unresolved namespace other."}],
                       "summary":{"error":0,"warning":1}}"#;
        let diags = finding_diags(json);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.range.start, Position::new(2, 5));
        assert_eq!(d.range.end, Position::new(2, 16));
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            d.code,
            Some(NumberOrString::String("unresolved-namespace".to_string()))
        );
        assert_eq!(d.source.as_deref(), Some("clj-kondo"));
        assert_eq!(d.message, "Unresolved namespace other.");
        assert_eq!(d.tags, None);
    }

    #[test]
    fn maps_every_level_to_a_severity() {
        let json = r#"{"findings":[
            {"type":"a","level":"error","row":1,"col":1,"message":"e"},
            {"type":"b","level":"warning","row":1,"col":1,"message":"w"},
            {"type":"c","level":"info","row":1,"col":1,"message":"i"}]}"#;
        let sev: Vec<_> = finding_diags(json).iter().map(|d| d.severity).collect();
        assert_eq!(
            sev,
            vec![
                Some(DiagnosticSeverity::ERROR),
                Some(DiagnosticSeverity::WARNING),
                Some(DiagnosticSeverity::INFORMATION),
            ]
        );
    }

    #[test]
    fn missing_end_position_yields_a_one_char_range() {
        // Bracket-mismatch `syntax` findings carry no end-row/end-col.
        let json = r#"{"findings":[{"type":"syntax","level":"error","row":11,"col":5,
                       "message":"Mismatched bracket"}]}"#;
        let diags = finding_diags(json);
        assert_eq!(diags[0].range.start, Position::new(10, 4));
        assert_eq!(diags[0].range.end, Position::new(10, 5));
    }

    #[test]
    fn unused_types_are_tagged_unnecessary() {
        let json = r#"{"findings":[
            {"type":"unused-namespace","level":"warning","row":2,"col":3,"message":"u"},
            {"type":"unused-binding","level":"warning","row":4,"col":1,"message":"u"}]}"#;
        for d in finding_diags(json) {
            assert_eq!(
                d.tags,
                Some(vec![DiagnosticTag::UNNECESSARY]),
                "{:?}",
                d.code
            );
        }
    }

    #[test]
    fn deprecated_var_is_tagged_deprecated() {
        let json = r#"{"findings":[{"type":"deprecated-var","level":"warning","row":2,
                       "col":3,"message":"d"}]}"#;
        assert_eq!(
            finding_diags(json)[0].tags,
            Some(vec![DiagnosticTag::DEPRECATED])
        );
    }

    #[test]
    fn empty_findings_parse_to_an_empty_diagnostic_set() {
        // A clean file is success with zero diagnostics — distinct from a
        // failed run, which must leave the native set alone.
        assert_eq!(
            parse_findings(r#"{"findings":[],"summary":{}}"#),
            Some(vec![])
        );
    }

    #[test]
    fn unparseable_output_yields_none() {
        assert_eq!(parse_findings(""), None);
        assert_eq!(parse_findings("Exception in thread \"main\""), None);
        assert_eq!(parse_findings("[1,2,3]"), None);
        assert_eq!(parse_findings(r#"{"summary":{}}"#), None);
    }

    #[test]
    fn malformed_findings_are_skipped_not_fatal() {
        // One good finding among junk still reaches the editor.
        let json = r#"{"findings":[
            {"level":"error","row":1,"col":1,"message":"no type"},
            {"type":"ok","level":"error","row":7,"col":2,"message":"good"}]}"#;
        let diags = finding_diags(json);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "good");
    }

    // --- subprocess runner + version probe -------------------------------

    /// Writes an executable stand-in for the `clj-kondo` binary and returns
    /// its path. `lint`/`probe_version` exec the binary directly (no shell),
    /// so the fake needs a shebang and the exec bit.
    #[cfg(unix)]
    fn fake_bin(dir: &Path, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("clj-kondo");
        std::fs::write(&p, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lint_treats_findings_exit_code_as_success() {
        // Exit 3 means "errors found", not "the run failed".
        let dir = tempfile::TempDir::new().unwrap();
        let bin = fake_bin(
            dir.path(),
            r#"cat > /dev/null
echo '{"findings":[{"type":"invalid-arity","level":"error","row":3,"col":12,"end-row":3,"end-col":19,"message":"a/f is called with 2 args but expects 1"}]}'
exit 3
"#,
        );
        let diags = lint(&bin, "(ns a)", Path::new("/p/src/a.clj"), TEST_TIMEOUT)
            .await
            .expect("exit 3 must be success");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "a/f is called with 2 args but expects 1");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lint_feeds_the_buffer_on_stdin_with_the_real_filename() {
        // The live buffer — not the file on disk — must reach clj-kondo, and
        // `--filename` must carry the real path (namespace-name-mismatch and
        // `.clj-kondo` dir resolution both key on it).
        let dir = tempfile::TempDir::new().unwrap();
        let seen = dir.path().join("seen.txt");
        let bin = fake_bin(
            dir.path(),
            &format!(
                "cat > '{}'\necho \"$@\" >> '{}'\necho '{{\"findings\":[]}}'\n",
                seen.display(),
                seen.display()
            ),
        );
        lint(
            &bin,
            "(ns live.buffer)",
            Path::new("/p/src/a.clj"),
            TEST_TIMEOUT,
        )
        .await
        .unwrap();
        let recorded = std::fs::read_to_string(&seen).unwrap();
        assert!(recorded.contains("(ns live.buffer)"), "stdin: {recorded}");
        assert!(
            recorded.contains("--filename /p/src/a.clj"),
            "argv: {recorded}"
        );
        assert!(recorded.contains(":format :json"), "argv: {recorded}");
        assert!(
            !recorded.contains("--lang"),
            "only .bb needs --lang: {recorded}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lint_passes_lang_clj_for_babashka_files() {
        // clj-kondo cannot derive the dialect from a `.bb` extension.
        let dir = tempfile::TempDir::new().unwrap();
        let seen = dir.path().join("argv.txt");
        let bin = fake_bin(
            dir.path(),
            &format!(
                "cat > /dev/null\necho \"$@\" > '{}'\necho '{{\"findings\":[]}}'\n",
                seen.display()
            ),
        );
        lint(&bin, "(println 1)", Path::new("/p/script.bb"), TEST_TIMEOUT)
            .await
            .unwrap();
        assert!(std::fs::read_to_string(&seen)
            .unwrap()
            .contains("--lang clj"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lint_errors_when_the_binary_crashes() {
        // Exit 1 with a stack trace on stderr: a crash, not a lint result.
        let dir = tempfile::TempDir::new().unwrap();
        let bin = fake_bin(
            dir.path(),
            "cat > /dev/null\necho 'Exception in thread \"main\"' >&2\nexit 1\n",
        );
        let err = lint(&bin, "(ns a)", Path::new("/p/src/a.clj"), TEST_TIMEOUT)
            .await
            .expect_err("exit 1 must be an error");
        assert!(err.contains("Exception in thread"), "err: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lint_errors_on_unparseable_output() {
        // A success exit code with garbage on stdout must not blank the
        // native diagnostics — it degrades to "kondo failed".
        let dir = tempfile::TempDir::new().unwrap();
        let bin = fake_bin(dir.path(), "cat > /dev/null\nexit 0\n");
        let err = lint(&bin, "(ns a)", Path::new("/p/src/a.clj"), TEST_TIMEOUT)
            .await
            .expect_err("empty stdout must be an error");
        assert!(
            err.contains("unparseable") || err.contains("output"),
            "err: {err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lint_errors_when_the_binary_is_missing() {
        let err = lint(
            "clj-kondo-definitely-not-installed",
            "(ns a)",
            Path::new("/p/src/a.clj"),
            TEST_TIMEOUT,
        )
        .await
        .expect_err("a missing binary must be an error, not a panic");
        assert!(
            err.contains("clj-kondo-definitely-not-installed"),
            "err: {err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lint_kills_the_child_on_timeout() {
        // A hung clj-kondo must not outlive the lint pass — one orphan per
        // keystroke would be ~54 MB of RSS each.
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("survived");
        let bin = fake_bin(
            dir.path(),
            &format!("cat > /dev/null\nsleep 1\ntouch '{}'\n", marker.display()),
        );
        let err = lint(
            &bin,
            "(ns a)",
            Path::new("/p/src/a.clj"),
            Duration::from_millis(200),
        )
        .await
        .expect_err("timeout must be an error");
        assert!(err.contains("timed out"), "err: {err}");

        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "clj-kondo kept running after the timeout — group kill missing?"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_version_reads_the_version_line() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = fake_bin(dir.path(), "echo 'clj-kondo v2026.08.04'\n");
        assert_eq!(probe_version(&bin).await, Some("v2026.08.04".to_string()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_version_rejects_a_binary_that_is_not_clj_kondo() {
        // A misconfigured `:path` pointing at some other tool must read as
        // "not found", not as a working clj-kondo.
        let dir = tempfile::TempDir::new().unwrap();
        let bin = fake_bin(dir.path(), "echo 'GNU bash, version 5.2'\n");
        assert_eq!(probe_version(&bin).await, None);
    }

    #[tokio::test]
    async fn probe_version_of_a_missing_binary_is_none() {
        assert_eq!(
            probe_version("clj-kondo-definitely-not-installed").await,
            None
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn warm_reports_a_failed_dependency_scan() {
        // `--dependencies` emits no findings, so unlike a buffer lint a
        // non-zero exit here really is a failure — reporting success would
        // leave callers believing the cache is populated.
        let dir = tempfile::TempDir::new().unwrap();
        let bin = fake_bin(dir.path(), "echo 'could not read jar' >&2\nexit 1\n");
        let err = warm(&bin, "/a.jar:/b.jar", dir.path(), TEST_TIMEOUT)
            .await
            .expect_err("a failed dependency scan must be an error");
        assert!(err.contains("could not read jar"), "err: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn warm_passes_the_classpath_and_dependency_flags() {
        let dir = tempfile::TempDir::new().unwrap();
        let seen = dir.path().join("argv.txt");
        let bin = fake_bin(
            dir.path(),
            &format!(
                "echo \"$@\" > '{}'\npwd >> '{}'\n",
                seen.display(),
                seen.display()
            ),
        );
        warm(&bin, "/a.jar:/b.jar", dir.path(), TEST_TIMEOUT)
            .await
            .unwrap();
        let argv = std::fs::read_to_string(&seen).unwrap();
        assert!(argv.contains("--lint /a.jar:/b.jar"), "argv: {argv}");
        assert!(argv.contains("--dependencies"), "argv: {argv}");
        assert!(argv.contains("--parallel"), "argv: {argv}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_version_rejects_a_binary_that_prints_a_version_then_fails() {
        // A broken wrapper must read as "not found", not as a usable install.
        let dir = tempfile::TempDir::new().unwrap();
        let bin = fake_bin(dir.path(), "echo 'clj-kondo v2026.08.04'\nexit 1\n");
        assert_eq!(probe_version(&bin).await, None);
    }
}
