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
#[path = "bench/dependencies.rs"]
mod dependencies;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::sampling::{
    has_children, median, quiet_for, rss_kib, QUIET, STAGE2_LINES, STAGE3_ANNOUNCE_GRACE,
    STAGE3_LINES,
};
use common::sites::{
    answers, definition, is_source_ish, library_site, project_site, source_files, Site,
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
/// Bound startup traffic so probing cannot fill a stalled server's input pipe.
const MAX_STARTUP_IN_FLIGHT: usize = 8;
/// `:kondo {:live-max-kb}`'s default, in bytes: above it clj-kondo sits out
/// the keystroke path, so a file on each side of the line is measured.
const LIVE_MAX_BYTES: u64 = 256 * 1024;

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

    // Fixed order, so a cold run is always the one that follows a cleared
    // cache and a warm one always inherits what the run before it left.
    let mut rows = Vec::new();
    for (server, temp) in [
        (Server::CljPulse, Temp::Cold),
        (Server::CljPulse, Temp::Warm),
        (Server::ClojureLsp, Temp::Cold),
        (Server::ClojureLsp, Temp::Warm),
    ] {
        let binary = match server {
            Server::CljPulse => None,
            Server::ClojureLsp => match &clojure_lsp {
                Some(path) => Some(path.as_path()),
                None => continue,
            },
        };
        if temp == Temp::Cold {
            clear_caches(&root, server);
        }
        let row = run(server, temp, binary, &root, &corpus, &probes);
        row.print(&probes, &root);
        row.print_json(&probes);
        rows.push(row);
    }

    summary(&rows, &probes);
}

/// Both servers use their defaults except classpath selection: clojure-lsp
/// must use the same deps.edn command, without merging a second bb.edn classpath.
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
    /// timed.
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

