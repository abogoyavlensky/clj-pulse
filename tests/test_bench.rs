//! A bench against pinned real Clojure projects, so index time, memory, and
//! per-edit latency are measured rather than guessed.
//!
//! Ignored by default and skipped when `CLJ_PULSE_BENCH_ROOT` is unset. Drive
//! it with `bb bench`, which checks the corpus out at its pinned commit and
//! passes `--nocapture`.
//!
//! Every metric is defined by *observable behavior* — a definition request
//! that lands where it should, a `publishDiagnostics` carrying the version of
//! the edit that caused it — and not by a log line, so the same code can
//! measure a second server that logs nothing we recognize. clj-pulse's own log
//! lines are still reported, as a cross-check on the behavioral numbers, but
//! no metric depends on one.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::sampling::{
    has_children, median, quiet_for, rss_kib, QUIET, STAGE2_LINES, STAGE3_ANNOUNCE_GRACE,
    STAGE3_LINES,
};
use common::sites::{
    answers, definition, is_source_ish, landing, project_site, source_files, third_party_sites,
    Expect, Landing, Site,
};
use common::LspClient;

/// Per-request waits. Generous: the point is to measure, not to fail.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Samples per latency metric.
const SAMPLES: usize = 20;
/// Below this many samples a median is printed with a warning rather than as a
/// clean number.
const MIN_SAMPLES: usize = 15;
/// How often a startup probe re-asks for a definition it did not get.
const POLL: Duration = Duration::from_millis(100);
/// `:kondo {:live-max-kb}`'s default, in bytes: above it clj-kondo sits out
/// the keystroke path, so a file on each side of the line is measured.
const LIVE_MAX_BYTES: u64 = 256 * 1024;
/// How many third-party dependency sites the startup probe keeps: the row is
/// carried by the first one that lands, so a namespace that turns out to be a
/// git dependency, ClojureScript-only, or a `potemkin/import-vars` re-export
/// costs nothing but a poll. Ten, because metabase's smallest files lean on
/// exactly those.
const LIBRARY_CANDIDATES: usize = 10;

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
    let corpus = std::env::var("CLJ_PULSE_BENCH_CORPUS").unwrap_or_else(|_| "(unnamed)".into());
    let clojure_lsp = std::env::var_os("CLJ_PULSE_BENCH_CLOJURE_LSP").map(PathBuf::from);

    let probes = Probes::discover(&root);
    probes.print(&root);

    if clojure_lsp.is_none() {
        println!();
        println!("CLJ_PULSE_BENCH_CLOJURE_LSP is unset — measuring clj-pulse alone.");
    }

    let runs = bench_runs();

    // Fixed order, so a cold run is always the one that follows a cleared
    // cache and a warm one always inherits what the run before it left. Cold
    // runs once; warm repeats `runs` times and reports a median row after
    // them, since the startup numbers are the ones a single run leaves noisy.
    // Only the first warm run samples latencies: the medians of 20 are stable
    // run to run, and a repeat exists for the timeline and RSS alone.
    let mut rows = Vec::new();
    for server in [Server::CljPulse, Server::ClojureLsp] {
        let binary = match server {
            Server::CljPulse => None,
            Server::ClojureLsp => match &clojure_lsp {
                Some(path) => Some(path.as_path()),
                None => continue,
            },
        };
        clear_caches(&root, server);
        let row = run(
            server,
            Temp::Cold,
            binary,
            &root,
            &corpus,
            &probes,
            RunId::Nth(1),
            true,
        );
        row.print(&probes, &root);
        row.print_json();
        rows.push(row);

        let mut warm = Vec::new();
        for n in 1..=runs {
            let row = run(
                server,
                Temp::Warm,
                binary,
                &root,
                &corpus,
                &probes,
                RunId::Nth(n),
                n == 1,
            );
            row.print(&probes, &root);
            row.print_json();
            warm.push(row);
        }
        if runs > 1 {
            let row = median_row(&warm);
            row.print_json();
            warm.push(row);
        }
        rows.extend(warm);
    }

    summary(&rows, &probes);
}

/// `CLJ_PULSE_BENCH_RUNS`: how many times each warm configuration runs.
/// Default 1, so a quick `bb bench clj-kondo` stays quick; the recorded tables
/// use 3.
fn bench_runs() -> usize {
    match std::env::var("CLJ_PULSE_BENCH_RUNS") {
        Ok(v) => v
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .unwrap_or_else(|| {
                panic!("CLJ_PULSE_BENCH_RUNS must be a positive integer, got {v:?}")
            }),
        Err(_) => 1,
    }
}

/// Which run a row is: the n-th repeat of its configuration, or the median
/// over all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunId {
    Nth(usize),
    Median { runs: usize },
}

impl RunId {
    fn label(self) -> String {
        match self {
            RunId::Nth(n) => n.to_string(),
            RunId::Median { .. } => "median".to_string(),
        }
    }
}

/// Only the probe discovery, printed: a quick check of what a corpus would
/// be asked, without starting a server. `CLJ_PULSE_BENCH_ROOT=.tmp/bench/metabase
/// cargo test --release --test test_bench bench_probes -- --ignored --nocapture`.
#[test]
#[ignore = "needs CLJ_PULSE_BENCH_ROOT; prints the probes `bb bench` would use"]
fn bench_probes() {
    let Some(root) = std::env::var_os("CLJ_PULSE_BENCH_ROOT") else {
        println!("CLJ_PULSE_BENCH_ROOT is unset — skipping.");
        return;
    };
    let root = PathBuf::from(root)
        .canonicalize()
        .expect("CLJ_PULSE_BENCH_ROOT does not exist");
    Probes::discover(&root).print(&root);
}

/// The servers under comparison. Neither is tuned: a comparison of two
/// defaults is the only fair one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Server {
    CljPulse,
    ClojureLsp,
}

