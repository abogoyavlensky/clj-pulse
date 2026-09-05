//! A bench against a large real Clojure project, so index time, memory, and
//! per-edit latency are measured rather than guessed.
//!
//! Ignored by default and skipped when `CLJ_PULSE_BENCH_ROOT` is unset. Drive
//! it with `bb bench`, which clones the corpus and passes `--nocapture`.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::LspClient;

/// The hang ceiling: indexing that takes longer than this is a bug, not a slow
/// machine. It is deliberately far above any number worth reporting.
const INDEX_CEILING: Duration = Duration::from_secs(120);
/// Per-request waits. Generous: the point is to measure, not to fail.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Samples per latency metric.
const SAMPLES: usize = 20;

#[test]
#[ignore = "needs CLJ_PULSE_BENCH_ROOT pointing at a large Clojure checkout; run with `bb bench`"]
fn bench_large_project() {
    let Some(root) = std::env::var_os("CLJ_PULSE_BENCH_ROOT") else {
        println!("CLJ_PULSE_BENCH_ROOT is unset — skipping. Run `bb bench`.");
        return;
    };
    let root = PathBuf::from(root)
        .canonicalize()
        .expect("CLJ_PULSE_BENCH_ROOT does not exist");

    let mut report = Report::new(&root);

    // Production settings, unlike every other test in the suite: stage-3
    // classpath resolution runs and clj-kondo is used when installed, because
    // that is what a user's machine does.
    let mut client = LspClient::start_production(&root);
    let pid = client.child.id();
    let started = Instant::now();
    client.initialize_no_wait(&root);

    // Stage 1: the project's own sources.
    match client.log_line_within(&["Indexed"], INDEX_CEILING) {
        Some(line) => {
            report.project_index_wall = Some(started.elapsed());
            report.project_index_reported = reported_elapsed(&line);
            let (symbols, namespaces) = indexed_counts(&line);
            report.symbols = symbols;
            report.namespaces = namespaces;
            report.rss_after_project = rss_kib(pid);
        }
        None => panic!(
            "project indexing did not finish within {:?} — the hang ceiling",
            INDEX_CEILING
        ),
    }

    // Stage 2/3: libraries. Stage 2 reads whatever `.cpcache` is already on
    // disk and logs `library indexing complete` — which on a warm checkout
    // arrives long before stage 3 has run `clojure -Spath` and re-indexed. So
    // wait for a line that means stage 3 is *settled*, and only fall back to
    // the stage-2 line when stage 3 never reports. Sampling before that would
    // mix a background reindex into every latency number below.
    let library_deadline = INDEX_CEILING.saturating_sub(started.elapsed());
    let settled = client.log_line_within(
        &[
            "full classpath indexed",
            "classpath resolution failed",
            "no classpath found",
        ],
        library_deadline,
    );
    let reached = match settled {
        Some(line) => Some(line),
        // Already stashed when stage 3 is disabled or absent, so this returns
        // at once rather than spending the rest of the budget.
        None => client.log_line_within(&["library indexing complete"], Duration::from_secs(1)),
    };
    if let Some(line) = reached {
        report.library_index_wall = Some(started.elapsed());
        report.rss_after_libraries = rss_kib(pid);
        report.library_stage = Some(line);
    }

    // The remaining metrics all run against the largest source file, the worst
    // realistic case for per-edit work.
    let Some(target) = largest_clj_file(&root) else {
        report.print();
        panic!("no .clj file under {}", root.display());
    };
    report.target = Some(target.clone());
    report.target_bytes = std::fs::metadata(&target).map(|m| m.len()).ok();

    let text = std::fs::read_to_string(&target).expect("bench target is not UTF-8");
    let suffix = file_name(&target);

    client.clear_notifications();
    let opened = Instant::now();
    client.did_open(&target);
    if wait_for_diagnostics(&mut client, &suffix, REQUEST_TIMEOUT) {
        report.first_diagnostics = Some(opened.elapsed());
    }

    // Edits land at the very end of the buffer, where they cannot corrupt a
    // form the parser is mid-way through.
    let end_line = text.lines().count() as u32;
    let mut edit_latencies = Vec::new();
    for i in 0..SAMPLES {
        client.clear_notifications();
        let sent = Instant::now();
        client.did_change_range(&target, (i + 2) as i64, (end_line, 0), (end_line, 0), "\n");
        if wait_for_diagnostics(&mut client, &suffix, REQUEST_TIMEOUT) {
            edit_latencies.push(sent.elapsed());
        }
    }
    report.edit_to_diagnostics = median(&mut edit_latencies);

    // Definition on a qualified name whose alias the file's own ns form
    // declares — the common navigation, and the one that touches the index.
    if let Some((line, character)) = first_aliased_usage(&text) {
        let mut latencies = Vec::new();
        for _ in 0..SAMPLES {
            let sent = Instant::now();
            let _ = client.goto_definition(&target, line, character);
            latencies.push(sent.elapsed());
        }
        report.definition = median(&mut latencies);
    }

    report.print();
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

struct Report {
    root: PathBuf,
    project_index_wall: Option<Duration>,
    project_index_reported: Option<String>,
    library_index_wall: Option<Duration>,
    /// The log line that ended the library wait, so a warm run (stage 3
    /// re-resolved) and a degraded one (stage 3 failed) are told apart.
    library_stage: Option<String>,
    symbols: Option<u64>,
    namespaces: Option<u64>,
    rss_after_project: Option<u64>,
    rss_after_libraries: Option<u64>,
    target: Option<PathBuf>,
    target_bytes: Option<u64>,
    first_diagnostics: Option<Duration>,
    edit_to_diagnostics: Option<Duration>,
    definition: Option<Duration>,
}

impl Report {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            project_index_wall: None,
            project_index_reported: None,
            library_index_wall: None,
            library_stage: None,
            symbols: None,
            namespaces: None,
            rss_after_project: None,
            rss_after_libraries: None,
            target: None,
            target_bytes: None,
            first_diagnostics: None,
            edit_to_diagnostics: None,
            definition: None,
        }
    }

    fn print(&self) {
        let kondo = if std::env::var_os("CLJ_PULSE_DISABLE_KONDO").is_some() {
            "disabled by environment"
        } else if which("clj-kondo") {
            "on (clj-kondo found on PATH)"
        } else {
            "off (no clj-kondo on PATH)"
        };
        let classpath = if std::env::var_os("CLJ_PULSE_DISABLE_CLASSPATH_CLI").is_some() {
            "disabled by environment"
        } else if which("clojure") {
            "on (clojure CLI found on PATH)"
        } else {
            "off (no clojure CLI on PATH)"
        };

        println!();
        println!("clj-pulse bench");
        println!("  root            {}", self.root.display());
        println!("  binary          {}", env!("CARGO_BIN_EXE_clj-pulse"));
        println!("  kondo           {}", kondo);
        println!("  classpath CLI   {}", classpath);
        if let Some(target) = &self.target {
            let size = self
                .target_bytes
                .map(|b| format!("{} KiB", b / 1024))
                .unwrap_or_else(|| "?".into());
            println!(
                "  edit target     {} ({})",
                target.strip_prefix(&self.root).unwrap_or(target).display(),
                size
            );
        }
        println!();
        row("metric", "value");
        println!("  {:-<34} {:-<24}", "", "");
        row("time to project index", &ms(self.project_index_wall));
        row(
            "  as the server reported it",
            self.project_index_reported.as_deref().unwrap_or("n/a"),
        );
        row("time to library index", &ms(self.library_index_wall));
        row(
            "  ended by",
            self.library_stage
                .as_deref()
                .map(|l| l.trim_start_matches("clj-pulse: "))
                .unwrap_or("n/a"),
        );
        row("symbols indexed", &count(self.symbols));
        row("namespaces indexed", &count(self.namespaces));
        row("RSS after project index", &mib(self.rss_after_project));
        row("RSS after library index", &mib(self.rss_after_libraries));
        row("didOpen -> first diagnostics", &ms(self.first_diagnostics));
        row(
            &format!("didChange -> diagnostics (median of {})", SAMPLES),
            &ms(self.edit_to_diagnostics),
        );
        row(
            &format!("definition (median of {})", SAMPLES),
            &ms(self.definition),
        );
        println!();
    }
}

