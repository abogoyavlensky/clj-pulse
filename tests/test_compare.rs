//! Differential correctness against clj-kondo's analysis: every position the
//! oracle knows the answer to becomes a probe, and clj-pulse's definition,
//! references and rename answers are judged against it. `bb check` runs the
//! whole pipeline on `simple_project`; `bb compare` runs it on a pinned real
//! corpus and reports every disagreement by language construct.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Value};

use common::diff::{brief, brief_set, Divergence};
use common::oracle::{self, Analysis, Expectation, Probe, Sites};
use common::session::{settle, Session, REQUEST_TIMEOUT};
use common::setup_project;
use common::sites::{definition_uris, landing, Dialect, Expect, Landing};
use common::LspClient;

// ---------------------------------------------------------------------------
// Asking
// ---------------------------------------------------------------------------

/// What the server said to one probe, in the shape its request kind has.
#[derive(Debug)]
enum Answer {
    /// A definition or references result, `null` included.
    Result(Value),
    /// A rename the server refused: `invalid params` (how `server.rs` maps
    /// every `rename_target` rejection) or `null` from `prepareRename` or
    /// `rename`.
    Refused,
    /// The `WorkspaceEdit` a rename produced.
    Edit(Value),
    /// A JSON-RPC error that is not a refusal — the panic guard's internal
    /// error, for one. Recorded on the session like every other one.
    Error(String),
}

const INVALID_PARAMS: i64 = -32602;

/// A rename's two responses as one answer. `prepare` decides first: a server
/// that refuses there is never asked to rename.
fn rename_answer(prepare: &Value, rename: Option<&Value>) -> Answer {
    let classify = |msg: &Value| -> Option<Answer> {
        if let Some(err) = msg.get("error") {
            return Some(if err["code"].as_i64() == Some(INVALID_PARAMS) {
                Answer::Refused
            } else {
                Answer::Error(err.to_string())
            });
        }
        msg["result"].is_null().then_some(Answer::Refused)
    };
    if let Some(verdict) = classify(prepare) {
        return verdict;
    }
    match rename {
        None => Answer::Error("rename was not asked".into()),
        Some(msg) => classify(msg).unwrap_or_else(|| Answer::Edit(msg["result"].clone())),
    }
}

fn ask(session: &mut Session, probe: &Probe) -> Answer {
    let at = json!({
        "textDocument": { "uri": format!("file://{}", probe.file.display()) },
        "position": { "line": probe.line, "character": probe.character }
    });
    match &probe.expect {
        Expectation::Definition { .. } | Expectation::LibraryDefinition { .. } => {
            Answer::Result(session.request("textDocument/definition", at))
        }
        Expectation::References(_) => {
            let mut params = at;
            params["context"] = json!({ "includeDeclaration": true });
            Answer::Result(session.request("textDocument/references", params))
        }
        Expectation::RenameRefused | Expectation::RenameSites(_) => {
            // The new name must be a valid symbol (or keyword name without its
            // colon), or the refusal would be about the name rather than the
            // target.
            let new_name = format!(
                "{}__cmp",
                probe
                    .token
                    .trim_start_matches(':')
                    .rsplit('/')
                    .next()
                    .unwrap_or("x")
            );
            let prepare = session
                .client
                .request_full("textDocument/prepareRename", at.clone());
            let answer = if prepare.get("error").is_some() || prepare["result"].is_null() {
                rename_answer(&prepare, None)
            } else {
                let mut params = at;
                params["newName"] = json!(new_name);
                let rename = session.client.request_full("textDocument/rename", params);
                rename_answer(&prepare, Some(&rename))
            };
            if let Answer::Error(err) = &answer {
                session
                    .errors
                    .push(format!("rename at {} answered {err}", where_(probe, None)));
            }
            answer
        }
    }
}

// ---------------------------------------------------------------------------
// Judging
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Agree,
    /// `missing` is how many expected sites the answer lacks; zero means the
    /// answer holds everything the oracle knows and more.
    Diverge {
        expected: String,
        got: String,
        missing: usize,
    },
    /// An empty or `null` answer where the oracle has one: a wrong answer,
    /// reported in its own column so "resolved wrong" and "did not resolve"
    /// stay apart, but a divergence for the allowlist and strict mode.
    Null {
        expected: String,
    },
    /// The per-line counts agree and only the columns differ — never a
    /// divergence, since the two sides range tokens differently by design.
    Soft,
}

/// The path of a `file:` URI, percent-decoded.
fn uri_path(uri: &str) -> Option<PathBuf> {
    let p = uri.strip_prefix("file://")?;
    Some(PathBuf::from(
        percent_encoding::percent_decode_str(p)
            .decode_utf8_lossy()
            .to_string(),
    ))
}