impl Server {
    fn label(self) -> &'static str {
        match self {
            Server::CljPulse => "clj-pulse",
            Server::ClojureLsp => "clojure-lsp",
        }
    }

    /// The hang ceiling. clojure-lsp analyzes the whole classpath through
    /// clj-kondo before it answers anything, which on a large project is
    /// minutes rather than seconds — dropping that row would hide the number
    /// the comparison is about, so it gets a ceiling of its own.
    fn ceiling(self) -> Duration {
        match self {
            Server::CljPulse => Duration::from_secs(120),
            Server::ClojureLsp => Duration::from_secs(900),
        }
    }

    /// What a cold run deletes. `.cpcache` is *not* here: resolving the
    /// classpath is preparation both servers share, done before anything is
    /// timed. `.clj-kondo/.cache` is: both servers warm it, so a cold run of
    /// either would otherwise inherit the other's — and "clj-kondo finished"
    /// would time a warm against a cache the previous run left.
    fn caches(self, root: &Path) -> Vec<PathBuf> {
        match self {
            Server::CljPulse => vec![
                root.join(".clj-pulse").join("jar-cache"),
                root.join(".clj-kondo").join(".cache"),
            ],
            Server::ClojureLsp => vec![
                root.join(".lsp").join(".cache"),
                root.join(".clj-kondo").join(".cache"),
                clojure_lsp_xdg_cache(root),
            ],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Temp {
    Cold,
    Warm,
}

impl Temp {
    fn label(self) -> &'static str {
        match self {
            Temp::Cold => "cold",
            Temp::Warm => "warm",
        }
    }
}

/// clojure-lsp caches its JDK-source analysis *globally*, under
/// `$XDG_CACHE_HOME/clojure-lsp` (about 150 MiB), not under the project. Left
/// alone, a "cold" row would silently reuse whatever an earlier run — or the
/// maintainer's own editor — left there. The bench gives it a cache of its own
/// inside the corpus, so cold means cold and warm means what the cold run
/// wrote.
fn clojure_lsp_xdg_cache(root: &Path) -> PathBuf {
    root.join(".lsp").join("bench-xdg-cache")
}

fn clear_caches(root: &Path, server: Server) {
    for dir in server.caches(root) {
        if dir.exists() {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => println!("cold run: removed {}", dir.display()),
                Err(e) => println!("cold run: could not remove {}: {e}", dir.display()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// One run of one server
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run(
    server: Server,
    temp: Temp,
    binary: Option<&Path>,
    root: &Path,
    corpus: &str,
    probes: &Probes,
    run: RunId,
    sample: bool,
) -> Row {
    let mut row = Row::new(server, temp, corpus, run);
    let ceiling = server.ceiling();

    // Production settings, unlike every other test in the suite: stage-3
    // classpath resolution runs and clj-kondo is used when installed, because
    // that is what a user's machine does.
    let xdg = clojure_lsp_xdg_cache(root);
    let mut client = match binary {
        None => LspClient::start_production(root),
        Some(path) => LspClient::start_binary(path, root, &[("XDG_CACHE_HOME", xdg.as_path())]),
    }
    .with_request_timeout(ceiling);
    let pid = client.child.id();
    let t0 = Instant::now();
    let mut watch = StageWatch::default();
    let capabilities = client.initialize_no_wait(root);
    let sync = SyncKind::of(&capabilities);
    row.sync = sync;

    // The startup probes are open from the moment the server is, the way an
    // editor restores a session: "time to first definition" is the wait a user
    // sitting in front of that buffer actually has.
    for site in probes.startup_sites() {
        client.did_open(&site.file);
    }

    // The library probe is gated for clj-pulse: it indexes the classpath in
    // the background and a third-party namespace can land long before the
    // last entry is read, so asking starts only once the server has said
    // every entry is in. clojure-lsp answers nothing before its whole
    // analysis is done, so there is nothing to gate on.
    let gate = match server {
        Server::CljPulse => Gate::LibraryStage,
        Server::ClojureLsp => Gate::None,
    };
    let startup = poll_definitions(
        &mut client,
        probes.startup_project.as_ref(),
        &probes.startup_library,
        gate,
        t0,
        t0 + ceiling,
        &mut watch,
    );
    row.first_definition = startup.first_definition;
    row.libraries_navigable = startup.libraries_navigable;
    row.library_site = startup
        .library_site
        .map(|i| probes.startup_library[i].token.clone());
    row.library_wrong_dialect = startup.wrong_dialect;

    // Settled, not merely answering: clj-pulse's stage 3 re-resolves and
    // re-indexes long after stage 2 has answered a definition, and clojure-lsp
    // is still publishing diagnostics across the project. Sampling latency or
    // RSS through either folds background work into every number below.
    // Its own deadline, not what is left of the startup one: a probe that
    // never resolved must not also make the settle check report a failure.
    let deadline = Instant::now() + ceiling;
    let settle = match server {
        Server::CljPulse => settle_clj_pulse(&mut client, pid, t0, deadline, &mut watch),
        Server::ClojureLsp => settle_clojure_lsp(&mut client, pid, t0, deadline, &mut watch),
    };
    row.settled = settle.at;
    row.settle_note = settle.note.clone();
    row.rss_settled = rss_kib(pid);

    // clj-pulse's own account of the same startup: a cross-check on the
    // numbers above, not a number to quote on its own.
    watch.observe(&client, t0);
    row.take_stages(&watch);

    // When clj-kondo finished: for clj-pulse the end of its dependency-cache
    // warm, which `clojurePulse/lintStatus` brackets with `warming`; for
    // clojure-lsp the last publication before it went quiet, since its
    // startup *is* a clj-kondo analysis that publishes across the project as
    // it goes — the settle time itself would carry the quiet window.
    row.kondo_finished = match server {
        Server::CljPulse => watch.warming_started.and(watch.warming_finished),
        Server::ClojureLsp => settle.at.map(|_| settle.last_activity),
    };

    if !sample {
        row.lint_engine = watch.lint_engine.clone();
        return row;
    }

    // didOpen on the largest file in the corpus, the worst realistic case for
    // per-edit work, once the server is settled.
    reset(&mut client, &mut watch, t0);
    let opened = Instant::now();
    client.did_open(&probes.edit_target);
    if let Some(params) = wait_for_diagnostics(
        &mut client,
        &probes.edit_target,
        Version::Any,
        REQUEST_TIMEOUT,
    ) {
        row.first_diagnostics = Some(opened.elapsed());
        row.open_tier = tier_of(&params);
    }

    // Definition latency on that file's first alias-qualified project symbol —
    // the common navigation, and the one that touches the index.
    if let Some(site) = &probes.edit_site {
        let mut latencies = Vec::new();
        for _ in 0..SAMPLES {
            let sent = Instant::now();
            let answer = definition(&mut client, site);
            let took = sent.elapsed();
            // A wrong or empty answer is not a sample: it would time an index
            // lookup that found nothing.
            if answers(&answer, &site.expect, &site.file) {
                latencies.push(took);
            }
        }
        row.definition_samples = latencies.len();
        row.definition = median(&mut latencies);
    }

    let large = edit_to_diagnostics(&mut client, &probes.edit_target, sync, &mut watch, t0);
    row.take_edits(large, false);

    // The same on a file *under* `:live-max-kb`, where clj-kondo does run on a
    // keystroke — otherwise the bench only ever prices clj-pulse's native tier.
    if let Some(small) = &probes.small_target {
        reset(&mut client, &mut watch, t0);
        client.did_open(small);
        wait_for_diagnostics(&mut client, small, Version::Any, REQUEST_TIMEOUT);
        let small_edits = edit_to_diagnostics(&mut client, small, sync, &mut watch, t0);
        row.take_edits(small_edits, true);
    }

    row.lint_engine = watch.lint_engine.clone();
    row
}

/// How the server wants its document changes: clj-pulse takes incremental
/// ranges, clojure-lsp declares `TextDocumentSyncKind.Full` and would read the
/// range's text as the *whole* buffer. Sending each what it asked for is part
/// of measuring what its own clients pay.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SyncKind {
    Full,
    Incremental,
}

impl SyncKind {
    fn of(capabilities: &Value) -> Self {
        let sync = &capabilities["capabilities"]["textDocumentSync"];
        let kind = sync.as_u64().or_else(|| sync["change"].as_u64());
        match kind {
            Some(1) => SyncKind::Full,
            _ => SyncKind::Incremental,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SyncKind::Full => "full",
            SyncKind::Incremental => "incremental",
        }
    }
}

/// Whether the library probe waits for the server to say every classpath
/// entry is indexed before it is asked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Ask once a library stage line has arrived (clj-pulse).
    LibraryStage,
    /// Ask from the start (clojure-lsp).
    None,
}

/// What the startup probes established.
struct Startup {
    first_definition: Option<Duration>,
    /// When a definition into a third-party dependency landed, after the gate.
    libraries_navigable: Option<Duration>,
    /// Index into the candidate set of the site that carried the row.
    library_site: Option<usize>,
    /// The library answer landed in the entry's other dialect.
    wrong_dialect: bool,
}

/// Asks the startup probes for their definition until each answers correctly,
/// and returns how long each took from `t0`. Interleaved, never in sequence:
/// the library index finishes after the project one, but a probe that never
/// resolves must not lend its whole wait to the other's number. `None` for a
/// probe the ceiling passed first, or one this corpus has no site for. The
/// library candidates are asked in order and the first to land carries the
/// row; `wrong_dialect` says its answer landed in a dialect the asking file
/// does not load (`foo/bar.cljs` from a `.clj`): timed, since the lookup
/// happened, but flagged on the row.
fn poll_definitions(
    client: &mut LspClient,
    project: Option<&Site>,
    library: &[Site],
    gate: Gate,
    t0: Instant,
    deadline: Instant,
    watch: &mut StageWatch,
) -> Startup {
    let mut startup = Startup {
        first_definition: None,
        libraries_navigable: None,
        library_site: None,
        wrong_dialect: false,
    };
    loop {
        watch.observe(client, t0);
        if let Some(site) = project {
            if startup.first_definition.is_none()
                && landing(&definition(client, site), &site.expect, &site.file) != Landing::Miss
            {
                startup.first_definition = Some(t0.elapsed());
            }
        }
        let open = match gate {
            Gate::LibraryStage => watch.library_stage_first.is_some(),
            Gate::None => true,
        };
        if open && startup.libraries_navigable.is_none() {
            for (i, site) in library.iter().enumerate() {
                let landed = landing(&definition(client, site), &site.expect, &site.file);
                if landed == Landing::Miss {
                    continue;
                }
                startup.libraries_navigable = Some(t0.elapsed());
                startup.library_site = Some(i);
                startup.wrong_dialect = landed == Landing::WrongDialect;
                break;
            }
        }
        let project_pending = project.is_some() && startup.first_definition.is_none();
        let library_pending = !library.is_empty() && startup.libraries_navigable.is_none();
        if (!project_pending && !library_pending) || Instant::now() >= deadline {
            return startup;
        }
        // Wait out the poll *receiving*: with the project probe answered and
        // the gate shut, no request is in flight to pull messages off the
        // channel, and the stage line that opens the gate would sit there
        // unread. An empty method list never cuts the window short.
        quiet_for(client, &[], POLL);
    }
}

/// clj-pulse is settled when stage 3 has reported (or degraded to stage 2), no
/// child process of the server is still running, and nothing has been logged
/// for [`QUIET`]. The child check is what keeps a `clojure -Spath` that is
/// still writing `.cpcache` from being sampled as idle.
fn settle_clj_pulse(
    client: &mut LspClient,
    pid: u32,
    t0: Instant,
    deadline: Instant,
    watch: &mut StageWatch,
) -> Settle {
    let terminal: Vec<&str> = STAGE2_LINES
        .iter()
        .chain(STAGE3_LINES.iter())
        .copied()
        .collect();
    let stage = client.log_line_within(
        &terminal,
        deadline.saturating_duration_since(Instant::now()),
    );
    let mut note = match &stage {
        Some(line) => line.trim_start_matches("clj-pulse: ").to_string(),
        None => "no library stage reported".to_string(),
    };
    // Stage 2 came first; stage 3 may still be to come, and it re-indexes. It
    // announces itself before doing any work, so a short look for that line
    // decides whether waiting for its result is worth anything at all — a
    // workspace with stage 3 disabled, or without a classpath command, must
    // not pay the whole ceiling for a line that is never coming.
    if stage.as_deref().is_some_and(|l| !is_stage3(l))
        && client
            .log_line_within(&["resolving classpath via"], STAGE3_ANNOUNCE_GRACE)
            .is_some()
    {
        match client.log_line_within(
            &STAGE3_LINES,
            deadline.saturating_duration_since(Instant::now()),
        ) {
            Some(line) => note = line.trim_start_matches("clj-pulse: ").to_string(),
            None => note.push_str("; stage 3 announced but never reported"),
        }
    }

    quiesce(
        client,
        pid,
        t0,
        deadline,
        watch,
        &["window/logMessage"],
        note,
    )
}

/// clojure-lsp is settled when its analysis progress has ended (when it
/// reports any) and neither a `publishDiagnostics` nor a `$/progress` has
/// arrived for [`QUIET`] — it publishes across the whole project while it
/// analyzes, so its own diagnostics stream is the signal. The same
/// no-child-process rule applies: it shells out for the classpath too.
fn settle_clojure_lsp(
    client: &mut LspClient,
    pid: u32,
    t0: Instant,
    deadline: Instant,
    watch: &mut StageWatch,
) -> Settle {
    let mut settle = quiesce(
        client,
        pid,
        t0,
        deadline,
        watch,
        &["textDocument/publishDiagnostics", "$/progress"],
        "no publishDiagnostics or $/progress".to_string(),
    );
    let progress = if watch.progress_end {
        "; analysis progress reported end"
    } else {
        // Confirmed against the binary: with `window.workDoneProgress`
        // advertised, a warm run reports no progress at all, so the quiet
        // window is the signal that has to carry the check.
        "; no analysis progress was reported"
    };
    settle.note.push_str(progress);
    settle
}

/// The shared tail of both settle checks: wait until nothing in `methods` has
/// arrived for [`QUIET`] *and* the server has no child process still working.
fn quiesce(
    client: &mut LspClient,
    pid: u32,
    t0: Instant,
    deadline: Instant,
    watch: &mut StageWatch,
    methods: &[&str],
    note: String,
) -> Settle {
    let started = t0.elapsed();
    loop {
        reset(client, watch, t0);
        let quiet = quiet_for(client, methods, QUIET);
        // `reset` observed what the wait pulled in, so the watch holds every
        // watched message's receipt time — including one that arrived while
        // the startup probes were still polling, before this check began.
        watch.observe(client, t0);
        let last_activity = watch.last_activity(methods).unwrap_or(started);
        if quiet && !has_children(pid) {
            return Settle {
                at: Some(t0.elapsed()),
                note,
                last_activity,
            };
        }
        if Instant::now() >= deadline {
            return Settle {
                at: None,
                note: format!("{note}; never settled within the ceiling"),
                last_activity,
            };
        }
    }
}

/// What the settle check found.
struct Settle {
    /// When the server was settled, `None` if the ceiling passed first.
    at: Option<Duration>,
    note: String,
    /// The last time one of the watched methods arrived, at any point since
    /// the server started, or the start of the check when none ever did: the
    /// settle time minus the quiet window, which is when the server's startup
    /// work actually ended.
    last_activity: Duration,
}

/// One edit-to-diagnostics measurement: [`SAMPLES`] single-character inserts at
/// the end of the buffer, each timed to the publication carrying its version.
fn edit_to_diagnostics(
    client: &mut LspClient,
    path: &Path,
    sync: SyncKind,
    watch: &mut StageWatch,
    t0: Instant,
) -> Edits {
    let mut edits = Edits::new(path);
    let Ok(text) = std::fs::read_to_string(path) else {
        return edits;
    };
    // Edits land at the very end of the buffer, where they cannot corrupt a
    // form the parser is mid-way through.
    let end_line = text.lines().count() as u32;
    let mut buffer = text;
    for i in 0..SAMPLES {
        reset(client, watch, t0);
        // Versions continue past the didOpen's 1 and never repeat, so a
        // publication can always be matched to the edit that caused it.
        let version = (i + 2) as i64;
        buffer.push('\n');
        let sent = Instant::now();
        match sync {
            SyncKind::Incremental => {
                client.did_change_range(path, version, (end_line, 0), (end_line, 0), "\n")
            }
            // The same edit, spelled the way a full-sync server's own clients
            // have to spell it: the whole buffer, every keystroke.
            SyncKind::Full => client.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {
                        "uri": format!("file://{}", path.display()),
                        "version": version
                    },
                    "contentChanges": [{ "text": buffer }]
                }),
            ),
        }
        match wait_for_diagnostics(client, path, Version::Exactly(version), REQUEST_TIMEOUT) {
            Some(params) => {
                edits.latencies.push(sent.elapsed());
                edits.versioned &= params.get("version").is_some();
                if tier_of(&params) == Tier::Kondo {
                    edits.kondo = true;
                }
            }
            None => edits.missing += 1,
        }
    }
    edits
}