fn run(
    server: Server,
    temp: Temp,
    binary: Option<&Path>,
    root: &Path,
    corpus: &str,
    probes: &Probes,
) -> Row {
    let mut row = Row::new(server, temp, corpus);
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
    let options = match server {
        Server::CljPulse => json!({}),
        Server::ClojureLsp => json!({"project-specs": [{"project-path":"deps.edn",
            "classpath-cmd":["clojure", "-A:dev:test", "-Spath"]}]}),
    };
    let capabilities = match initialize_until(&mut client, root, options, t0 + ceiling) {
        Ok(result) => result,
        Err(reason) => {
            row.settle_note = reason.clone();
            row.dependency_outcomes = probes
                .dependencies
                .targets
                .iter()
                .map(|target| {
                    let mut outcome = dependencies::Outcome::new(target.symbol.clone());
                    outcome.last_failure = Some(reason.clone());
                    outcome
                })
                .collect();
            return row;
        }
    };
    let sync = SyncKind::of(&capabilities);
    row.sync = sync;

    // The startup probes are open from the moment the server is, the way an
    // editor restores a session: "time to first definition" is the wait a user
    // sitting in front of that buffer actually has.
    for site in probes.startup_sites() {
        client.did_open(&site.file);
    }

    let dependency_uri = tower_lsp::lsp_types::Url::from_file_path(
        root.join("src/__clj_pulse_benchmark_dependencies__.clj"),
    )
    .unwrap()
    .to_string();
    if !probes.dependencies.targets.is_empty() {
        let message = json!({"jsonrpc":"2.0", "method":"textDocument/didOpen", "params": {
            "textDocument": {"uri":dependency_uri, "languageId":"clojure", "version":1,
                "text":probes.dependencies.buffer()}}});
        if let Err(reason) = send_until(&mut client, message, t0 + ceiling) {
            row.dependency_outcomes = probes
                .dependencies
                .targets
                .iter()
                .map(|target| {
                    let mut outcome = dependencies::Outcome::new(target.symbol.clone());
                    outcome.last_failure = Some(reason.clone());
                    outcome
                })
                .collect();
            return row;
        }
    }
    let startup = poll_definitions(
        &mut client,
        probes,
        &dependency_uri,
        t0,
        t0 + ceiling,
        &mut watch,
    );
    let startup_complete = (probes.startup_project.is_none() || startup.project.is_some())
        && (probes.startup_library.is_none() || startup.library.is_some())
        && (probes.dependencies.targets.is_empty()
            || dependencies::ready_ms(&startup.dependencies).is_some());
    row.first_definition = startup.project;
    row.first_library_definition = startup.library;
    row.dependency_outcomes = startup.dependencies;
    if !startup.transport_usable {
        return row;
    }
    if !probes.dependencies.targets.is_empty() {
        client.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": dependency_uri}}),
        );
    }

    // Settled, not merely answering: clj-pulse's stage 3 re-resolves and
    // re-indexes long after stage 2 has answered a definition, and clojure-lsp
    // is still publishing diagnostics across the project. Sampling latency or
    // RSS through either folds background work into every number below.
    // Its own deadline, not what is left of the startup one: a probe that
    // never resolved must not also make the settle check report a failure.
    let deadline = Instant::now() + ceiling;
    let (settled, note) = match server {
        Server::CljPulse => settle_clj_pulse(&mut client, pid, t0, deadline, &mut watch),
        Server::ClojureLsp => settle_clojure_lsp(&mut client, pid, t0, deadline, &mut watch),
    };
    row.settled = if startup_complete { settled } else { None };
    row.settle_note = if startup_complete {
        note
    } else {
        format!("{note}; sampled after incomplete startup probes, settled time unavailable")
    };
    row.rss_settled = rss_kib(pid);

    // clj-pulse's own account of the same startup. Observed at poll
    // granularity (100 ms), so it is a cross-check on the numbers above, not a
    // number to quote on its own.
    watch.observe(&client, t0);
    row.take_stages(&watch);

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
            if answers(&answer, &site.expect) {
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

struct Startup {
    transport_usable: bool,
    project: Option<Duration>,
    library: Option<Duration>,
    dependencies: Vec<dependencies::Outcome>,
}

/// Initialization can do all of a peer's analysis before returning. Treat a
/// timeout, exit, or error as an incomplete run, not a panic losing later rows.
fn initialize_until(
    client: &mut LspClient,
    root: &Path,
    options: Value,
    deadline: Instant,
) -> Result<Value, String> {
    client.next_id += 1;
    let id = client.next_id;
    send_until(
        client,
        json!({"jsonrpc":"2.0", "id":id, "method":"initialize",
        "params":LspClient::initialize_params(root, options)}),
        deadline,
    )?;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("initialization deadline exceeded")?;
        let message = client
            .incoming
            .recv_timeout(remaining)
            .map_err(|e| format!("initialization failed: {e}"))?;
        if message.get("method").is_none() && message.get("id") == Some(&json!(id)) {
            if let Some(error) = message.get("error") {
                return Err(format!("initialization error: {error}"));
            }
            send_until(
                client,
                json!({"jsonrpc":"2.0", "method":"initialized", "params":{}}),
                deadline,
            )?;
            return Ok(message["result"].clone());
        }
        if let (Some(id), Some(_)) = (message.get("id"), message.get("method")) {
            send_until(
                client,
                json!({"jsonrpc":"2.0", "id":id, "result":null}),
                deadline,
            )?;
        }
        client.notifications.push(message);
    }
}

/// The ordinary test client intentionally uses blocking IO. A benchmark has
/// to report a hung peer, including one that stops draining its input pipe.
#[cfg(unix)]
fn send_until(client: &mut LspClient, message: Value, deadline: Instant) -> Result<(), String> {
    use std::io::{ErrorKind, Write};
    use std::os::fd::AsRawFd;
    struct RestoreFlags {
        fd: i32,
        flags: i32,
    }
    impl Drop for RestoreFlags {
        fn drop(&mut self) {
            unsafe {
                libc::fcntl(self.fd, libc::F_SETFL, self.flags);
            }
        }
    }
    let fd = client.stdin.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(format!(
            "cannot make benchmark pipe nonblocking: {}",
            std::io::Error::last_os_error()
        ));
    }
    let _restore = RestoreFlags { fd, flags };
    let body = message.to_string();
    let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    let mut remaining = frame.as_bytes();
    while !remaining.is_empty() {
        if Instant::now() >= deadline {
            return Err("startup deadline exceeded writing request".into());
        }
        match client.stdin.write(remaining) {
            Ok(0) => return Err("server closed its input pipe".into()),
            Ok(written) => remaining = &remaining[written..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(2)),
                );
            }
            Err(e) => return Err(format!("request write failed: {e}")),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn send_until(_: &mut LspClient, _: Value, _: Instant) -> Result<(), String> {
    Err("deadline-bounded benchmark transport requires Unix".into())
}