/// A `Location[]` answer as sites.
fn location_sites(result: &Value) -> Sites {
    let exact = result
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|l| {
                    let file = uri_path(l["uri"].as_str()?)?;
                    let start = &l["range"]["start"];
                    Some((
                        file,
                        start["line"].as_u64()? as u32,
                        start["character"].as_u64()? as u32,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Sites::from_exact(exact)
}

/// A `WorkspaceEdit` as the sites its edits start at, from `changes` and
/// `documentChanges` both — the server sends the first, but the shape is the
/// client's to choose.
fn edit_sites(edit: &Value) -> Sites {
    let mut exact = BTreeSet::new();
    let mut add = |uri: &str, edits: &Value| {
        let Some(file) = uri_path(uri) else { return };
        for e in edits.as_array().into_iter().flatten() {
            let start = &e["range"]["start"];
            if let (Some(line), Some(col)) = (start["line"].as_u64(), start["character"].as_u64()) {
                exact.insert((file.clone(), line as u32, col as u32));
            }
        }
    };
    if let Some(changes) = edit["changes"].as_object() {
        for (uri, edits) in changes {
            add(uri, edits);
        }
    }
    for change in edit["documentChanges"].as_array().into_iter().flatten() {
        if let Some(uri) = change["textDocument"]["uri"].as_str() {
            add(uri, &change["edits"]);
        }
    }
    Sites::from_exact(exact)
}

/// `file:line×count` for every line of a site set, relative to `root`, so a
/// divergence reads as which lines one side has that the other does not.
fn site_keys(sites: &Sites, root: &Path) -> BTreeSet<String> {
    sites
        .per_line
        .iter()
        .map(|((file, line), count)| {
            let rel = file.strip_prefix(root).unwrap_or(file).display();
            if *count == 1 {
                format!("{rel}:{}", line + 1)
            } else {
                format!("{rel}:{}×{count}", line + 1)
            }
        })
        .collect()
}

/// `sites` without the ones outside the linted roots: a file clj-pulse indexes
/// that the oracle never read (a sub-project's, a scratch dir's) is neither
/// side's finding.
fn within(sites: Sites, roots: &[PathBuf]) -> Sites {
    Sites::from_exact(
        sites
            .exact
            .into_iter()
            .filter(|(file, _, _)| roots.iter().any(|r| file.starts_with(r)))
            .collect(),
    )
}

/// How many expected sites the answer lacks, counted per line the way the
/// verdict is: a site the answer ranges from another column is present.
fn missing_sites(expected: &Sites, got: &Sites) -> usize {
    expected
        .per_line
        .iter()
        .map(|(key, n)| n.saturating_sub(got.per_line.get(key).copied().unwrap_or(0)))
        .sum()
}

fn judge_sites(expected: &Sites, got: &Sites, root: &Path) -> Verdict {
    if got.is_empty() && !expected.is_empty() {
        return Verdict::Null {
            expected: format!("{:?}", site_keys(expected, root)),
        };
    }
    if expected.per_line != got.per_line {
        let (mine, theirs) = (site_keys(got, root), site_keys(expected, root));
        return Verdict::Diverge {
            expected: brief_set(&theirs, &mine),
            got: brief_set(&mine, &theirs),
            missing: missing_sites(expected, got),
        };
    }
    if expected.exact != got.exact {
        return Verdict::Soft;
    }
    Verdict::Agree
}

fn judge(probe: &Probe, answer: &Answer, root: &Path, roots: &[PathBuf]) -> Verdict {
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    match (&probe.expect, answer) {
        (Expectation::Definition { file, line }, Answer::Result(result)) => {
            let expected = format!("{}:{}", rel(file), line + 1);
            let uris = definition_uris(result);
            let Some(first) = uris.first() else {
                return Verdict::Null { expected };
            };
            let start_line = match result {
                Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
                other => other.clone(),
            };
            let start_line = start_line["range"]["start"]["line"]
                .as_u64()
                .or_else(|| start_line["targetSelectionRange"]["start"]["line"].as_u64());
            let landed =
                uri_path(first).is_some_and(|p| p == *file) && start_line == Some(*line as u64);
            if landed {
                Verdict::Agree
            } else {
                Verdict::Diverge {
                    missing: 1,
                    expected,
                    got: match uri_path(first) {
                        Some(p) => format!(
                            "{}:{}",
                            rel(&p),
                            start_line.map(|l| (l + 1).to_string()).unwrap_or_default()
                        ),
                        None => first.clone(),
                    },
                }
            }
        }
        (Expectation::LibraryDefinition { ns, dialect }, Answer::Result(result)) => {
            let entry = ns.replace('-', "_").replace('.', "/");
            let expected = format!("<library>/{entry}.{{{}}}", dialect_exts(*dialect));
            let uris = definition_uris(result);
            if uris.is_empty() {
                return Verdict::Null { expected };
            }
            match landing(result, &Expect::Archive(entry.clone()), &probe.file) {
                Landing::Landed => return Verdict::Agree,
                Landing::WrongDialect => {
                    return Verdict::Diverge {
                        expected,
                        got: format!("wrong dialect: {}", uris.join(", ")),
                        missing: 1,
                    }
                }
                Landing::Miss => {}
            }
            // A source-directory library (a git dep, a `:local/root`) is a
            // plain `file:` URI outside the corpus.
            let in_dir = uris.iter().any(|u| {
                uri_path(u).is_some_and(|p| {
                    !p.starts_with(root)
                        && p.extension().and_then(|e| e.to_str()).is_some_and(|ext| {
                            dialect.accepts(ext)
                                && p.to_string_lossy().ends_with(&format!("{entry}.{ext}"))
                        })
                })
            });
            if in_dir {
                Verdict::Agree
            } else {
                Verdict::Diverge {
                    expected,
                    got: uris.join(", "),
                    missing: 1,
                }
            }
        }
        (Expectation::References(sites), Answer::Result(result)) => {
            judge_sites(sites, &within(location_sites(result), roots), root)
        }
        (Expectation::RenameSites(sites), Answer::Edit(edit)) => {
            judge_sites(sites, &within(edit_sites(edit), roots), root)
        }
        (Expectation::RenameSites(sites), Answer::Refused) => Verdict::Diverge {
            expected: format!("{:?}", site_keys(sites, root)),
            got: "refused".into(),
            missing: sites.exact.len(),
        },
        (Expectation::RenameRefused, Answer::Refused) => Verdict::Agree,
        (Expectation::RenameRefused, Answer::Edit(edit)) => Verdict::Diverge {
            expected: "refused".into(),
            got: format!("edit of {:?}", site_keys(&edit_sites(edit), root)),
            missing: 0,
        },
        (_, Answer::Error(err)) => Verdict::Null {
            expected: format!("an answer, not {}", brief(&json!(err))),
        },
        (expect, answer) => Verdict::Null {
            expected: format!("{expect:?} cannot be judged from {answer:?}"),
        },
    }
}

fn dialect_exts(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Clj => "clj,cljc",
        Dialect::Cljs => "cljs,cljc",
        Dialect::Cljc => "clj,cljs,cljc",
    }
}

// ---------------------------------------------------------------------------
// The allowlist
// ---------------------------------------------------------------------------

/// A divergence that is understood and accepted, for now or for good. Triage
/// adds an entry with a dated reason; a fixed bug removes its entry.
struct Known {
    bucket_prefix: &'static str,
    matches: fn(&Probe, &Verdict) -> bool,
    reason: &'static str,
}

static KNOWN: &[Known] = &[
    Known {
        bucket_prefix: "var-def/",
        matches: |probe, _| probe.token.starts_with(':'),
        reason: "Integrant keys are definitions in clj-pulse only",
    },
    Known {
        bucket_prefix: "var-usage/",
        matches: |probe, _| {
            matches!(&probe.expect, Expectation::LibraryDefinition { ns, .. } if ns.starts_with("letgo."))
        },
        reason: "let-go core is indexed from lgx deps only",
    },
    // A protocol method's implementations (`(deftype T [] P (m [_] …))`,
    // `extend-protocol`) are sites in clj-pulse — references show them and a
    // rename must rewrite them — while kondo's analysis lists the method's
    // callers alone. A superset answer is the difference; a missing caller is
    // not.
    Known {
        bucket_prefix: "var-def/defprotocol",
        matches: |_, verdict| matches!(verdict, Verdict::Diverge { missing: 0, .. }),
        reason: "protocol method implementations are sites in clj-pulse; kondo lists callers only",
    },
    // `references::local_refs_at` never claims a qualified symbol ("locals
    // are never qualified"), so a cursor on the binding resolves the keyword
    // it reads — one occurrence — where kondo sees the local `x` and its
    // usages. From a usage of `x` the two agree. ROADMAP backlog, 2026-09-17.
    Known {
        bucket_prefix: "local/destructured",
        matches: |probe, _| probe.token.contains('/'),
        reason: "a qualified `:keys` entry (`{:keys [c/x]}`) resolves as the keyword it reads, not the local it binds (2026-09-17)",
    },
];

fn known_reason(probe: &Probe, verdict: &Verdict) -> Option<&'static str> {
    KNOWN
        .iter()
        .find(|k| probe.bucket.starts_with(k.bucket_prefix) && (k.matches)(probe, verdict))
        .map(|k| k.reason)
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct Row {
    probes: usize,
    agree: usize,
    diverge: usize,
    known: usize,
    null: usize,
    soft: usize,
}

#[derive(Default)]
struct Report {
    corpus: String,
    rows: BTreeMap<String, Row>,
    new_divergences: Vec<Divergence>,
    known: BTreeMap<&'static str, Vec<Divergence>>,
    unjudged: usize,
    files: usize,
}

fn where_(probe: &Probe, root: Option<&Path>) -> String {
    let file = match root {
        Some(root) => probe.file.strip_prefix(root).unwrap_or(&probe.file),
        None => &probe.file,
    };
    format!(
        "{}:{}:{} `{}`",
        file.display(),
        probe.line + 1,
        probe.character + 1,
        probe.token
    )
}

impl Report {
    fn record(&mut self, probe: &Probe, verdict: Verdict, root: &Path) {
        let row = self.rows.entry(probe.bucket.clone()).or_default();
        row.probes += 1;
        let (expected, got) = match verdict {
            Verdict::Agree => {
                row.agree += 1;
                return;
            }
            Verdict::Soft => {
                row.soft += 1;
                return;
            }
            Verdict::Null { ref expected } => {
                row.null += 1;
                (expected.clone(), "null".to_string())
            }
            Verdict::Diverge {
                ref expected,
                ref got,
                ..
            } => (expected.clone(), got.clone()),
        };
        let divergence = Divergence {
            request: format!("{} [{}]", probe.expect.request(), probe.bucket),
            site: where_(probe, Some(root)),
            mine: got,
            theirs: expected,
        };
        match known_reason(probe, &verdict) {
            Some(reason) => {
                row.known += 1;
                self.known.entry(reason).or_default().push(divergence);
            }
            None => {
                if matches!(verdict, Verdict::Diverge { .. }) {
                    row.diverge += 1;
                }
                self.new_divergences.push(divergence);
            }
        }
    }

    fn print(&self) {
        println!();
        println!(
            "compare on {}: {} files visited, {} unjudged tokens",
            self.corpus, self.files, self.unjudged
        );
        println!();
        println!(
            "  {:<40} {:>6} {:>6} {:>7} {:>6} {:>5} {:>5}",
            "bucket", "probes", "agree", "diverge", "known", "null", "soft"
        );
        println!(
            "  {:-<40} {:-<6} {:-<6} {:-<7} {:-<6} {:-<5} {:-<5}",
            "", "", "", "", "", "", ""
        );
        let mut total = Row::default();
        for (bucket, row) in &self.rows {
            println!(
                "  {:<40} {:>6} {:>6} {:>7} {:>6} {:>5} {:>5}",
                bucket, row.probes, row.agree, row.diverge, row.known, row.null, row.soft
            );
            total.probes += row.probes;
            total.agree += row.agree;
            total.diverge += row.diverge;
            total.known += row.known;
            total.null += row.null;
            total.soft += row.soft;
        }
        println!(
            "  {:<40} {:>6} {:>6} {:>7} {:>6} {:>5} {:>5}",
            "total", total.probes, total.agree, total.diverge, total.known, total.null, total.soft
        );
        println!();
        for (bucket, row) in &self.rows {
            println!(
                "COMPARE_JSON {}",
                json!({
                    "corpus": self.corpus,
                    "bucket": bucket,
                    "probes": row.probes,
                    "agree": row.agree,
                    "diverge": row.diverge,
                    "known": row.known,
                    "null": row.null,
                    "soft": row.soft,
                })
            );
        }
        if !self.new_divergences.is_empty() {
            println!();
            println!("new divergences ({}):", self.new_divergences.len());
            for d in &self.new_divergences {
                d.print("got", "expected");
            }
        }
        for (reason, divergences) in &self.known {
            println!();
            println!("known: {reason} ({}):", divergences.len());
            for d in divergences {
                d.print("got", "expected");
            }
        }
    }
}

/// Files in path order; each is opened, its probes asked and judged, then
/// closed — the way an editor visits a project, and so that the live
/// definitions of an open buffer take part the way they do for a user.
fn ask_and_judge(
    session: &mut Session,
    probes: &[Probe],
    analysis: &Analysis,
    root: &Path,
    roots: &[PathBuf],
    corpus: &str,
) -> Report {
    let mut report = Report {
        corpus: corpus.to_string(),
        ..Default::default()
    };
    let mut by_file: BTreeMap<&Path, Vec<&Probe>> = BTreeMap::new();
    for probe in probes {
        by_file.entry(&probe.file).or_default().push(probe);
    }
    for (file, probes) in by_file {
        session.client.did_open(file);
        for probe in probes {
            let answer = ask(session, probe);
            let verdict = judge(probe, &answer, root, roots);
            report.record(probe, verdict, root);
        }
        session.client.did_close(file);
        if let Ok(text) = std::fs::read_to_string(file) {
            report.unjudged += oracle::unjudged(&text, &oracle::covered(analysis, file));
        }
        report.files += 1;
    }
    report
}

/// `Some(root)` when the host has clj-kondo, printing why not otherwise: the
/// suite has to stay green on a box without it.
fn with_kondo(name: &str) -> Option<tempfile::TempDir> {
    match oracle::kondo_available() {
        Some(version) => {
            if !version.contains(oracle::PINNED_KONDO) {
                println!(
                    "{name}: clj-kondo {version} is not the pinned {}",
                    oracle::PINNED_KONDO
                );
            }
            Some(setup_project())
        }
        None => {
            println!("{name}: skipped, no clj-kondo on PATH");
            None
        }
    }
}

mod oracle_tests {
    use super::*;

    fn fixture_probes() -> Option<(tempfile::TempDir, Vec<Probe>)> {
        let tmp = with_kondo("oracle_tests")?;
        let analysis = oracle::run(tmp.path(), &["src"], "").expect("clj-kondo runs");
        let probes = oracle::probes(&analysis);
        Some((tmp, probes))
    }

    fn at<'a>(probes: &'a [Probe], file: &str, line: u32, character: u32) -> Vec<&'a Probe> {
        probes
            .iter()
            .filter(|p| p.file.ends_with(file) && p.line == line && p.character == character)
            .collect()
    }

    #[test]
    fn run_parses_every_analysis_section() {
        let Some(tmp) = with_kondo("run_parses_every_analysis_section") else {
            return;
        };
        let analysis = oracle::run(tmp.path(), &["src"], "").expect("clj-kondo runs");
        assert!(!analysis.var_definitions.is_empty());
        assert!(!analysis.var_usages.is_empty());
        assert!(!analysis.locals.is_empty());
        assert!(!analysis.local_usages.is_empty());
        assert!(!analysis.keywords.is_empty());
        assert!(!analysis.namespace_definitions.is_empty());
        // Filenames come back absolute, whatever kondo printed.
        assert!(analysis
            .var_definitions
            .iter()
            .all(|d| Path::new(&d.filename).is_absolute()));
    }

    #[test]
    fn var_usage_expects_the_definition_name_row() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // utils.clj:7 `(* 2 (core/add x y))` — kondo's `name-col` is 9, the
        // `c` of `core/add`; the probe moves to `add` at column 14 (1-based).
        let found = at(&probes, "src/utils.clj", 6, 13);
        assert_eq!(found.len(), 1, "{found:?}");
        let probe = found[0];
        assert_eq!(probe.token, "core/add");
        assert_eq!(probe.bucket, "var-usage/project/aliased");
        match &probe.expect {
            Expectation::Definition { file, line } => {
                assert!(file.ends_with("src/core.clj"));
                assert_eq!(*line, 4, "`(defn add` is line 5");
            }
            other => panic!("expected Definition, got {other:?}"),
        }
    }

    #[test]
    fn core_usage_expects_a_library_definition_in_the_asking_dialect() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // core.clj:5 `(defn add` — `defn` at column 2.
        let found = at(&probes, "src/core.clj", 4, 1);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].bucket, "var-usage/core/macro");
        match &found[0].expect {
            Expectation::LibraryDefinition { ns, dialect } => {
                assert_eq!(ns, "clojure.core");
                assert_eq!(*dialect, Dialect::Clj);
            }
            other => panic!("expected LibraryDefinition, got {other:?}"),
        }
    }

    #[test]
    fn destructured_local_refuses_rename_and_keys_keyword_too() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // kw_destructure.clj:6 `(defn read-local [{::kw/keys [local]}]` — `local` at column 31.
        let found = at(&probes, "src/kw_destructure.clj", 5, 30);
        let local_rename = found.iter().find(|p| {
            p.bucket == "local/destructured" && matches!(p.expect, Expectation::RenameRefused)
        });
        assert!(local_rename.is_some(), "{found:?}");
        let keys_rename = found
            .iter()
            .find(|p| p.bucket == "keyword/keys" && matches!(p.expect, Expectation::RenameRefused));
        assert!(keys_rename.is_some(), "{found:?}");
        // The local still gets a references probe; the keyword from that
        // position does not, since the cursor is on the local binding.
        assert!(found
            .iter()
            .any(|p| p.bucket == "local/destructured"
                && matches!(p.expect, Expectation::References(_))));
        assert!(!found
            .iter()
            .any(|p| p.bucket.starts_with("keyword/")
                && matches!(p.expect, Expectation::References(_))));
    }

    #[test]
    fn plain_local_definition_and_sites() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // locals.clj:5 `        scaled (* base 2)]` — usage of `base` at column 19.
        let found = at(&probes, "src/locals.clj", 4, 18);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].bucket, "local/plain");
        match &found[0].expect {
            Expectation::Definition { file, line } => {
                assert!(file.ends_with("src/locals.clj"));
                assert_eq!(*line, 3);
            }
            other => panic!("expected Definition, got {other:?}"),
        }
        // locals.clj:4 `  (let [base   (inc n)` — the binding at column 9.
        let binding = at(&probes, "src/locals.clj", 3, 8);
        let refs = binding
            .iter()
            .find_map(|p| match &p.expect {
                Expectation::References(sites) => Some(sites),
                _ => None,
            })
            .expect("references probe on the binding");
        assert_eq!(refs.exact.len(), 3, "binding + two usages: {refs:?}");
        assert!(binding
            .iter()
            .any(|p| matches!(&p.expect, Expectation::RenameSites(s) if s.exact.len() == 3)));
    }

    #[test]
    fn declare_resolves_to_the_real_def_and_stays_a_site() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // ns_options.clj:21 `  (cmap inc (defined-later m)))` — `defined-later` at column 14.
        let usage = at(&probes, "src/ns_options.clj", 20, 13);
        assert_eq!(usage.len(), 1, "{usage:?}");
        match &usage[0].expect {
            Expectation::Definition { line, .. } => {
                assert_eq!(*line, 10, "`(defn defined-later` is line 11")
            }
            other => panic!("expected Definition, got {other:?}"),
        }
        // The declare on line 9 has no probe of its own …
        assert!(at(&probes, "src/ns_options.clj", 8, 9).is_empty());
        // … but the def's sites include it.
        let def = at(&probes, "src/ns_options.clj", 10, 6);
        assert_eq!(def.len(), 2, "references + rename: {def:?}");
        for probe in def {
            assert_eq!(probe.bucket, "var-def/defn");
            let sites = match &probe.expect {
                Expectation::References(s) | Expectation::RenameSites(s) => s,
                other => panic!("{other:?}"),
            };
            let lines: BTreeSet<u32> = sites.exact.iter().map(|(_, l, _)| *l).collect();
            assert_eq!(lines, BTreeSet::from([8, 10, 20]), "{sites:?}");
        }
        // A var with only a declare resolves to the declare.
        let only = at(&probes, "src/ns_options.clj", 11, 3);
        assert_eq!(only.len(), 1);
        match &only[0].expect {
            Expectation::Definition { line, .. } => assert_eq!(*line, 6),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn two_usages_on_one_line_count_twice() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // twice.clj:5 `(defn twin [x] x)` — the def at column 7.
        let def = at(&probes, "src/twice.clj", 4, 6);
        let sites = def
            .iter()
            .find_map(|p| match &p.expect {
                Expectation::References(s) => Some(s),
                _ => None,
            })
            .expect("references probe");
        let usage_line = sites
            .per_line
            .iter()
            .find(|((_, line), _)| *line == 7)
            .map(|(_, n)| *n);
        assert_eq!(usage_line, Some(2), "{sites:?}");
        // The def, the two on line 8, and the one after the emoji on line 17.
        assert_eq!(sites.exact.len(), 4);
    }

    #[test]
    fn keyword_groups_refuse_rename_when_destructured_or_foreign() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // keywords.clj:7 `   ::local true})` — plain `::local` at column 4,
        // cursor on `local` at 6; kw_destructure.clj reads the key through
        // `::kw/keys`, so the whole group refuses.
        let local = at(&probes, "src/keywords.clj", 6, 5);
        assert!(local
            .iter()
            .any(|p| p.bucket == "keyword/qualified"
                && matches!(p.expect, Expectation::RenameRefused)));
        let refs = local
            .iter()
            .find_map(|p| match &p.expect {
                Expectation::References(s) => Some(s),
                _ => None,
            })
            .expect("references probe");
        assert_eq!(refs.exact.len(), 3, "{refs:?}");
        // alias_sites.clj:11 `(def lit :c/site)` — `c` is a namespace nobody
        // defines or requires, so its sites are all in the project: renamable.
        let foreign = at(&probes, "src/alias_sites.clj", 10, 12);
        assert!(
            foreign
                .iter()
                .any(|p| matches!(&p.expect, Expectation::RenameSites(s) if s.exact.len() == 1)),
            "{foreign:?}"
        );
        // ns_options.clj:15 `(get system ::cfg/port)` — `simple.config` is
        // defined in the corpus, so the group is renamable even through an
        // `:as-alias`.
        let port = at(&probes, "src/ns_options.clj", 14, 20);
        assert!(
            port.iter()
                .any(|p| matches!(&p.expect, Expectation::RenameSites(s) if s.exact.len() == 2)),
            "{port:?}"
        );
        // alias_sites.clj:7 `(defn f [{::c/keys [blend]}] blend)` — the
        // `::c/keys` directive at column 11 gets no probe.
        assert!(at(&probes, "src/alias_sites.clj", 6, 14).is_empty());
        // twice.clj:11 `(def config {::keys [:a :b]})` is a map value, not a
        // binding: its `::keys` is a key somebody reads (`:14`), so both
        // sites are probed and expected.
        let data_key = at(&probes, "src/twice.clj", 10, 15);
        assert!(
            data_key.iter().any(|p| p.bucket == "keyword/qualified"
                && matches!(&p.expect, Expectation::References(s) if s.exact.len() == 2)),
            "{data_key:?}"
        );
        // keywords.clj:13 `(assoc m :id (::c/thing m)))` — one site, renamable;
        // the cursor sits on `thing`, past the alias.
        let thing = at(&probes, "src/keywords.clj", 12, 20);
        assert!(
            thing.iter().any(|p| p.bucket == "keyword/alias"
                && matches!(&p.expect, Expectation::RenameSites(s) if s.exact.len() == 1)),
            "{thing:?}"
        );
    }

    #[test]
    fn columns_are_utf16_units() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // twice.clj:17 `(defn smile [x] (str "😀" (twin x)))` — `twin` starts
        // 27 UTF-16 units in (26 chars: the emoji is one char, two units).
        let found = at(&probes, "src/twice.clj", 16, 27);
        assert!(
            found.iter().any(|p| p.token == "twin"
                && matches!(&p.expect, Expectation::Definition { line, .. } if *line == 4)),
            "{found:?}"
        );
        // And the same site is expected of the def's references at that column.
        let def = at(&probes, "src/twice.clj", 4, 6);
        let sites = def
            .iter()
            .find_map(|p| match &p.expect {
                Expectation::References(s) => Some(s),
                _ => None,
            })
            .expect("references probe");
        assert!(
            sites.exact.iter().any(|(_, l, c)| (*l, *c) == (16, 27)),
            "{sites:?}"
        );
    }

    #[test]
    fn unjudged_counts_uncovered_tokens_only() {
        let Some(tmp) = with_kondo("unjudged_counts_uncovered_tokens_only") else {
            return;
        };
        let analysis = oracle::run(tmp.path(), &["src"], "").expect("clj-kondo runs");
        let file = tmp.path().join("src/consumer.clj");
        let text = std::fs::read_to_string(&file).unwrap();
        let covered = oracle::covered(&analysis, &file);
        let unjudged = oracle::unjudged(&text, &covered);
        // `helpers/greet` resolves to no namespace, and kondo reports nothing
        // for the `ns` head itself: two tokens no oracle entry vouches for.
        assert_eq!(unjudged, 2);
        assert!(oracle::unjudged(&text, &BTreeSet::new()) > unjudged);
    }
}