// ---------------------------------------------------------------------------
// Reading what a server answered
// ---------------------------------------------------------------------------

/// Which version of a document a publication has to carry to count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Version {
    /// Any publication for the document (the didOpen case).
    Any,
    /// Only the publication caused by this edit. Older ones are skipped; a
    /// server that reports no version at all is taken at its word, and the row
    /// is marked as unversioned.
    Exactly(i64),
}

/// The `publishDiagnostics` params for `path`, or `None` on timeout.
fn wait_for_diagnostics(
    client: &mut LspClient,
    path: &Path,
    want: Version,
    timeout: Duration,
) -> Option<Value> {
    let tail = path.to_string_lossy().to_string();
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let Ok(msg) = client.incoming.recv_timeout(remaining) else {
            return None;
        };
        let mut hit = None;
        if msg["method"] == "textDocument/publishDiagnostics"
            && msg["params"]["uri"]
                .as_str()
                .is_some_and(|u| u.ends_with(&tail))
        {
            let matches = match (want, msg["params"]["version"].as_i64()) {
                (Version::Any, _) => true,
                (Version::Exactly(want), Some(got)) => want == got,
                // No version reported: the server does not echo one, so this
                // is the best evidence available that the edit was linted.
                (Version::Exactly(_), None) => true,
            };
            if matches {
                hit = Some(msg["params"].clone());
            }
        }
        client.stash(msg);
        if hit.is_some() {
            return hit;
        }
    }
}

