use std::path::{Path, PathBuf};
use std::time::Duration;

/// Parses a classpath string: entries split cross-platform, relative ones
/// resolved against `root` (they are project paths, not cwd-relative), missing
/// ones dropped. Also reports whether any *absolute* entry exists — a library
/// path, the signal `discover` uses to tell a live cache from a stale one.
fn parse_entries(root: &Path, classpath: &str) -> (Vec<PathBuf>, bool) {
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut has_lib_entry = false;
    for raw in std::env::split_paths(classpath.trim()) {
        let was_absolute = raw.is_absolute();
        let resolved = if was_absolute { raw } else { root.join(raw) };
        if resolved.exists() {
            has_lib_entry |= was_absolute;
            entries.push(resolved);
        }
    }
    (entries, has_lib_entry)
}

/// Default timeout for a stage-3 classpath command (a cold resolve may
/// download dependencies for minutes).
pub const CMD_TIMEOUT: Duration = Duration::from_secs(300);

/// Resolves a project's classpath by running a verbatim shell command
/// (`sh -c` on Unix, `cmd /C` on Windows) in `dir`.
///
/// The command's last non-empty stdout line is the classpath; relative entries
/// resolve against `dir`. For `clojure -Spath` the CLI is its own staleness
/// check: with a warm `.cpcache` it prints the cached classpath without
/// booting a JVM. Errors are human-readable reasons for the caller to log —
/// resolution failing must never take the server down.
pub async fn resolve_via_cmd(
    cmd_str: &str,
    dir: &Path,
    timeout: Duration,
) -> Result<Vec<PathBuf>, String> {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd_str);
        // The shell's children (the actual command, its JVM) must die with it
        // on timeout: run the whole tree as its own process group so the
        // timeout path can signal the group, not just the shell.
        c.process_group(0);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(cmd_str);
        c
    };
    cmd.current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // A dropped future (timeout) must not orphan a JVM mid-download. On Unix
    // the explicit group kill below is the real cleanup; kill_on_drop is the
    // fallback (and all Windows has).
    cmd.kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to run `{cmd_str}`: {e}"))?;
    let pid = child.id();

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| format!("failed to run `{cmd_str}`: {e}"))?,
        Err(_elapsed) => {
            // The child future was dropped (kill_on_drop reaps the shell);
            // also kill its descendants so the command itself dies too.
            if let Some(pid) = pid {
                // Unix: SIGKILL the process group created above.
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL)
                };
                // Windows: no process groups — taskkill the whole tree.
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/T", "/F", "/PID", &pid.to_string()])
                        .output();
                }
            }
            return Err(format!("`{cmd_str}` timed out after {timeout:?}"));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet: String = stderr.trim().chars().take(500).collect();
        return Err(format!("`{cmd_str}` failed ({}): {snippet}", output.status));
    }

    // The command may print download-progress lines; the classpath is the
    // last non-empty line of stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| format!("`{cmd_str}` produced no output"))?;

    let (entries, _) = parse_entries(dir, line);
    if entries.is_empty() {
        return Err(format!("`{cmd_str}` classpath has no existing entries"));
    }
    Ok(entries)
}

/// Discovers classpath entries from the project's `.cpcache/` directory.
///
/// Tries `.cp` files newest-first and returns the entries of the first one
/// that still resolves (at least one absolute path exists on disk). Older
/// files are a fallback for stale caches — e.g. after an `~/.m2` cleanup or
/// when caches were created on another machine. Uses `std::env::split_paths`
/// for cross-platform parsing (`:` on Unix, `;` on Windows).
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let cpcache = root.join(".cpcache");
    if !cpcache.exists() {
        return vec![];
    }

    for cp_file in cp_files_newest_first(&cpcache) {
        let content = match std::fs::read_to_string(&cp_file) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to read {}: {}", cp_file.display(), e);
                continue;
            }
        };

        // A .cp file only counts as current if at least one of its
        // *absolute* entries (a library path) still exists — relative
        // entries ("src") resolve under any project root and prove nothing.
        let (entries, has_lib_entry) = parse_entries(root, &content);

        if has_lib_entry {
            return entries;
        }
        tracing::debug!("skipping stale classpath file {}", cp_file.display());
    }

    vec![]
}