mod judge_tests {
    use super::*;

    fn probe(expect: Expectation) -> Probe {
        Probe {
            file: PathBuf::from("/corpus/src/a.clj"),
            line: 3,
            character: 2,
            token: "f".into(),
            bucket: "var-def/defn".into(),
            expect,
        }
    }

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/corpus/src")]
    }

    fn sites(entries: &[(u32, u32)]) -> Sites {
        Sites::from_exact(
            entries
                .iter()
                .map(|(l, c)| (PathBuf::from("/corpus/src/a.clj"), *l, *c))
                .collect(),
        )
    }

    fn locations(entries: &[(u32, u32)]) -> Value {
        Value::Array(
            entries
                .iter()
                .map(|(l, c)| {
                    json!({ "uri": "file:///corpus/src/a.clj", "range": { "start": { "line": l, "character": c }, "end": { "line": l, "character": c + 1 } } })
                })
                .collect(),
        )
    }

    #[test]
    fn a_null_definition_is_a_null_verdict_and_a_new_divergence() {
        let root = Path::new("/corpus");
        let p = probe(Expectation::Definition {
            file: PathBuf::from("/corpus/src/b.clj"),
            line: 7,
        });
        let verdict = judge(&p, &Answer::Result(Value::Null), root, &roots());
        assert!(matches!(verdict, Verdict::Null { .. }), "{verdict:?}");
        let mut report = Report::default();
        report.record(&p, verdict, root);
        assert_eq!(report.new_divergences.len(), 1);
        assert_eq!(report.rows["var-def/defn"].null, 1);
        assert_eq!(report.rows["var-def/defn"].diverge, 0);
    }

    #[test]
    fn a_definition_on_the_right_line_agrees() {
        let root = Path::new("/corpus");
        let p = probe(Expectation::Definition {
            file: PathBuf::from("/corpus/src/a.clj"),
            line: 7,
        });
        assert_eq!(
            judge(&p, &Answer::Result(locations(&[(7, 6)])), root, &roots()),
            Verdict::Agree
        );
        assert!(matches!(
            judge(&p, &Answer::Result(locations(&[(8, 6)])), root, &roots()),
            Verdict::Diverge { .. }
        ));
    }

    #[test]
    fn one_answer_on_a_line_the_oracle_counts_twice_diverges() {
        let root = Path::new("/corpus");
        let p = probe(Expectation::References(sites(&[(3, 2), (9, 4), (9, 12)])));
        let verdict = judge(
            &p,
            &Answer::Result(locations(&[(3, 2), (9, 4)])),
            root,
            &roots(),
        );
        assert!(matches!(verdict, Verdict::Diverge { .. }), "{verdict:?}");
    }

    #[test]
    fn same_counts_with_shifted_columns_is_soft() {
        let root = Path::new("/corpus");
        let p = probe(Expectation::References(sites(&[(3, 2), (9, 4), (9, 12)])));
        let verdict = judge(
            &p,
            &Answer::Result(locations(&[(3, 2), (9, 6), (9, 14)])),
            root,
            &roots(),
        );
        assert_eq!(verdict, Verdict::Soft);
        let mut report = Report::default();
        report.record(&p, verdict, root);
        assert!(report.new_divergences.is_empty());
        assert_eq!(report.rows["var-def/defn"].soft, 1);
    }

    #[test]
    fn rename_edits_read_changes_and_document_changes() {
        let root = Path::new("/corpus");
        let p = probe(Expectation::RenameSites(sites(&[(3, 2), (9, 4)])));
        let changes = json!({ "changes": { "file:///corpus/src/a.clj": [
            { "range": { "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 3 } }, "newText": "g" },
            { "range": { "start": { "line": 9, "character": 4 }, "end": { "line": 9, "character": 5 } }, "newText": "g" }
        ] } });
        assert_eq!(
            judge(&p, &Answer::Edit(changes), root, &roots()),
            Verdict::Agree
        );
        let document_changes = json!({ "documentChanges": [ { "textDocument": { "uri": "file:///corpus/src/a.clj", "version": 1 }, "edits": [
            { "range": { "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 3 } }, "newText": "g" },
            { "range": { "start": { "line": 9, "character": 4 }, "end": { "line": 9, "character": 5 } }, "newText": "g" }
        ] } ] });
        assert_eq!(
            judge(&p, &Answer::Edit(document_changes), root, &roots()),
            Verdict::Agree
        );
        assert!(matches!(
            judge(&p, &Answer::Refused, root, &roots()),
            Verdict::Diverge { .. }
        ));
    }

    #[test]
    fn an_internal_error_on_rename_is_a_harness_error_not_a_refusal() {
        let internal =
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32603, "message": "boom" } });
        assert!(matches!(rename_answer(&internal, None), Answer::Error(_)));
        let invalid = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "cannot rename" } });
        assert!(matches!(rename_answer(&invalid, None), Answer::Refused));
        let prepared = json!({ "jsonrpc": "2.0", "id": 1, "result": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } });
        let null_rename = json!({ "jsonrpc": "2.0", "id": 2, "result": null });
        assert!(matches!(
            rename_answer(&prepared, Some(&null_rename)),
            Answer::Refused
        ));
        let edit = json!({ "jsonrpc": "2.0", "id": 2, "result": { "changes": {} } });
        assert!(matches!(
            rename_answer(&prepared, Some(&edit)),
            Answer::Edit(_)
        ));
        // A refusal of the rename itself, reported with the plan's verdict.
        let p = probe(Expectation::RenameRefused);
        assert_eq!(
            judge(&p, &Answer::Refused, Path::new("/corpus"), &roots()),
            Verdict::Agree
        );
    }

    #[test]
    fn sites_outside_the_linted_roots_are_not_judged() {
        let root = Path::new("/corpus");
        let p = probe(Expectation::References(sites(&[(3, 2)])));
        let mut answer = locations(&[(3, 2)]);
        answer.as_array_mut().unwrap().push(json!({
            "uri": "file:///corpus/analysis/src/tools.clj",
            "range": { "start": { "line": 1, "character": 1 }, "end": { "line": 1, "character": 2 } }
        }));
        assert_eq!(
            judge(&p, &Answer::Result(answer), root, &roots()),
            Verdict::Agree
        );
    }

    #[test]
    fn missing_counts_per_line_so_a_shifted_column_is_present() {
        let root = Path::new("/corpus");
        // Expected: the def and one caller. Got: both lines, the caller at
        // another column, plus an implementation site — a superset by line.
        let p = probe(Expectation::References(sites(&[(3, 2), (9, 4)])));
        let verdict = judge(
            &p,
            &Answer::Result(locations(&[(3, 2), (9, 8), (20, 3)])),
            root,
            &roots(),
        );
        assert!(
            matches!(verdict, Verdict::Diverge { missing: 0, .. }),
            "{verdict:?}"
        );
    }

    #[test]
    fn sampling_keeps_every_request_kind() {
        let mut probes = Vec::new();
        for i in 0..150u32 {
            for expect in [
                Expectation::References(sites(&[(i, 0)])),
                Expectation::RenameSites(sites(&[(i, 0)])),
            ] {
                let mut p = probe(expect);
                p.line = i;
                probes.push(p);
            }
        }
        // 300 probes, limit 200: a stride over the bucket alone would be 2 and
        // keep only the even indices — every references probe, no rename.
        let kept = sample(probes, 200, None);
        let renames = kept
            .iter()
            .filter(|p| matches!(p.expect, Expectation::RenameSites(_)))
            .count();
        let references = kept.len() - renames;
        assert_eq!((references, renames), (150, 150));
        let capped = sample(
            (0..500u32)
                .map(|i| {
                    let mut p = probe(Expectation::References(sites(&[(i, 0)])));
                    p.line = i;
                    p
                })
                .collect(),
            200,
            None,
        );
        assert!(
            capped.len() <= 200 && capped.len() >= 160,
            "{}",
            capped.len()
        );
        assert!(
            capped.iter().any(|p| p.line > 400),
            "spread over the whole range"
        );
    }

    #[test]
    fn known_divergences_are_counted_apart() {
        let root = Path::new("/corpus");
        let mut p = probe(Expectation::RenameSites(sites(&[(3, 2)])));
        p.token = ":app/db".into();
        let verdict = judge(&p, &Answer::Refused, root, &roots());
        let mut report = Report::default();
        report.record(&p, verdict, root);
        assert!(report.new_divergences.is_empty());
        assert_eq!(report.rows["var-def/defn"].known, 1);
        assert_eq!(report.known.len(), 1);
    }
}