/// Which lint tier a publication came from, by the only evidence a client has:
/// whether clj-kondo owns any of the diagnostics in it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    Kondo,
    Unknown,
}

fn tier_of(params: &Value) -> Tier {
    let kondo = params["diagnostics"]
        .as_array()
        .is_some_and(|ds| ds.iter().any(|d| d["source"] == "clj-kondo"));
    if kondo {
        Tier::Kondo
    } else {
        Tier::Unknown
    }
}

/// Drops the stash, after letting the watch read what is in it: the sampling
/// loops clear before every sample, and the server's lint status and stage
/// lines would otherwise be thrown away with it.
fn reset(client: &mut LspClient, watch: &mut StageWatch, t0: Instant) {
    watch.observe(client, t0);
    client.clear_notifications();
    watch.observed = 0;
}

// ---------------------------------------------------------------------------
// The server's own log lines: a cross-check, never a metric
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StageWatch {
    /// How many stashed messages have been read: each is processed once, so a
    /// time recorded here is the message's receipt time and a later scan
    /// never re-stamps it. `reset` zeroes it with the stash.
    observed: usize,
    /// Needle -> (the line, when it was first seen).
    seen: BTreeMap<&'static str, (String, Duration)>,
    /// The earliest library stage line, stage 2 or 3, whichever came first:
    /// the gate the library probe waits for.
    library_stage_first: Option<Duration>,
    /// The engine of the last `clojurePulse/lintStatus` seen. Kept here rather
    /// than read off the stash at the end, which the sampling loops clear.
    lint_engine: Option<String>,
    /// The first `clojurePulse/lintStatus` with `warming: true`, and the last
    /// with `warming: false` after it — the clj-kondo dependency-cache warm's
    /// start and end. A `warming: false` before any `true` is the probe or a
    /// recovery reporting, not a warm ending.
    warming_started: Option<Duration>,
    warming_finished: Option<Duration>,
    /// Whether a `$/progress` has reported `end` — clojure-lsp's analysis
    /// progress, when it reports one.
    progress_end: bool,
    /// The latest receipt time of each method seen, across resets: the settle
    /// check reads its watched methods here, so a publication that arrived
    /// while the startup probes were still polling keeps its real time.
    last_by_method: BTreeMap<String, Duration>,
}