fn row(label: &str, value: &str) {
    println!("  {:<34} {}", label, value);
}

fn ms(d: Option<Duration>) -> String {
    d.map(|d| format!("{} ms", d.as_millis()))
        .unwrap_or_else(|| "n/a".into())
}

fn count(n: Option<u64>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "n/a".into())
}

fn mib(kib: Option<u64>) -> String {
    kib.map(|k| format!("{} MiB", k / 1024))
        .unwrap_or_else(|| "n/a".into())
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// Resident set size in KiB, or `None` on a platform with neither reader.
fn rss_kib(pid: u32) -> Option<u64> {
    if cfg!(target_os = "linux") {
        let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
        return status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok());
    }
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        return String::from_utf8_lossy(&out.stdout).trim().parse().ok();
    }
    None
}

fn median(samples: &mut [Duration]) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    Some(samples[samples.len() / 2])
}

fn which(binary: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join(binary).is_file())
}

/// A `publishDiagnostics` for `suffix`, or `false` on timeout. Unlike the e2e
/// harness's waiter this never panics: a missing metric prints as `n/a`.
fn wait_for_diagnostics(client: &mut LspClient, suffix: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        let Ok(msg) = client.incoming.recv_timeout(remaining) else {
            return false;
        };
        let hit = msg["method"] == "textDocument/publishDiagnostics"
            && msg["params"]["uri"]
                .as_str()
                .is_some_and(|u| u.ends_with(suffix));
        client.stash(msg);
        if hit {
            return true;
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing the server's own log lines
// ---------------------------------------------------------------------------

/// `Indexed 1234 symbols in 56 namespaces in 1.2s` -> `(1234, 56)`.
fn indexed_counts(line: &str) -> (Option<u64>, Option<u64>) {
    let after = |keyword: &str| -> Option<u64> {
        let words: Vec<&str> = line.split_whitespace().collect();
        let at = words.iter().position(|w| *w == keyword)?;
        words.get(at.wrapping_sub(1))?.parse().ok()
    };
    (after("symbols"), after("namespaces"))
}

/// The `{:?}` duration the server itself logged, i.e. everything after the
/// final ` in `.
fn reported_elapsed(line: &str) -> Option<String> {
    line.rsplit_once(" in ")
        .map(|(_, tail)| tail.trim().to_string())
}

// ---------------------------------------------------------------------------
// Choosing what to measure against
// ---------------------------------------------------------------------------

/// The largest `.clj` file under `root`, ties broken by path so repeat runs
/// measure the same file. Skips dot-directories (`.git`, `.cpcache`).
fn largest_clj_file(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "clj") {
                let Ok(size) = entry.metadata().map(|m| m.len()) else {
                    continue;
                };
                let better = match &best {
                    None => true,
                    Some((best_size, best_path)) => {
                        size > *best_size || (size == *best_size && path < *best_path)
                    }
                };
                if better {
                    best = Some((size, path));
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into()
}

/// The (line, character) of the first `alias/name` usage whose `alias` the
/// file's own ns form declares with `:as`. `None` when the file has none, so
/// the metric prints `n/a` instead of failing the run.
fn first_aliased_usage(text: &str) -> Option<(u32, u32)> {
    let aliases = as_aliases(text);
    if aliases.is_empty() {
        return None;
    }
    for (line_no, line) in text.lines().enumerate() {
        // The ns form itself is full of `:as` pairs that are not usages.
        if line.contains(":require") || line.contains(":as ") {
            continue;
        }
        for (col, token) in tokens(line) {
            let Some((alias, name)) = token.split_once('/') else {
                continue;
            };
            if !name.is_empty() && aliases.iter().any(|a| a == alias) {
                return Some((line_no as u32, (col + alias.len() / 2) as u32));
            }
        }
    }
    None
}

/// Every `:as <alias>` name in the file's ns form.
fn as_aliases(text: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    for pair in words.windows(2) {
        if pair[0] == ":as" {
            let alias = pair[1].trim_matches(|c: char| c == ']' || c == ')' || c == '[');
            if !alias.is_empty() && !alias.starts_with(':') {
                aliases.push(alias.to_string());
            }
        }
    }
    aliases
}

/// `(column, token)` for every symbol-ish token in `line`, ignoring anything
/// after a `;` comment.
fn tokens(line: &str) -> Vec<(usize, &str)> {
    let code = line.split(';').next().unwrap_or("");
    let mut out = Vec::new();
    let mut start = None;
    let boundary = |c: char| c.is_whitespace() || "()[]{}\"'`~@^,".contains(c);
    for (i, c) in code.char_indices() {
        match (boundary(c), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                out.push((s, &code[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &code[s..]));
    }
    out
}