const FIXTURE_LINT_AS: &str = "{:lint-as {malli.util/defn clojure.core/defn}}";

/// The whole pipeline on the fixture, with the kill switches on: what keeps
/// the harness working between corpus runs. Library definitions are left out
/// — under `LspClient::start` the fixture has no classpath on CI, and a probe
/// whose answer depends on the host's `.cpcache` is not a fixture test.
#[test]
fn compare_simple_project() {
    let Some(tmp) = with_kondo("compare_simple_project") else {
        return;
    };
    let root = tmp.path().to_path_buf();
    // The `:lint-as` malli ships: without it kondo reads `(mu/defn scale
    // [factor x] …)` as a call and its argv as var usages, which clj-pulse's
    // qualified-head rule never does.
    let analysis = oracle::run(&root, &["src"], FIXTURE_LINT_AS).expect("clj-kondo runs");
    let probes: Vec<Probe> = oracle::probes(&analysis)
        .into_iter()
        .filter(|p| !matches!(p.expect, Expectation::LibraryDefinition { .. }))
        .collect();
    assert!(probes.len() > 50, "{} probes", probes.len());
    let mut session = Session::new(LspClient::start(&root));
    session.client.initialize(&root);
    let roots = vec![root.join("src")];
    let report = ask_and_judge(
        &mut session,
        &probes,
        &analysis,
        &root,
        &roots,
        "simple_project",
    );
    report.print();
    assert!(session.errors.is_empty(), "{:?}", session.errors);
    assert!(
        report.new_divergences.is_empty(),
        "{} new divergences (see the report above)",
        report.new_divergences.len()
    );
}