impl StageWatch {
    /// The latest receipt time among `methods`, if any arrived.
    fn last_activity(&self, methods: &[&str]) -> Option<Duration> {
        methods
            .iter()
            .filter_map(|m| self.last_by_method.get(*m))
            .max()
            .copied()
    }

    /// Records the stage lines and lint status among the messages stashed
    /// since the last call, each at the time it was received.
    fn observe(&mut self, client: &LspClient, t0: Instant) {
        let from = self.observed.min(client.notifications.len());
        for (msg, at) in client.notifications[from..]
            .iter()
            .zip(&client.received[from..])
        {
            let elapsed = at.saturating_duration_since(t0);
            if let Some(method) = msg["method"].as_str() {
                self.last_by_method.insert(method.to_string(), elapsed);
            }
            if msg["method"] == "$/progress" && msg["params"]["value"]["kind"] == "end" {
                self.progress_end = true;
                continue;
            }
            if msg["method"] == "clojurePulse/lintStatus" {
                if let Some(engine) = msg["params"]["engine"].as_str() {
                    self.lint_engine = Some(engine.to_string());
                }
                match msg["params"]["warming"].as_bool() {
                    Some(true) => {
                        self.warming_started.get_or_insert(elapsed);
                    }
                    Some(false) if self.warming_started.is_some() => {
                        self.warming_finished = Some(elapsed);
                    }
                    _ => {}
                }
                continue;
            }
            if msg["method"] != "window/logMessage" {
                continue;
            }
            let Some(text) = msg["params"]["message"].as_str() else {
                continue;
            };
            for needle in ["Indexed"].iter().chain(&STAGE2_LINES).chain(&STAGE3_LINES) {
                if text.contains(needle) && !self.seen.contains_key(needle) {
                    self.seen.insert(needle, (text.to_string(), elapsed));
                    if *needle != "Indexed" {
                        self.library_stage_first.get_or_insert(elapsed);
                    }
                }
            }
        }
        self.observed = client.notifications.len();
    }

    fn line(&self, needle: &str) -> Option<&(String, Duration)> {
        self.seen.get(needle)
    }

    /// The first library-stage line seen, stage 3 preferred over stage 2.
    fn library_stage(&self) -> Option<&(String, Duration)> {
        STAGE3_LINES
            .iter()
            .chain(&STAGE2_LINES)
            .find_map(|n| self.seen.get(n))
    }
}

fn is_stage3(line: &str) -> bool {
    STAGE3_LINES.iter().any(|n| line.contains(n))
}

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

/// Whether `binary` is on PATH — the bench reports what it found rather than
/// assuming a tool is installed.
fn which(binary: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join(binary).is_file())
}

// ---------------------------------------------------------------------------
// What the run measures against
// ---------------------------------------------------------------------------

struct Probes {
    /// The largest `.clj` in the corpus: the worst realistic case per edit.
    edit_target: PathBuf,
    edit_bytes: u64,
    /// The largest `.clj` *under* `:live-max-kb`, where clj-kondo still runs on
    /// a keystroke. `None` when the largest file is already under it — then the
    /// row above already measures the kondo tier.
    small_target: Option<PathBuf>,
    small_bytes: Option<u64>,
    /// Definition into the project, from a small file open at startup.
    startup_project: Option<Site>,
    /// Definitions into third-party dependency archives, the same way: up to
    /// [`LIBRARY_CANDIDATES`] of them, one per namespace, the first to land
    /// carrying the row.
    startup_library: Vec<Site>,
    /// Definition into the project from the largest file, for the latency
    /// median.
    edit_site: Option<Site>,
}

impl Probes {
    fn discover(root: &Path) -> Self {
        let mut files = source_files(root);
        // Deterministic: size descending, path ascending.
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        // Only files under a `src` or `test` directory are candidates for a
        // definition probe, at either end of it: a corpus of deliberately
        // broken sample files (clj-kondo's `corpus/`, metabase's fixtures) is
        // on no server's source path, so a definition into one would never
        // resolve and the probe would burn the whole ceiling.
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|(_, p)| p.clone())
            .filter(|p| is_source_ish(p, root))
            .collect();
        let (edit_bytes, edit_target) = files
            .iter()
            .find(|(_, p)| p.extension().is_some_and(|e| e == "clj"))
            .cloned()
            .unwrap_or_else(|| panic!("no .clj file under {}", root.display()));
        let small = files
            .iter()
            .find(|(size, p)| *size < LIVE_MAX_BYTES && p.extension().is_some_and(|e| e == "clj"))
            .cloned()
            .filter(|(_, p)| *p != edit_target);

        let edit_text = std::fs::read_to_string(&edit_target).unwrap_or_default();
        let edit_site = project_site(&edit_text, &edit_target, root, &paths);

        // The startup probes are opened before anything is indexed, so they are
        // taken from the *smallest* files that qualify: a didOpen of the 450 KiB
        // file at that moment would measure the harness's own choice, not the
        // server.
        let mut startup_project = None;
        let mut startup_library: Vec<Site> = Vec::new();
        for (_, path) in files.iter().rev() {
            if startup_project.is_some() && startup_library.len() >= LIBRARY_CANDIDATES {
                break;
            }
            if !is_source_ish(path, root) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            if startup_project.is_none() {
                startup_project = project_site(&text, path, root, &paths);
            }
            if startup_library.len() < LIBRARY_CANDIDATES {
                for site in third_party_sites(
                    &text,
                    path,
                    root,
                    &paths,
                    LIBRARY_CANDIDATES - startup_library.len(),
                ) {
                    let Expect::Archive(entry) = &site.expect else {
                        continue;
                    };
                    let dup = startup_library
                        .iter()
                        .any(|s| matches!(&s.expect, Expect::Archive(e) if e == entry));
                    if !dup {
                        startup_library.push(site);
                    }
                }
            }
        }