fn cp_files_newest_first(cpcache: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(cpcache) else {
        return vec![];
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "cp").unwrap_or(false))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .collect();
    files.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    files.into_iter().map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Writes a stub shell script standing in for the classpath command and
    /// returns the command string (`sh <script>`) that runs it.
    #[cfg(unix)]
    fn stub_cmd(dir: &Path, script: &str) -> String {
        let p = dir.join("classpath-stub.sh");
        fs::write(&p, script).unwrap();
        format!("sh {}", p.display())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_via_cmd_parses_last_line_and_resolves_relative_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("src")).unwrap();
        let lib = tempfile::TempDir::new().unwrap();

        // Progress noise above the classpath line must be ignored.
        let script = format!(
            "echo 'Downloading: org/foo/foo.pom'\necho 'src:{}'\n",
            lib.path().display()
        );
        let cmd = stub_cmd(root, &script);

        let entries = resolve_via_cmd(&cmd, root, std::time::Duration::from_secs(10))
            .await
            .expect("stub resolution should succeed");
        assert_eq!(entries, vec![root.join("src"), lib.path().to_path_buf()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_via_cmd_runs_through_the_shell_in_dir() {
        // A raw shell one-liner (no stub file) must work, and relative output
        // must resolve against `dir` — proving both the `sh -c` wrapping and
        // the working directory.
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("src")).unwrap();

        let entries = resolve_via_cmd(
            "echo ignored && echo src",
            root,
            std::time::Duration::from_secs(10),
        )
        .await
        .expect("shell one-liner should succeed");
        assert_eq!(entries, vec![root.join("src")]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_via_cmd_reports_stderr_on_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let cmd = stub_cmd(dir.path(), "echo 'boom: bad alias' >&2\nexit 1\n");
        let err = resolve_via_cmd(&cmd, dir.path(), std::time::Duration::from_secs(10))
            .await
            .expect_err("non-zero exit must be an error");
        assert!(err.contains("boom: bad alias"), "err: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_via_cmd_errors_on_empty_output() {
        let dir = tempfile::TempDir::new().unwrap();
        let cmd = stub_cmd(dir.path(), "exit 0\n");
        let err = resolve_via_cmd(&cmd, dir.path(), std::time::Duration::from_secs(10))
            .await
            .expect_err("empty output must be an error");
        assert!(err.contains("no output"), "err: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_via_cmd_kills_child_on_timeout() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let marker = root.join("survived");
        // If the child outlives the timeout, it leaves a marker file behind.
        let script = format!("sleep 1\ntouch '{}'\n", marker.display());
        let cmd = stub_cmd(root, &script);

        let err = resolve_via_cmd(&cmd, root, std::time::Duration::from_millis(200))
            .await
            .expect_err("timeout must be an error");
        assert!(err.contains("timed out"), "err: {err}");

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "child kept running after the timeout — kill_on_drop missing?"
        );
    }

    #[test]
    fn test_discover_no_cpcache() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = discover(dir.path());
        assert!(result.is_empty());
    }

    #[test]
    fn test_discover_returns_existing_paths_filters_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        let cpcache = root.join(".cpcache");
        fs::create_dir(&cpcache).unwrap();

        // Create a real directory to put in the classpath
        let lib_dir = root.join("lib");
        fs::create_dir(&lib_dir).unwrap();

        // Use the OS path separator for the classpath
        let cp_content = if cfg!(windows) {
            format!("{};/nonexistent/path.jar", lib_dir.display())
        } else {
            format!("{}:/nonexistent/path.jar", lib_dir.display())
        };
        fs::write(cpcache.join("abc123.cp"), &cp_content).unwrap();

        let result = discover(root);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], lib_dir);
    }

    #[test]
    fn test_discover_picks_most_recent_cp() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let cpcache = root.join(".cpcache");
        fs::create_dir(&cpcache).unwrap();

        let lib1 = root.join("lib1");
        let lib2 = root.join("lib2");
        fs::create_dir(&lib1).unwrap();
        fs::create_dir(&lib2).unwrap();

        // Write two .cp files; the second will have a later mtime
        let sep = if cfg!(windows) { ";" } else { ":" };
        fs::write(
            cpcache.join("old.cp"),
            format!("{}{}{}", lib1.display(), sep, lib2.display()),
        )
        .unwrap();
        // Brief sleep to ensure different mtime
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(cpcache.join("new.cp"), lib2.display().to_string()).unwrap();

        let result = discover(root);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], lib2);
    }

    #[test]
    fn test_discover_falls_back_when_newest_cp_is_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let cpcache = root.join(".cpcache");
        fs::create_dir(&cpcache).unwrap();

        let lib = root.join("lib");
        fs::create_dir(&lib).unwrap();

        let sep = if cfg!(windows) { ";" } else { ":" };
        // Older .cp resolves; newest one references another machine's paths
        fs::write(
            cpcache.join("old.cp"),
            format!("src{}{}", sep, lib.display()),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            cpcache.join("new.cp"),
            format!("src{}/machine/gone/lib.jar", sep),
        )
        .unwrap();

        let result = discover(root);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], lib);
    }
}