// ---------------------------------------------------------------------------
// The corpus run
// ---------------------------------------------------------------------------

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Deterministic sampling: probes come sorted by file, line, character; a
/// bucket with more than `limit` probes of one request kind keeps every k-th
/// of them, so the sample spreads over the corpus instead of exhausting the
/// limit on whichever files sort first (a vendored `inlined/` tree would
/// otherwise be the whole library story). The stride runs per bucket *and*
/// request: a var-definition bucket alternates references and rename probes,
/// and an even stride over the bucket alone would keep one kind and drop the
/// other entirely. `files` caps how many files are visited at all, for a
/// first look at a corpus the size of metabase.
fn sample(probes: Vec<Probe>, limit: usize, files: Option<usize>) -> Vec<Probe> {
    let probes: Vec<Probe> = match files {
        Some(cap) => {
            let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
            probes
                .into_iter()
                .filter(|probe| {
                    visited.contains(&probe.file)
                        || (visited.len() < cap && visited.insert(probe.file.clone()))
                })
                .collect()
        }
        None => probes,
    };
    let key = |probe: &Probe| format!("{} {}", probe.bucket, probe.expect.request());
    let mut per_key: BTreeMap<String, usize> = BTreeMap::new();
    for probe in &probes {
        *per_key.entry(key(probe)).or_insert(0) += 1;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut kept: BTreeMap<String, usize> = BTreeMap::new();
    probes
        .into_iter()
        .filter(|probe| {
            let key = key(probe);
            let total = per_key[&key];
            let i = seen.entry(key.clone()).or_insert(0);
            let index = *i;
            *i += 1;
            let stride = total.div_ceil(limit.max(1));
            if !index.is_multiple_of(stride) {
                return false;
            }
            let k = kept.entry(key).or_insert(0);
            if *k >= limit {
                return false;
            }
            *k += 1;
            true
        })
        .collect()
}

/// clj-pulse under production settings on a pinned real corpus, asked every
/// question its clj-kondo analysis has an answer to. Advisory by default —
/// the report is the product — and a gate on the server dying, a panic, an
/// unexpected JSON-RPC error, or kondo not running; `CLJ_PULSE_COMPARE_STRICT`
/// makes any new divergence fail it too.
#[test]
#[ignore = "needs CLJ_PULSE_COMPARE_ROOT pointing at a pinned corpus; run with `bb compare`"]
fn compare_corpus() {
    let Some(root) = std::env::var_os("CLJ_PULSE_COMPARE_ROOT") else {
        println!("CLJ_PULSE_COMPARE_ROOT is unset — skipping. Run `bb compare`.");
        return;
    };
    let root = PathBuf::from(root)
        .canonicalize()
        .expect("CLJ_PULSE_COMPARE_ROOT does not exist");
    let corpus = std::env::var("CLJ_PULSE_COMPARE_CORPUS").unwrap_or_else(|_| "(unnamed)".into());
    let limit: usize = env_parse("CLJ_PULSE_COMPARE_LIMIT").unwrap_or(200);
    let files: Option<usize> = env_parse("CLJ_PULSE_COMPARE_FILES");
    let strict = std::env::var_os("CLJ_PULSE_COMPARE_STRICT").is_some_and(|v| !v.is_empty());

    let version = oracle::kondo_available().expect("clj-kondo on PATH");
    println!("clj-kondo: {version}");
    if !version.contains(oracle::PINNED_KONDO) {
        println!(
            "WARNING: not the pinned clj-kondo {} — expectations were checked against that one",
            oracle::PINNED_KONDO
        );
    }
    let started = Instant::now();
    // What clj-pulse indexes as the root project's own source (deps.edn
    // `:paths`, alias `:extra-paths`, `src`/`test`), and nothing else: the
    // oracle reads what the server reads. clj-kondo's `corpus/` of deliberately
    // broken samples stays out on both sides.
    let roots: Vec<PathBuf> = clj_pulse::config::source_paths(&root)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();
    let paths: Vec<String> = roots
        .iter()
        .map(|p| {
            p.strip_prefix(&root)
                .unwrap_or(p)
                .to_string_lossy()
                .to_string()
        })
        .collect();
    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    println!("linting {}", paths.join(" "));
    let analysis = oracle::run(&root, &path_refs, "").expect("clj-kondo runs");
    let all = oracle::probes(&analysis);
    let probes = sample(all, limit, files);
    println!(
        "analysis in {:?}: {} var-definitions, {} var-usages, {} locals, {} keywords; {} probes after sampling (limit {limit} per bucket and request{})",
        started.elapsed(),
        analysis.var_definitions.len(),
        analysis.var_usages.len(),
        analysis.locals.len(),
        analysis.keywords.len(),
        probes.len(),
        files.map(|f| format!(", {f} files")).unwrap_or_default()
    );

    let mut session = Session::production(&root);
    let started = Instant::now();
    session.client.initialize_no_wait(&root);
    let note = settle(&mut session, Instant::now() + REQUEST_TIMEOUT);
    println!("server settled in {:?} ({note})", started.elapsed());

    let started = Instant::now();
    let report = ask_and_judge(&mut session, &probes, &analysis, &root, &roots, &corpus);
    println!("asked in {:?}", started.elapsed());
    report.print();

    let mut failures: Vec<String> = Vec::new();
    if !session.alive() {
        failures.push("the server died".into());
    }
    let panics: Vec<String> = LspClient::server_log(&root)
        .lines()
        .filter(|line| line.contains("panicked at"))
        .map(|line| line.trim().to_string())
        .collect();
    if !panics.is_empty() {
        failures.push(format!(
            "{} panic(s) in server.log: {:#?}",
            panics.len(),
            panics
        ));
    }
    if !session.errors.is_empty() {
        failures.push(format!(
            "{} unexpected JSON-RPC error(s): {:#?}",
            session.errors.len(),
            session.errors
        ));
    }
    if strict && !report.new_divergences.is_empty() {
        failures.push(format!(
            "{} new divergence(s) under CLJ_PULSE_COMPARE_STRICT",
            report.new_divergences.len()
        ));
    }
    assert!(
        failures.is_empty(),
        "compare failed:\n{}",
        failures.join("\n")
    );
}