        Self {
            edit_target,
            edit_bytes,
            small_target: small.as_ref().map(|(_, p)| p.clone()),
            small_bytes: small.as_ref().map(|(size, _)| *size),
            startup_project,
            startup_library,
            edit_site,
        }
    }

    /// The files the startup probes need open, each once.
    fn startup_sites(&self) -> Vec<&Site> {
        let mut sites: Vec<&Site> = Vec::new();
        for site in self
            .startup_project
            .iter()
            .chain(self.startup_library.iter())
        {
            if !sites.iter().any(|s| s.file == site.file) {
                sites.push(site);
            }
        }
        sites
    }

    fn print(&self, root: &Path) {
        let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
        let site = |label: &str, site: &Option<Site>| match site {
            Some(s) => println!(
                "  {:<22} {}:{} `{}` -> {}",
                label,
                rel(&s.file),
                s.line + 1,
                s.token,
                s.expect.describe()
            ),
            None => println!("  {:<22} n/a", label),
        };
        println!();
        println!("bench probes");
        println!(
            "  {:<22} {} ({} KiB)",
            "edit target",
            rel(&self.edit_target),
            self.edit_bytes / 1024
        );
        match (&self.small_target, self.small_bytes) {
            (Some(p), Some(bytes)) => println!(
                "  {:<22} {} ({} KiB)",
                "under live-max-kb",
                rel(p),
                bytes / 1024
            ),
            _ => println!(
                "  {:<22} n/a (the edit target is already under {} KiB)",
                "under live-max-kb",
                LIVE_MAX_BYTES / 1024
            ),
        }
        site("first navigation", &self.startup_project);
        if self.startup_library.is_empty() {
            println!("  {:<22} n/a", "library candidates");
        }
        for (i, s) in self.startup_library.iter().enumerate() {
            println!(
                "  {:<22} {}:{} `{}` -> {}",
                if i == 0 { "library candidates" } else { "" },
                rel(&s.file),
                s.line + 1,
                s.token,
                s.expect.describe()
            );
        }
        site("definition latency", &self.edit_site);
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// One edit-to-diagnostics measurement.
struct Edits {
    latencies: Vec<Duration>,
    /// Samples whose publication never arrived within the request timeout.
    missing: usize,
    /// Whether every publication carried the version of its edit. A server that
    /// echoes no version is measured on the next publication instead, and the
    /// row says so.
    versioned: bool,
    /// Whether clj-kondo owned a diagnostic in any of them.
    kondo: bool,
    file: PathBuf,
}

impl Edits {
    fn new(file: &Path) -> Self {
        Self {
            latencies: Vec::new(),
            missing: 0,
            versioned: true,
            kondo: false,
            file: file.to_path_buf(),
        }
    }
}

struct Row {
    server: Server,
    temp: Temp,
    corpus: String,
    sync: SyncKind,
    run: RunId,
    first_definition: Option<Duration>,
    /// When a definition into a third-party dependency landed, asked only
    /// once the server said every classpath entry was indexed.
    libraries_navigable: Option<Duration>,
    /// The candidate that carried it.
    library_site: Option<String>,
    /// The library answer landed in the entry's other dialect.
    library_wrong_dialect: bool,
    /// When clj-kondo finished: the dependency-cache warm for clj-pulse, the
    /// settle for clojure-lsp.
    kondo_finished: Option<Duration>,
    settled: Option<Duration>,
    settle_note: String,
    rss_settled: Option<u64>,
    project_index_observed: Option<Duration>,
    project_index_reported: Option<String>,
    symbols: Option<u64>,
    namespaces: Option<u64>,
    library_stage_observed: Option<Duration>,
    library_stage: Option<String>,
    first_diagnostics: Option<Duration>,
    open_tier: Tier,
    definition: Option<Duration>,
    definition_samples: usize,
    edit: Option<Duration>,
    edit_samples: usize,
    edit_kondo: bool,
    edit_versioned: bool,
    edit_file: Option<PathBuf>,
    small_edit: Option<Duration>,
    small_edit_samples: usize,
    small_edit_kondo: bool,
    small_edit_file: Option<PathBuf>,
    lint_engine: Option<String>,
}

impl Row {
    fn new(server: Server, temp: Temp, corpus: &str, run: RunId) -> Self {
        Self {
            server,
            temp,
            corpus: corpus.to_string(),
            sync: SyncKind::Incremental,
            run,
            first_definition: None,
            libraries_navigable: None,
            library_site: None,
            library_wrong_dialect: false,
            kondo_finished: None,
            settled: None,
            settle_note: String::new(),
            rss_settled: None,
            project_index_observed: None,
            project_index_reported: None,
            symbols: None,
            namespaces: None,
            library_stage_observed: None,
            library_stage: None,
            first_diagnostics: None,
            open_tier: Tier::Unknown,
            definition: None,
            definition_samples: 0,
            edit: None,
            edit_samples: 0,
            edit_kondo: false,
            edit_versioned: true,
            edit_file: None,
            small_edit: None,
            small_edit_samples: 0,
            small_edit_kondo: false,
            small_edit_file: None,
            lint_engine: None,
        }
    }

    fn take_stages(&mut self, watch: &StageWatch) {
        if let Some((line, at)) = watch.line("Indexed") {
            self.project_index_observed = Some(*at);
            self.project_index_reported = reported_elapsed(line);
            let (symbols, namespaces) = indexed_counts(line);
            self.symbols = symbols;
            self.namespaces = namespaces;
        }
        if let Some((line, at)) = watch.library_stage() {
            self.library_stage_observed = Some(*at);
            self.library_stage = Some(line.trim_start_matches("clj-pulse: ").to_string());
        }
    }

    fn take_edits(&mut self, mut edits: Edits, small: bool) {
        let samples = edits.latencies.len();
        let median = median(&mut edits.latencies);
        if small {
            self.small_edit = median;
            self.small_edit_samples = samples;
            self.small_edit_kondo = edits.kondo;
            self.small_edit_file = Some(edits.file);
        } else {
            self.edit = median;
            self.edit_samples = samples;
            self.edit_kondo = edits.kondo;
            self.edit_versioned = edits.versioned;
            self.edit_file = Some(edits.file);
        }
    }

    fn print(&self, probes: &Probes, root: &Path) {
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
        println!(
            "{} ({}, run {}) on {}",
            self.server.label(),
            self.temp.label(),
            self.run.label(),
            self.corpus
        );
        println!("  root            {}", root.display());
        println!("  document sync   {}", self.sync.label());
        println!("  kondo on PATH   {}", kondo);
        println!("  classpath CLI   {}", classpath);
        if self.server == Server::CljPulse {
            println!("  binary          {}", env!("CARGO_BIN_EXE_clj-pulse"));
            println!(
                "  lint engine     {}",
                self.lint_engine.as_deref().unwrap_or("not reported")
            );
        }
        println!();
        row("metric", "value");
        println!("  {:-<34} {:-<34}", "", "");
        row("time to first navigation", &ms(self.first_definition));
        row(
            "all dependencies navigable",
            &format!(
                "{}{}",
                ms(self.libraries_navigable),
                if self.library_wrong_dialect {
                    " (dialect: wrong)"
                } else {
                    ""
                }
            ),
        );
        row(
            "  landed at",
            self.library_site
                .as_deref()
                .unwrap_or("no candidate landed"),
        );
        row("clj-kondo finished", &ms(self.kondo_finished));
        row("time to settled", &ms(self.settled));
        row("  settled by", &self.settle_note);
        row("RSS settled", &mib(self.rss_settled));
        row(
            &format!("definition (median of {})", SAMPLES),
            &sampled(self.definition, self.definition_samples),
        );
        row("didOpen -> first diagnostics", &ms(self.first_diagnostics));
        row("  tier", self.tier_label(self.open_tier == Tier::Kondo));
        row(
            &format!(
                "didChange -> diagnostics ({} KiB)",
                probes.edit_bytes / 1024
            ),
            &sampled(self.edit, self.edit_samples),
        );
        row("  tier", self.tier_label(self.edit_kondo));
        if let Some(bytes) = probes.small_bytes {
            row(
                &format!("didChange -> diagnostics ({} KiB)", bytes / 1024),
                &sampled(self.small_edit, self.small_edit_samples),
            );
            row("  tier", self.tier_label(self.small_edit_kondo));
        }
        if !self.edit_versioned {
            row(
                "  note",
                "the server echoes no document version; the next publication was timed",
            );
        }
        if self.server != Server::CljPulse {
            println!();
            return;
        }
        println!();
        println!("  as the server logged it");
        row("  project index", &ms(self.project_index_observed));
        row(
            "    as it reported",
            self.project_index_reported.as_deref().unwrap_or("n/a"),
        );
        row("  symbols indexed", &count(self.symbols));
        row("  namespaces indexed", &count(self.namespaces));
        row("  library stage", &ms(self.library_stage_observed));
        row(
            "    ended by",
            self.library_stage.as_deref().unwrap_or("n/a"),
        );
        println!();
    }

    /// "with clj-kondo" only on evidence, and the only evidence a client has
    /// per pass is a diagnostic clj-kondo owns. `clojurePulse/lintStatus` says
    /// whether the engine is live at all, which is a different question: on a
    /// buffer above `:live-max-kb` it reports `kondo+native` while clj-kondo
    /// sits out every keystroke, so it can qualify a negative but never turn
    /// one into a positive.
    fn tier_label(&self, kondo_diagnostic: bool) -> &'static str {
        match (kondo_diagnostic, self.lint_engine.as_deref()) {
            (true, _) => "with clj-kondo (a diagnostic carried its source)",
            (false, Some("kondo+native")) => {
                "native only in these publications (the engine is kondo+native, so the file is \
                 above :live-max-kb or has no clj-kondo findings)"
            }
            (false, _) => "native only (clj-kondo not in use)",
        }
    }

    /// One line a later run can be diffed against.
    fn print_json(&self) {
        let ms = |d: Option<Duration>| match d {
            Some(d) => json!(d.as_millis() as u64),
            None => Value::Null,
        };
        println!(
            "BENCH_JSON {}",
            json!({
                "server": self.server.label(),
                "temperature": self.temp.label(),
                "document_sync": self.sync.label(),
                "corpus": self.corpus,
                "run": match self.run {
                    RunId::Nth(n) => json!(n),
                    RunId::Median { .. } => json!("median"),
                },
                "runs": match self.run {
                    RunId::Nth(_) => Value::Null,
                    RunId::Median { runs } => json!(runs),
                },
                "first_definition_ms": ms(self.first_definition),
                "libraries_navigable_ms": ms(self.libraries_navigable),
                "library_site": self.library_site,
                "library_wrong_dialect": self.library_wrong_dialect,
                "kondo_finished_ms": ms(self.kondo_finished),
                "settled_ms": ms(self.settled),
                "settled_by": self.settle_note,
                "rss_settled_kib": self.rss_settled,
                "definition_ms": ms(self.definition),
                "definition_samples": self.definition_samples,
                "first_diagnostics_ms": ms(self.first_diagnostics),
                "edit_ms": ms(self.edit),
                "edit_samples": self.edit_samples,
                "edit_kondo": self.edit_kondo,
                "edit_versioned": self.edit_versioned,
                "edit_file": self.edit_file.as_ref().map(|p| p.display().to_string()),
                "small_edit_ms": ms(self.small_edit),
                "small_edit_samples": self.small_edit_samples,
                "small_edit_kondo": self.small_edit_kondo,
                "small_edit_file": self.small_edit_file.as_ref().map(|p| p.display().to_string()),
                "symbols": self.symbols,
                "namespaces": self.namespaces,
                "lint_engine": self.lint_engine,
            })
        );
    }
}