/// At most one request per target is outstanding. Collect answers as they arrive,
/// regardless of request order: a slow dependency cannot block the project probe.
/// The overall deadline bounds even a server that never answers a request.
fn poll_definitions(
    client: &mut LspClient,
    probes: &Probes,
    dependency_uri: &str,
    t0: Instant,
    deadline: Instant,
    watch: &mut StageWatch,
) -> Startup {
    let sites = [
        probes.startup_project.as_ref(),
        probes.startup_library.as_ref(),
    ];
    let mut params: Vec<Option<Value>> = sites
        .iter()
        .map(|site| {
            site.map(|s| json!({
        "textDocument": {"uri": tower_lsp::lsp_types::Url::from_file_path(&s.file).unwrap()},
        "position": {"line": s.line, "character": s.character}
    }))
        })
        .collect();
    for i in 0..probes.dependencies.targets.len() {
        params.push(Some(json!({"textDocument": {"uri": dependency_uri},
            "position": {"line": probes.dependencies.probe_line(i), "character": 0}})));
    }
    let mut outcomes: Vec<dependencies::Outcome> = sites
        .iter()
        .map(|site| dependencies::Outcome::new(site.map(|s| s.token.clone()).unwrap_or_default()))
        .chain(
            probes
                .dependencies
                .targets
                .iter()
                .map(|t| dependencies::Outcome::new(t.symbol.clone())),
        )
        .collect();
    let mut pending = BTreeMap::new();
    let mut retry = vec![t0; params.len()];
    let mut next_probe = 0;
    let mut transport_usable = true;
    let mut last_progress = t0;
    'poll: while Instant::now() < deadline {
        let start = next_probe;
        for offset in 0..params.len() {
            let i = (start + offset) % params.len();
            let param = &params[i];
            if pending.len() >= MAX_STARTUP_IN_FLIGHT {
                break;
            }
            if outcomes[i].ready_ms.is_some()
                || retry[i] > Instant::now()
                || pending.values().any(|&index| index == i)
            {
                continue;
            }
            let Some(param) = param else { continue };
            if Instant::now() >= deadline {
                break;
            }
            client.next_id += 1;
            let id = client.next_id;
            outcomes[i].attempts += 1;
            if let Err(reason) = send_until(
                client,
                json!({"jsonrpc":"2.0", "id":id,
                "method":"textDocument/definition", "params":param}),
                deadline,
            ) {
                for outcome in outcomes.iter_mut().filter(|o| o.ready_ms.is_none()) {
                    outcome.last_failure = Some(reason.clone());
                }
                transport_usable = false;
                break 'poll;
            }
            pending.insert(id, i);
            next_probe = (i + 1) % params.len();
        }
        if params
            .iter()
            .enumerate()
            .all(|(i, p)| p.is_none() || outcomes[i].ready_ms.is_some())
        {
            break;
        }
        let wait = deadline.saturating_duration_since(Instant::now()).min(POLL);
        match client.incoming.recv_timeout(wait) {
            Ok(message) => {
                if message.get("method").is_some() {
                    if let Some(id) = message.get("id") {
                        if let Err(reason) = send_until(
                            client,
                            json!({"jsonrpc":"2.0", "id":id, "result":null}),
                            deadline,
                        ) {
                            for outcome in outcomes.iter_mut().filter(|o| o.ready_ms.is_none()) {
                                outcome.last_failure = Some(reason.clone());
                            }
                            transport_usable = false;
                            break;
                        }
                    } else {
                        client.notifications.push(message);
                    }
                } else if let Some(i) = message["id"].as_i64().and_then(|id| pending.remove(&id)) {
                    let accepted = message.get("error").is_none()
                        && if i < 2 {
                            answers(&message["result"], &sites[i].unwrap().expect)
                        } else {
                            probes.dependencies.targets[i - 2].accepts(&message["result"])
                        };
                    outcomes[i].observe(&message, accepted, t0.elapsed().as_millis() as u64);
                    retry[i] = Instant::now() + POLL;
                } else {
                    client.stash(message);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                for outcome in outcomes.iter_mut().filter(|o| o.ready_ms.is_none()) {
                    outcome.last_failure = Some("server disconnected".into());
                }
                transport_usable = false;
                break;
            }
        }
        watch.observe(client, t0);
        if last_progress.elapsed() >= Duration::from_secs(10) {
            println!(
                "  startup at {} s: project {}, dependencies {}/{}",
                t0.elapsed().as_secs(),
                if outcomes[0].ready_ms.is_some() {
                    "ready"
                } else {
                    "waiting"
                },
                outcomes
                    .iter()
                    .skip(2)
                    .filter(|o| o.ready_ms.is_some())
                    .count(),
                outcomes.len() - 2
            );
            last_progress = Instant::now();
        }
    }
    // If an outstanding request hit the deadline, the peer may be wedged.
    // Do not send cancellation/cleanup over a potentially partial frame, or
    // wait through another settle deadline. Drop kills/reaps the child.
    if !pending.is_empty() && Instant::now() >= deadline {
        transport_usable = false;
        for &i in pending.values() {
            outcomes[i].last_failure =
                Some("startup deadline exceeded waiting for response".into());
        }
    }
    Startup {
        transport_usable,
        project: outcomes[0].ready_ms.map(Duration::from_millis),
        library: outcomes[1].ready_ms.map(Duration::from_millis),
        dependencies: outcomes.into_iter().skip(2).collect(),
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
) -> (Option<Duration>, String) {
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
) -> (Option<Duration>, String) {
    let (settled, note) = quiesce(
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
    (settled, format!("{note}{progress}"))
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
) -> (Option<Duration>, String) {
    loop {
        reset(client, watch, t0);
        let quiet = quiet_for(client, methods, QUIET);
        if quiet && !has_children(pid) {
            return (Some(t0.elapsed()), note);
        }
        if Instant::now() >= deadline {
            return (None, format!("{note}; never settled within the ceiling"));
        }
    }
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
}

// ---------------------------------------------------------------------------
// The server's own log lines: a cross-check, never a metric
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StageWatch {
    /// Needle -> (the line, when it was first seen).
    seen: BTreeMap<&'static str, (String, Duration)>,
    /// The engine of the last `clojurePulse/lintStatus` seen. Kept here rather
    /// than read off the stash at the end, which the sampling loops clear.
    lint_engine: Option<String>,
    /// Whether a `$/progress` has reported `end` — clojure-lsp's analysis
    /// progress, when it reports one.
    progress_end: bool,
}

impl StageWatch {
    /// Records the stage lines among the messages stashed so far. Called
    /// between polls, so a time here is accurate to [`POLL`], not better.
    fn observe(&mut self, client: &LspClient, t0: Instant) {
        let elapsed = t0.elapsed();
        for msg in &client.notifications {
            if msg["method"] == "$/progress" && msg["params"]["value"]["kind"] == "end" {
                self.progress_end = true;
                continue;
            }
            if msg["method"] == "clojurePulse/lintStatus" {
                if let Some(engine) = msg["params"]["engine"].as_str() {
                    self.lint_engine = Some(engine.to_string());
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
                }
            }
        }
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
    /// Definition into a JAR (`clojure.string`), the same way.
    startup_library: Option<Site>,
    /// Definition into the project from the largest file, for the latency
    /// median.
    edit_site: Option<Site>,
    dependencies: dependencies::Inventory,
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
        let mut startup_library = None;
        for (_, path) in files.iter().rev() {
            if startup_project.is_some() && startup_library.is_some() {
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
            if startup_library.is_none() {
                startup_library = library_site(&text, path);
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
            dependencies: match std::env::var_os("CLJ_PULSE_BENCH_CLASSPATH_FILE") {
                Some(path) => match std::fs::read_to_string(&path) {
                    Ok(cp) => dependencies::Inventory::discover(
                        root,
                        std::env::split_paths(cp.trim())
                            .map(|p| if p.is_absolute() { p } else { root.join(p) })
                            .collect(),
                    ),
                    Err(e) => dependencies::Inventory {
                        errors: vec![dependencies::Exclusion {
                            artifact: PathBuf::from(path),
                            reason: e.to_string(),
                        }],
                        ..Default::default()
                    },
                },
                None => dependencies::Inventory {
                    errors: vec![dependencies::Exclusion {
                        artifact: root.into(),
                        reason: "no prepared classpath supplied".into(),
                    }],
                    ..Default::default()
                },
            },
        }
    }

    /// The files the startup probes need open, each once.
    fn startup_sites(&self) -> Vec<&Site> {
        let mut sites: Vec<&Site> = Vec::new();
        for site in [&self.startup_project, &self.startup_library]
            .into_iter()
            .flatten()
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
        site("first definition", &self.startup_project);
        site("first library def", &self.startup_library);
        site("definition latency", &self.edit_site);
        println!(
            "  dependency probes: {} / {} Clojure source-bearing entries ({} total entries)",
            self.dependencies.targets.len(),
            self.dependencies.source_dependencies,
            self.dependencies.entries
        );
        for target in &self.dependencies.targets {
            println!(
                "    {} -> {}!{}:{}",
                target.symbol,
                target.artifact.display(),
                target.source,
                target.line + 1
            );
        }
        for entry in &self.dependencies.excluded {
            println!(
                "    excluded {}: {}",
                entry.artifact.display(),
                entry.reason
            );
        }
        for entry in &self.dependencies.errors {
            println!(
                "    DISCOVERY ERROR {}: {}",
                entry.artifact.display(),
                entry.reason
            );
        }
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
    first_definition: Option<Duration>,
    first_library_definition: Option<Duration>,
    dependency_outcomes: Vec<dependencies::Outcome>,
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
    fn new(server: Server, temp: Temp, corpus: &str) -> Self {
        Self {
            server,
            temp,
            corpus: corpus.to_string(),
            sync: SyncKind::Incremental,
            first_definition: None,
            first_library_definition: None,
            dependency_outcomes: Vec::new(),
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
            "{} ({}) on {}",
            self.server.label(),
            self.temp.label(),
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
        row("time to first definition", &ms(self.first_definition));
        row(
            "time to first library definition",
            &ms(self.first_library_definition),
        );
        row("tested dependency navigation", &self.dependency_time());
        row(
            "dependency coverage",
            &format!(
                "{}/{} probes; {}/{} source dependencies selected; {} discovery errors",
                self.dependency_outcomes
                    .iter()
                    .filter(|o| o.ready_ms.is_some())
                    .count(),
                self.dependency_outcomes.len(),
                probes.dependencies.targets.len(),
                probes.dependencies.source_dependencies,
                probes.dependencies.errors.len()
            ),
        );
        for outcome in self
            .dependency_outcomes
            .iter()
            .filter(|o| o.ready_ms.is_none())
        {
            row(
                &format!("  unresolved {}", outcome.symbol),
                outcome
                    .last_failure
                    .as_deref()
                    .unwrap_or("not attempted before deadline"),
            );
        }
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
        if self.server == Server::ClojureLsp {
            return "clojure-lsp diagnostics (embedded clj-kondo and built-in linters)";
        }
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
    fn dependency_time(&self) -> String {
        match dependencies::ready_ms(&self.dependency_outcomes) {
            Some(ms) => format!("{ms} ms"),
            None => format!(
                "incomplete ({}/{})",
                self.dependency_outcomes
                    .iter()
                    .filter(|o| o.ready_ms.is_some())
                    .count(),
                self.dependency_outcomes.len()
            ),
        }
    }

    fn print_json(&self, probes: &Probes) {
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
                "first_definition_ms": ms(self.first_definition),
                "first_library_definition_ms": ms(self.first_library_definition),
                "classpath_command": "clojure -A:dev:test -Spath",
                "dependency_navigation_ms": dependencies::ready_ms(&self.dependency_outcomes),
                "dependency_probes": self.dependency_outcomes,
                "dependency_inventory": probes.dependencies,
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
        "  {:<12} {:<5} {:>9} {:>11} {:>9} {:>7} {:>7} {:>11} {:>11}",
        "server",
        "temp",
        "1st def",
        "deps ready",
        "settled",
        "RSS",
        "def",
        format!("edit {large}K"),
        small
            .map(|k| format!("edit {k}K"))
            .unwrap_or_else(|| "edit -".into()),
    );
    for r in rows {
        println!(
            "  {:<12} {:<5} {:>9} {:>11} {:>9} {:>7} {:>7} {:>11} {:>11}",
            r.server.label(),
            r.temp.label(),
            ms(r.first_definition),
            r.dependency_time(),
            ms(r.settled),
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

#[test]
fn dependency_probes_check_real_navigation_and_report_missing_targets() {
    let root = common::setup_project();
    let library = tempfile::tempdir().unwrap();
    std::fs::create_dir(library.path().join("bench")).unwrap();
    std::fs::write(
        library.path().join("bench/lib.clj"),
        "(ns bench.lib)\n(defn available [] :ok)\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join(".cpcache")).unwrap();
    let cp = std::env::join_paths([root.path().join("src"), library.path().into()]).unwrap();
    std::fs::write(root.path().join(".cpcache/bench.cp"), cp.as_encoded_bytes()).unwrap();
    let mut probes = Probes::discover(root.path());
    probes.dependencies =
        dependencies::Inventory::discover(root.path(), vec![library.path().into()]);
    assert_eq!(probes.dependencies.targets.len(), 1);
    // The smoke probe depends on the host's Clojure JAR, so omit it in this
    // hermetic test. Dependency coverage is the external source directory.
    probes.startup_library = None;
    let mut client = LspClient::start(root.path());
    client.initialize(root.path());
    for site in probes.startup_sites() {
        client.did_open(&site.file);
    }
    let uri = tower_lsp::lsp_types::Url::from_file_path(root.path().join("src/bench_probe.clj"))
        .unwrap()
        .to_string();
    client.did_open_uri(&uri, &probes.dependencies.buffer());
    let t0 = Instant::now();
    let result = poll_definitions(
        &mut client,
        &probes,
        &uri,
        t0,
        t0 + Duration::from_secs(10),
        &mut StageWatch::default(),
    );
    assert!(result.project.is_some());
    assert!(
        dependencies::ready_ms(&result.dependencies).is_some(),
        "{:?}",
        result.dependencies
    );

    probes.dependencies.targets[0].symbol = "bench.lib/missing".into();
    client.did_close(&root.path().join("src/bench_probe.clj"));
    client.did_open_uri(&uri, &probes.dependencies.buffer());
    let t0 = Instant::now();
    let result = poll_definitions(
        &mut client,
        &probes,
        &uri,
        t0,
        t0 + Duration::from_millis(200),
        &mut StageWatch::default(),
    );
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(dependencies::ready_ms(&result.dependencies), None);
    assert!(result.dependencies[0].last_failure.is_some());
}

#[cfg(unix)]
#[test]
fn initialization_deadline_bounds_a_server_that_never_answers() {
    let root = common::setup_project();
    let mut client = LspClient::start(root.path());
    assert_eq!(
        unsafe { libc::kill(client.child.id() as i32, libc::SIGSTOP) },
        0
    );
    let t0 = Instant::now();
    let error = initialize_until(
        &mut client,
        root.path(),
        json!({}),
        t0 + Duration::from_millis(100),
    )
    .unwrap_err();
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(error.contains("initialization"), "{error}");
}

#[cfg(unix)]
#[test]
fn dependency_startup_deadline_bounds_a_server_that_never_answers() {
    let root = common::setup_project();
    let mut probes = Probes::discover(root.path());
    probes.startup_library = None;
    let mut client = LspClient::start(root.path());
    client.initialize(root.path());
    for site in probes.startup_sites() {
        client.did_open(&site.file);
    }
    probes.dependencies.targets.push(dependencies::Target {
        artifact: PathBuf::from("/missing.jar"),
        source: "missing.clj".into(),
        symbol: "missing/var".into(),
        line: 0,
        character: 0,
    });
    // A stopped real child consumes no requests and cannot produce responses.
    // LspClient::drop kills and reaps it even while stopped.
    assert_eq!(
        unsafe { libc::kill(client.child.id() as i32, libc::SIGSTOP) },
        0
    );
    let t0 = Instant::now();
    let result = poll_definitions(
        &mut client,
        &probes,
        "file:///missing.clj",
        t0,
        t0 + Duration::from_millis(100),
        &mut StageWatch::default(),
    );
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(result.project.is_none());
    assert_eq!(dependencies::ready_ms(&result.dependencies), None);
    assert_eq!(result.dependencies[0].attempts, 1);
    assert!(result.dependencies[0]
        .last_failure
        .as_ref()
        .unwrap()
        .contains("deadline"));
}

#[cfg(unix)]
#[test]
fn startup_write_deadline_handles_pipe_backpressure() {
    let root = common::setup_project();
    let mut client = LspClient::start(root.path());
    client.initialize(root.path());
    assert_eq!(
        unsafe { libc::kill(client.child.id() as i32, libc::SIGSTOP) },
        0
    );
    let t0 = Instant::now();
    let error = send_until(
        &mut client,
        json!({"payload":"x".repeat(1024 * 1024)}),
        t0 + Duration::from_millis(100),
    )
    .unwrap_err();
    assert!(error.contains("deadline"), "{error}");
    assert!(t0.elapsed() < Duration::from_secs(2));
}