/// The median over repeats of one configuration: every timing and the RSS
/// take the median of the runs that have a value, sample counts the minimum,
/// and the flags hold only when every run set them. Labels and the server's
/// own reported lines come from the first run.
fn median_row(rows: &[Row]) -> Row {
    let first = rows.first().expect("a median needs at least one run");
    let med = |pick: fn(&Row) -> Option<Duration>| {
        let mut values: Vec<Duration> = rows.iter().filter_map(pick).collect();
        median(&mut values)
    };
    let med_u64 = |pick: fn(&Row) -> Option<u64>| {
        let mut values: Vec<u64> = rows.iter().filter_map(pick).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        Some(values[values.len() / 2])
    };
    let min = |pick: fn(&Row) -> usize| rows.iter().map(pick).min().unwrap_or(0);
    let all = |pick: fn(&Row) -> bool| rows.iter().all(pick);
    // A warning flag survives if any run raised it: the median above may
    // include that run's timing.
    let any = |pick: fn(&Row) -> bool| rows.iter().any(pick);
    Row {
        server: first.server,
        temp: first.temp,
        corpus: first.corpus.clone(),
        sync: first.sync,
        run: RunId::Median { runs: rows.len() },
        first_definition: med(|r| r.first_definition),
        libraries_navigable: med(|r| r.libraries_navigable),
        library_site: first.library_site.clone(),
        library_wrong_dialect: any(|r| r.library_wrong_dialect),
        kondo_finished: med(|r| r.kondo_finished),
        settled: med(|r| r.settled),
        settle_note: first.settle_note.clone(),
        rss_settled: med_u64(|r| r.rss_settled),
        project_index_observed: med(|r| r.project_index_observed),
        project_index_reported: first.project_index_reported.clone(),
        symbols: first.symbols,
        namespaces: first.namespaces,
        library_stage_observed: med(|r| r.library_stage_observed),
        library_stage: first.library_stage.clone(),
        first_diagnostics: med(|r| r.first_diagnostics),
        open_tier: first.open_tier,
        definition: med(|r| r.definition),
        definition_samples: min(|r| r.definition_samples),
        edit: med(|r| r.edit),
        edit_samples: min(|r| r.edit_samples),
        edit_kondo: all(|r| r.edit_kondo),
        edit_versioned: all(|r| r.edit_versioned),
        edit_file: first.edit_file.clone(),
        small_edit: med(|r| r.small_edit),
        small_edit_samples: min(|r| r.small_edit_samples),
        small_edit_kondo: all(|r| r.small_edit_kondo),
        small_edit_file: first.small_edit_file.clone(),
        lint_engine: first.lint_engine.clone(),
    }
}

/// One line per configuration, in the order they ran — the shape the README
/// and `docs/MEMORY.md` tables are built from.
fn summary(rows: &[Row], probes: &Probes) {
    let large = probes.edit_bytes / 1024;
    let small = probes.small_bytes.map(|b| b / 1024);
    println!();
    println!(
        "summary (fixed order: clj-pulse cold, clj-pulse warm, clojure-lsp cold, clojure-lsp warm)"
    );
    println!(
        "  {:<12} {:<5} {:>6} {:>9} {:>9} {:>10} {:>7} {:>7} {:>11} {:>11}",
        "server",
        "temp",
        "run",
        "1st nav",
        "all libs",
        "kondo done",
        "RSS",
        "def",
        format!("edit {large}K"),
        small
            .map(|k| format!("edit {k}K"))
            .unwrap_or_else(|| "edit -".into()),
    );
    for r in rows {
        println!(
            "  {:<12} {:<5} {:>6} {:>9} {:>9} {:>10} {:>7} {:>7} {:>11} {:>11}",
            r.server.label(),
            r.temp.label(),
            r.run.label(),
            ms(r.first_definition),
            ms(r.libraries_navigable),
            ms(r.kondo_finished),
            mib(r.rss_settled),
            ms(r.definition),
            ms(r.edit),
            ms(r.small_edit),
        );
    }
    println!();
}

fn row(label: &str, value: &str) {
    println!("  {:<34} {}", label, value);
}

fn ms(d: Option<Duration>) -> String {
    d.map(|d| format!("{} ms", d.as_millis()))
        .unwrap_or_else(|| "n/a".into())
}

/// A median with its sample count, warned about when too few landed for the
/// number to stand on its own.
fn sampled(d: Option<Duration>, samples: usize) -> String {
    match d {
        None => format!("n/a (0/{} samples)", SAMPLES),
        Some(d) if samples < MIN_SAMPLES => format!(
            "{} ms (only {}/{} samples — treat as indicative)",
            d.as_millis(),
            samples,
            SAMPLES
        ),
        Some(d) if samples < SAMPLES => {
            format!("{} ms ({}/{} samples)", d.as_millis(), samples, SAMPLES)
        }
        Some(d) => format!("{} ms", d.as_millis()),
    }
}

fn count(n: Option<u64>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "n/a".into())
}

fn mib(kib: Option<u64>) -> String {
    kib.map(|k| format!("{} MiB", k / 1024))
        .unwrap_or_else(|| "n/a".into())
}

// ---------------------------------------------------------------------------
// Unit tests of the harness's own rules, run by `bb check`
// ---------------------------------------------------------------------------

/// A third-party candidate is a usage of a namespace that is neither
/// `clojure.*` nor the project's own — one per namespace, cursor in the name
/// part.
#[test]
fn third_party_sites_skip_clojure_and_project_namespaces() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let util = root.join("src").join("app").join("util.clj");
    std::fs::create_dir_all(util.parent().unwrap()).unwrap();
    std::fs::write(&util, "(ns app.util)\n(defn f [x] x)\n").unwrap();
    // A facade: it defines nothing itself, and is still the project's.
    let facade = root.join("src").join("app").join("api.clj");
    std::fs::write(
        &facade,
        "(ns app.api (:require [potemkin :as p]))\n(p/import-vars [app.util f])\n",
    )
    .unwrap();
    let paths = vec![util.clone(), facade.clone()];
    let text = "(ns app.core\n  (:require [clojure.string :as str]\n            [app.util :as util]\n            [app.api :as api]\n            [honey.sql :as sql]))\n\
                (defn g [x] (str/join \",\" [(util/f x) (api/f x) (sql/format x) (sql/format-expr x)]))\n";
    let file = root.join("src").join("app").join("core.clj");
    let sites = third_party_sites(text, &file, root, &paths, 5);
    assert_eq!(sites.len(), 1, "one site per namespace");
    let site = &sites[0];
    assert_eq!(site.token, "sql/format");
    assert!(matches!(&site.expect, Expect::Archive(e) if e == "honey/sql"));
    assert_eq!(site.line, 5);
    // Inside `format`, past the `sql/` prefix.
    let line = text.lines().nth(5).unwrap();
    let col = line.find("sql/format").unwrap();
    assert!(site.character as usize > col + 4);
    assert!((site.character as usize) < col + "sql/format".len());
}

/// The median row takes each field's median over the runs that have it, keeps
/// a warning any run raised, and says how many runs it stands for.
#[test]
fn median_row_takes_the_median_of_each_field() {
    let mut rows: Vec<Row> = [300u64, 100, 200]
        .iter()
        .enumerate()
        .map(|(i, ms)| {
            let mut r = Row::new(Server::CljPulse, Temp::Warm, "unit", RunId::Nth(i + 1));
            r.first_definition = Some(Duration::from_millis(*ms));
            r.definition_samples = 20;
            r
        })
        .collect();
    rows[0].rss_settled = Some(3);
    rows[2].rss_settled = Some(1);
    rows[1].definition_samples = 18;
    rows[1].library_wrong_dialect = true;
    let m = median_row(&rows);
    assert!(m.library_wrong_dialect, "a warning any run raised survives");
    assert_eq!(m.run, RunId::Median { runs: 3 });
    assert_eq!(m.first_definition, Some(Duration::from_millis(200)));
    assert_eq!(m.rss_settled, Some(3), "upper middle of the two present");
    assert_eq!(m.definition_samples, 18);
    assert_eq!(m.libraries_navigable, None);
}
