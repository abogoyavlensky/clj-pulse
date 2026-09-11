//! A soak run: one long-lived server driven through many rounds of realistic
//! file churn on a pinned real corpus, and checked at every checkpoint against
//! a server freshly started on the same on-disk state.
//!
//! `bb check` proves the server correct on fixtures that never move under it,
//! and `bb bench` proves it fast for the first two minutes of a session.
//! Neither sees what a working day does to it: hundreds of edits, saves, file
//! creations and deletions, a branch switch, all against one process. The
//! failures that live there — an index that drifts after churn, memory that
//! never comes back, a handler that stops answering — are what this measures.
//!
//! Drive it with `bb soak`; the driver itself is `#[ignore]`d and skipped
//! without `CLJ_PULSE_SOAK_ROOT`. The corpus is churned in place, so
//! [`CorpusGuard`] restores it on the way out, panic or not.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::sampling::{
    has_children, median, quiet_for, rss_kib, QUIET, STAGE2_LINES, STAGE3_ANNOUNCE_GRACE,
    STAGE3_LINES,
};
use common::sites::{answers, definition, is_source_ish, project_sites, source_files, Site};
use common::{LspClient, FILE_CHANGED, FILE_CREATED, FILE_DELETED};

// ---------------------------------------------------------------------------
// The seeded generator
// ---------------------------------------------------------------------------

/// A linear congruential generator, so a run replays exactly from its seed.
/// Written out rather than pulled in: one function does not justify a
/// dependency in the tree.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        // An all-zero state would stick; the odd constant also spreads a seed
        // of 0, 1, 2 — the ones a human types — across the whole word.
        Lcg(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// The *high* half of the next state: the low bits of an LCG cycle far too
    /// regularly to draw a small index from.
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 32
    }

    /// A number in `0..n`; 0 when `n` is 0, so a caller with nothing to pick
    /// from does not have to special-case the draw.
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// A random `u64` for the default seed. `SystemTime` alone repeats on a fast
/// machine; the address of a heap allocation is what ASLR makes different per
/// process.
fn random_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let boxed = Box::new(0u8);
    let address = Box::into_raw(boxed) as u64;
    // SAFETY: the pointer came from `Box::into_raw` one line above.
    unsafe { drop(Box::from_raw(address as *mut u8)) };
    nanos ^ address.rotate_left(17)
}

// ---------------------------------------------------------------------------
// What a round does, and what it promises
// ---------------------------------------------------------------------------

/// One churn action. The `OnDisk`, `Create`, `Delete` and `Rename` variants are
/// paired with a `didChangeWatchedFiles` notification; `Buffer` never touches
/// disk.
#[derive(Clone, Debug)]
enum Edit {
    /// Type at the end of an open buffer: didChange only, no disk write.
    Buffer { file: PathBuf, var: String },
    /// Write the open buffer's current text to disk, then didSave.
    Save { file: PathBuf },
    /// Rewrite a tracked corpus file on disk (append a form), notify Changed.
    ModifyOnDisk { file: PathBuf, var: String },
    /// Write a new namespace file the soak owns, notify Created.
    Create {
        file: PathBuf,
        ns: String,
        var: String,
    },
    /// Remove a file the soak created, notify Deleted.
    Delete { file: PathBuf },
    /// Delete + Create with the ns renamed to match the new path.
    Rename {
        from: PathBuf,
        to: PathBuf,
        ns: String,
    },
}

impl Edit {
    fn describe(&self) -> String {
        match self {
            Edit::Buffer { file, .. } => format!("buffer edit {}", file.display()),
            Edit::Save { file } => format!("save {}", file.display()),
            Edit::ModifyOnDisk { file, .. } => format!("modify on disk {}", file.display()),
            Edit::Create { file, .. } => format!("create {}", file.display()),
            Edit::Delete { file } => format!("delete {}", file.display()),
            Edit::Rename { from, to, .. } => {
                format!("rename {} -> {}", from.display(), to.display())
            }
        }
    }
}

/// Where a witness var has to be once the server has caught up.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    /// Defined exactly once, in this file.
    At(PathBuf),
    /// Defined nowhere.
    Gone,
}

/// The observable consequence of one churn action: a uniquely named var whose
/// `workspace/symbol` answer must change because of it. `workspace/symbol` is
/// the vehicle because it reads the index alone — no document has to be open —
/// and matches on the symbol's name.
#[derive(Clone, Debug)]
struct Witness {
    name: String,
    expect: Expect,
    /// What produced it, so a batch that never lands names the action.
    action: String,
}

impl Witness {
    /// `Ok(())` when the index agrees, `Err(why)` when it does not — a plain
    /// description, because a witness that has not landed yet and one that
    /// landed wrong read the same until the deadline passes.
    fn check(&self, session: &mut Session) -> Result<(), String> {
        // The query is fuzzy (`workspace/symbol` matches by subsequence too),
        // so `soak-witness-…-7` also matches `soak-witness-…-17`. Only an
        // exact name counts, and the exact tier sorts first, so the 128-result
        // cap can never drop it.
        let answer = session.request("workspace/symbol", json!({ "query": self.name }));
        let hits: Vec<String> = answer
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item["name"] == self.name.as_str())
                    .map(|item| item["location"]["uri"].as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default();
        match &self.expect {
            Expect::Gone if hits.is_empty() => Ok(()),
            Expect::Gone => Err(format!("{} is still defined in {:?}", self.name, hits)),
            Expect::At(path) => {
                let want = format!("file://{}", path.display());
                match hits.as_slice() {
                    [uri] if *uri == want => Ok(()),
                    [] => Err(format!("{} is not defined anywhere", self.name)),
                    others => Err(format!(
                        "{} is defined in {:?}, want {}",
                        self.name, others, want
                    )),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The churn
// ---------------------------------------------------------------------------

/// An open buffer, as the client knows it: the server's copy is whatever the
/// notifications have built, and the two must not be allowed to drift.
struct Buffer {
    text: String,
    version: i64,
    /// Vars appended since the last save — not in the index until one lands.
    pending: Vec<String>,
}

/// A file the soak wrote, and the var that proves it is indexed.
struct Created {
    path: PathBuf,
    var: String,
}

/// Draws and applies rounds of churn against one corpus checkout.
struct Churn {
    /// `<root>/src/soak_<seed>` — everything the soak creates lives here, so
    /// the guard can remove the lot with one `remove_dir_all`.
    soak_dir: PathBuf,
    tag: String,
    /// Corpus files the startup scan already indexed: only these are churned,
    /// so no round makes the churned server index a file a fresh one would not.
    tracked: Vec<PathBuf>,
    created: Vec<Created>,
    open: BTreeMap<PathBuf, Buffer>,
    /// Tracked files this run has written to; `git checkout -- .` restores
    /// them, and the checkpoint has to tell the server about every one.
    dirtied: BTreeSet<PathBuf>,
    /// Tracked files whose disk copy the churn owns. Never opened as buffers:
    /// a later save would write the buffer over the appended form and retract
    /// a witness the server rightly indexed.
    disk_owned: BTreeSet<PathBuf>,
    /// Every witness var written into a *tracked* file, with the file it went
    /// into: the restore reverts those files, so each one has to disappear
    /// again before the reference server is asked anything.
    saved_vars: Vec<(String, PathBuf)>,
    rng: Lcg,
    counter: usize,
}

impl Churn {
    fn new(root: &Path, seed: u64) -> Self {
        let tag = format!("{seed:x}");
        Self {
            soak_dir: root.join("src").join(format!("soak_{tag}")),
            tag,
            tracked: churn_candidates(root),
            created: Vec::new(),
            open: BTreeMap::new(),
            dirtied: BTreeSet::new(),
            disk_owned: BTreeSet::new(),
            saved_vars: Vec::new(),
            rng: Lcg::new(seed),
            counter: 0,
        }
    }

    /// A round's worth of edits. An ordinary round draws a mix; a bulk round is
    /// the shape a branch switch has — many files, one notification.
    fn draw(&mut self, round: usize, count: usize, bulk: bool) -> Vec<Edit> {
        // The projection of the state each drawn edit leaves behind, so a
        // `Save` is never drawn for a file the round has not opened yet and a
        // `Delete` never for a file it has not created.
        let mut open: BTreeSet<PathBuf> = self.open.keys().cloned().collect();
        let mut created: Vec<PathBuf> = self.created.iter().map(|c| c.path.clone()).collect();
        let mut disk_owned = self.disk_owned.clone();
        let mut edits = Vec::with_capacity(count);
        for _ in 0..count {
            let roll = if bulk {
                // A branch switch is files arriving and files appearing, not
                // typing: 3 modifications to every creation.
                if self.rng.below(4) == 0 {
                    85
                } else {
                    60
                }
            } else {
                self.rng.below(100)
            };
            let edit = match roll {
                0..=34 => self.draw_buffer(&disk_owned, &mut open),
                35..=54 => self.draw_save(&open),
                55..=79 => self.draw_modify(&open, &mut disk_owned),
                80..=89 => Some(self.draw_create(round, &mut created)),
                90..=94 => self.draw_delete(&mut created),
                _ => self.draw_rename(round, &mut created),
            };
            // A draw with nothing to draw from (no open buffer yet, nothing
            // created yet) falls back to a creation, which always applies.
            let edit = edit.unwrap_or_else(|| self.draw_create(round, &mut created));
            edits.push(edit);
        }
        edits
    }

    fn draw_buffer(
        &mut self,
        disk_owned: &BTreeSet<PathBuf>,
        open: &mut BTreeSet<PathBuf>,
    ) -> Option<Edit> {
        let file = self.pick_tracked(|p| !disk_owned.contains(p))?;
        open.insert(file.clone());
        let var = self.next_var(0);
        Some(Edit::Buffer { file, var })
    }

    fn draw_save(&mut self, open: &BTreeSet<PathBuf>) -> Option<Edit> {
        let files: Vec<&PathBuf> = open.iter().collect();
        let file = files.get(self.rng.below(files.len()))?;
        Some(Edit::Save {
            file: (*file).clone(),
        })
    }

    fn draw_modify(
        &mut self,
        open: &BTreeSet<PathBuf>,
        disk_owned: &mut BTreeSet<PathBuf>,
    ) -> Option<Edit> {
        let file = self.pick_tracked(|p| !open.contains(p))?;
        disk_owned.insert(file.clone());
        let var = self.next_var(0);
        Some(Edit::ModifyOnDisk { file, var })
    }

    fn draw_create(&mut self, round: usize, created: &mut Vec<PathBuf>) -> Edit {
        let n = self.counter;
        let var = self.next_var(round);
        let file = self.soak_dir.join(format!("f{n}.clj"));
        created.push(file.clone());
        Edit::Create {
            file,
            ns: format!("soak-{}.f{n}", self.tag),
            var,
        }
    }

    fn draw_delete(&mut self, created: &mut Vec<PathBuf>) -> Option<Edit> {
        if created.is_empty() {
            return None;
        }
        let at = self.rng.below(created.len());
        Some(Edit::Delete {
            file: created.remove(at),
        })
    }

    fn draw_rename(&mut self, round: usize, created: &mut Vec<PathBuf>) -> Option<Edit> {
        if created.is_empty() {
            return None;
        }
        let at = self.rng.below(created.len());
        let from = created.remove(at);
        let n = self.counter;
        self.counter += 1;
        let _ = round;
        let to = self.soak_dir.join(format!("f{n}.clj"));
        created.push(to.clone());
        Some(Edit::Rename {
            from,
            to,
            ns: format!("soak-{}.f{n}", self.tag),
        })
    }

    /// A tracked file matching `ok`, drawn from a rotating start so a long run
    /// does not keep churning the same handful.
    fn pick_tracked(&mut self, ok: impl Fn(&PathBuf) -> bool) -> Option<PathBuf> {
        if self.tracked.is_empty() {
            return None;
        }
        let start = self.rng.below(self.tracked.len());
        (0..self.tracked.len())
            .map(|i| &self.tracked[(start + i) % self.tracked.len()])
            .find(|p| ok(p))
            .cloned()
    }

    /// The next witness name. Unique across the run — the counter never
    /// repeats — and unmistakable in a `workspace/symbol` answer.
    fn next_var(&mut self, round: usize) -> String {
        let n = self.counter;
        self.counter += 1;
        format!("soak-witness-{}-{round}-{n}", self.tag)
    }

    /// Applies `edits` in order, sending each buffer notification as it goes
    /// and every watched-file change as *one* notification at the end — a
    /// branch switch arrives as one, and file-by-file delivery would measure a
    /// shape no editor produces.
    fn apply(&mut self, client: &mut LspClient, edits: &[Edit]) -> Vec<Witness> {
        let mut witnesses = Vec::new();
        let mut watched: Vec<(PathBuf, u8)> = Vec::new();
        for edit in edits {
            match edit {
                Edit::Buffer { file, var } => {
                    if self.open_buffer(client, file).is_none() {
                        continue;
                    }
                    let form = witness_form(var);
                    let buffer = self.open.get_mut(file).expect("just opened");
                    let end = end_position(&buffer.text);
                    buffer.version += 1;
                    let version = buffer.version;
                    buffer.text.push_str(&form);
                    buffer.pending.push(var.clone());
                    client.did_change_range(file, version, end, end, &form);
                }
                Edit::Save { file } => {
                    let Some(buffer) = self.open.get_mut(file) else {
                        continue;
                    };
                    if std::fs::write(file, &buffer.text).is_err() {
                        continue;
                    }
                    self.dirtied.insert(file.clone());
                    let saved: Vec<String> = buffer.pending.drain(..).collect();
                    for var in saved {
                        witnesses.push(Witness {
                            name: var.clone(),
                            expect: Expect::At(file.clone()),
                            action: edit.describe(),
                        });
                        self.saved_vars.push((var, file.clone()));
                    }
                    client.did_save(file);
                }
                Edit::ModifyOnDisk { file, var } => {
                    let Ok(text) = std::fs::read_to_string(file) else {
                        continue;
                    };
                    // Only ever an append: a form added after the last one
                    // cannot corrupt a file, so a parse failure is never the
                    // churn generator's doing.
                    if std::fs::write(file, format!("{text}{}", witness_form(var))).is_err() {
                        continue;
                    }
                    self.dirtied.insert(file.clone());
                    self.disk_owned.insert(file.clone());
                    self.saved_vars.push((var.clone(), file.clone()));
                    watched.push((file.clone(), FILE_CHANGED));
                    witnesses.push(Witness {
                        name: var.clone(),
                        expect: Expect::At(file.clone()),
                        action: edit.describe(),
                    });
                }
                Edit::Create { file, ns, var } => {
                    if !self.write_created(file, ns, var) {
                        continue;
                    }
                    watched.push((file.clone(), FILE_CREATED));
                    witnesses.push(Witness {
                        name: var.clone(),
                        expect: Expect::At(file.clone()),
                        action: edit.describe(),
                    });
                }
                Edit::Delete { file } => {
                    let Some(at) = self.created.iter().position(|c| c.path == *file) else {
                        continue;
                    };
                    if std::fs::remove_file(file).is_err() {
                        continue;
                    }
                    let gone = self.created.remove(at);
                    watched.push((file.clone(), FILE_DELETED));
                    witnesses.push(Witness {
                        name: gone.var,
                        expect: Expect::Gone,
                        action: edit.describe(),
                    });
                }
                Edit::Rename { from, to, ns } => {
                    let Some(at) = self.created.iter().position(|c| c.path == *from) else {
                        continue;
                    };
                    let var = self.created[at].var.clone();
                    if std::fs::remove_file(from).is_err() || !self.write_created(to, ns, &var) {
                        continue;
                    }
                    self.created.remove(at);
                    watched.push((from.clone(), FILE_DELETED));
                    watched.push((to.clone(), FILE_CREATED));
                    // One witness, not two: "defined exactly once, at the new
                    // path" already fails on an entry left behind at the old.
                    witnesses.push(Witness {
                        name: var,
                        expect: Expect::At(to.clone()),
                        action: edit.describe(),
                    });
                }
            }
        }
        notify_watched(client, &watched);
        last_word(witnesses)
    }

    /// Opens `file` as a buffer if it is not open already. `None` when the file
    /// cannot be read, so the caller drops that edit rather than inventing one.
    fn open_buffer(&mut self, client: &mut LspClient, file: &Path) -> Option<()> {
        if self.open.contains_key(file) {
            return Some(());
        }
        let text = std::fs::read_to_string(file).ok()?;
        client.did_open(file);
        self.open.insert(
            file.to_path_buf(),
            Buffer {
                text,
                version: 1,
                pending: Vec::new(),
            },
        );
        Some(())
    }

    fn write_created(&mut self, file: &Path, ns: &str, var: &str) -> bool {
        if std::fs::create_dir_all(&self.soak_dir).is_err() {
            return false;
        }
        let text = format!("(ns {ns})\n{}", witness_form(var));
        if std::fs::write(file, text).is_err() {
            return false;
        }
        self.created.push(Created {
            path: file.to_path_buf(),
            var: var.to_string(),
        });
        true
    }

    fn open_files(&self) -> Vec<PathBuf> {
        self.open.keys().cloned().collect()
    }

    /// The watched-file changes that describe a restore to baseline: every
    /// tracked file written back, every file the soak created gone.
    fn restore_changes(&self) -> Vec<(PathBuf, u8)> {
        let mut changes: Vec<(PathBuf, u8)> = self
            .dirtied
            .iter()
            .map(|p| (p.clone(), FILE_CHANGED))
            .collect();
        changes.extend(self.created.iter().map(|c| (c.path.clone(), FILE_DELETED)));
        changes
    }

    /// What the index must say once the restore has landed: every var the run
    /// appended is gone again. The baseline is the state the reference server
    /// will be started on, so this is the check that makes the comparison fair.
    fn restore_witnesses(&self) -> Vec<Witness> {
        let mut witnesses: Vec<Witness> = self
            .created
            .iter()
            .map(|c| Witness {
                name: c.var.clone(),
                expect: Expect::Gone,
                action: format!("restore removes {}", c.path.display()),
            })
            .collect();
        witnesses.extend(self.saved_vars.iter().map(|(var, path)| Witness {
            name: var.clone(),
            expect: Expect::Gone,
            action: format!("restore reverts {}", path.display()),
        }));
        witnesses
    }

    /// Forgets everything the restore undid: buffers are closed, created files
    /// are gone, tracked files are back at their pinned contents.
    fn reset_to_baseline(&mut self) {
        self.open.clear();
        self.created.clear();
        self.dirtied.clear();
        self.disk_owned.clear();
        self.saved_vars.clear();
    }
}

/// The end of a buffer, in the units the protocol counts: a line index and a
/// UTF-16 column. `lines().count()` is the end only when the text finishes with
/// a newline — the pinned clj-kondo corpus has files that do not, and a change
/// starting past the end is rejected outright, leaving the server's copy of the
/// buffer behind the client's for the rest of the run.
fn end_position(text: &str) -> (u32, u32) {
    // `split` rather than `lines`: it keeps the empty final line a trailing
    // newline leaves behind, which is exactly the position wanted there.
    let last = text.split('\n').next_back().unwrap_or_default();
    let line = text.split('\n').count().saturating_sub(1) as u32;
    (line, last.encode_utf16().count() as u32)
}

/// One witness per var, the last action's. A round can create a file and then
/// rename or delete it, and a `Delete` reuses the var name its `Create` wrote:
/// only the final state of the round is something the server can be held to.
fn last_word(witnesses: Vec<Witness>) -> Vec<Witness> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut kept: Vec<Witness> = witnesses
        .into_iter()
        .rev()
        .filter(|w| seen.insert(w.name.clone()))
        .collect();
    kept.reverse();
    kept
}

/// The form every churn action appends: one top-level def whose name exists
/// nowhere else, so a `workspace/symbol` query for it answers about that
/// action alone.
fn witness_form(var: &str) -> String {
    format!("\n(defn {var} [] nil)\n")
}

fn notify_watched(client: &mut LspClient, changes: &[(PathBuf, u8)]) {
    if changes.is_empty() {
        return;
    }
    let borrowed: Vec<(&Path, u8)> = changes.iter().map(|(p, t)| (p.as_path(), *t)).collect();
    client.did_change_watched_files(&borrowed);
}

/// The corpus files a round may churn: `.clj`/`.cljc` under the corpus's own
/// top-level `src`, which both pinned corpora carry in their deps.edn
/// `:paths`. The restriction matters for the oracle, not for realism — a file
/// outside `:paths` is indexed when it is *opened*, so churning one would
/// leave the churned server holding symbols a freshly started reference server
/// has never seen, and every checkpoint would report that as a divergence.
fn churn_candidates(root: &Path) -> Vec<PathBuf> {
    let src = root.join("src");
    let mut out = Vec::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "clj" || e == "cljc") {
                out.push(path);
            }
        }
    }
    // Deterministic: the same seed has to draw the same files on every run.
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Putting the corpus back
// ---------------------------------------------------------------------------

/// Restores the churned checkout, on drop as well as on demand — a failed
/// assertion must not leave the corpus dirty for the next run.
///
/// `git checkout -- .` and not `git clean -fd`: the caches the bench and the
/// server leave behind (`.clj-pulse/jar-cache`, `.lsp/`) are untracked in at
/// least one corpus, and re-earning them costs minutes.
struct CorpusGuard {
    root: PathBuf,
    soak_dir: PathBuf,
    head: Option<String>,
}

impl CorpusGuard {
    fn new(root: &Path, soak_dir: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            soak_dir: soak_dir.to_path_buf(),
            head: git(root, &["rev-parse", "HEAD"]),
        }
    }

    fn restore(&self) {
        if self.soak_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&self.soak_dir) {
                println!("soak: could not remove {}: {e}", self.soak_dir.display());
            }
        }
        if git(&self.root, &["checkout", "--", "."]).is_none() {
            println!(
                "soak: `git checkout -- .` failed in {}",
                self.root.display()
            );
        }
        let head = git(&self.root, &["rev-parse", "HEAD"]);
        if head != self.head {
            println!(
                "soak: HEAD moved during the run ({:?} -> {:?})",
                self.head, head
            );
        }
    }
}

impl Drop for CorpusGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Trimmed stdout of a git command, or `None` when it failed.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// How the run is configured
// ---------------------------------------------------------------------------

/// Everything the run reads from the environment, read once and echoed in the
/// report header so a run is reproducible from its own output.
struct Settings {
    root: PathBuf,
    corpus: String,
    seed: u64,
    rounds: usize,
    files: usize,
    bulk_every: usize,
    bulk_files: usize,
    checkpoint_every: usize,
    converge_timeout: Duration,
    rss_growth: f64,
    probes: usize,
}

impl Settings {
    fn from_env(root: PathBuf, corpus: String) -> Self {
        Self {
            seed: env_parse("CLJ_PULSE_SOAK_SEED").unwrap_or_else(random_seed),
            rounds: env_parse("CLJ_PULSE_SOAK_ROUNDS").unwrap_or(20),
            files: env_parse("CLJ_PULSE_SOAK_FILES").unwrap_or(10),
            bulk_every: env_parse("CLJ_PULSE_SOAK_BULK_EVERY").unwrap_or(5),
            bulk_files: env_parse("CLJ_PULSE_SOAK_BULK_FILES").unwrap_or(100),
            // A metabase checkpoint costs about a minute, nearly all of it the
            // reference server settling, so it is taken half as often there.
            checkpoint_every: env_parse("CLJ_PULSE_SOAK_CHECKPOINT_EVERY")
                .unwrap_or(if corpus == "metabase" { 10 } else { 5 }),
            converge_timeout: Duration::from_secs(
                env_parse("CLJ_PULSE_SOAK_CONVERGE_TIMEOUT").unwrap_or(30),
            ),
            rss_growth: env_parse("CLJ_PULSE_SOAK_RSS_GROWTH").unwrap_or(1.5),
            probes: env_parse("CLJ_PULSE_SOAK_PROBES").unwrap_or(12),
            root,
            corpus,
        }
    }

    fn is_checkpoint(&self, round: usize) -> bool {
        round == self.rounds || round.is_multiple_of(self.checkpoint_every.max(1))
    }

    fn is_bulk(&self, round: usize) -> bool {
        self.bulk_every > 0 && round.is_multiple_of(self.bulk_every)
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// One server, and everything that has gone wrong with it
// ---------------------------------------------------------------------------

/// A client plus the liveness record for the server behind it. Every request
/// goes through here: a JSON-RPC error answer is a liveness failure — the panic
/// guard answers a panicked handler with exactly one — so it is recorded rather
/// than merely returned. A request that never answers panics inside the client
/// and takes the run down with it, which is the same verdict by a louder route.
struct Session {
    client: LspClient,
    errors: Vec<String>,
}

impl Session {
    fn new(client: LspClient) -> Self {
        Self {
            client,
            errors: Vec::new(),
        }
    }

    /// A server on `root` under production settings — stage 3 runs and
    /// clj-kondo is spawned when installed, because that is what the machine
    /// under a real editor does.
    fn production(root: &Path) -> Self {
        Self::new(LspClient::start_production(root).with_request_timeout(REQUEST_TIMEOUT))
    }

    fn client(&mut self) -> &mut LspClient {
        &mut self.client
    }

    fn pid(&self) -> u32 {
        self.client.child.id()
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let msg = self.client.request_full(method, params.clone());
        if let Some(error) = msg.get("error") {
            self.errors
                .push(format!("{method} answered {error}: {params}"));
        }
        msg["result"].clone()
    }

    /// Whether the server process is still running. A handler that panics is
    /// answered by the guard, so a dead process here means something worse.
    fn alive(&mut self) -> bool {
        matches!(self.client.child.try_wait(), Ok(None))
    }
}

/// Generous, like the bench's: the point is to find divergence, not to fail on
/// a slow machine.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Definitions timed per checkpoint.
const SAMPLES: usize = 20;
/// How often a convergence poll re-asks.
const POLL: Duration = Duration::from_millis(100);

/// Settles a server the way the bench does: stage 2 (or stage 3, when it is
/// coming) has reported, no child process is still working, and nothing has
/// been logged for [`QUIET`]. Waiting for `Indexed` alone would compare a
/// settled server against one still indexing its libraries, and invent
/// divergences that are nothing but a race.
fn settle(session: &mut Session, deadline: Instant) -> String {
    let terminal: Vec<&str> = STAGE2_LINES
        .iter()
        .chain(STAGE3_LINES.iter())
        .copied()
        .collect();
    let remaining = |deadline: Instant| deadline.saturating_duration_since(Instant::now());
    let stage = session
        .client
        .log_line_within(&terminal, remaining(deadline));
    let mut note = match &stage {
        Some(line) => line.trim_start_matches("clj-pulse: ").to_string(),
        None => "no library stage reported".to_string(),
    };
    let stage3 = stage
        .as_deref()
        .is_some_and(|l| STAGE3_LINES.iter().any(|s| l.contains(s)));
    if !stage3
        && session
            .client
            .log_line_within(&["resolving classpath via"], STAGE3_ANNOUNCE_GRACE)
            .is_some()
    {
        match session
            .client
            .log_line_within(&STAGE3_LINES, remaining(deadline))
        {
            Some(line) => note = line.trim_start_matches("clj-pulse: ").to_string(),
            None => note.push_str("; stage 3 announced but never reported"),
        }
    }
    let pid = session.pid();
    loop {
        session.client.clear_notifications();
        let quiet = quiet_for(&mut session.client, &["window/logMessage"], QUIET);
        if quiet && !has_children(pid) {
            return note;
        }
        if Instant::now() >= deadline {
            return format!("{note}; never settled");
        }
    }
}

// ---------------------------------------------------------------------------
// The probe set
// ---------------------------------------------------------------------------

/// A `workspace/symbol` answer this close to the server's 128-result cap is a
/// truncated list, and two servers are not obliged to truncate the same one.
const QUERY_CAP: usize = 100;

/// The fixed set of questions both servers are asked at every checkpoint. The
/// same sites carry definition, references, completion and hover: a site the
/// rules picked is one a server can answer, and re-using it keeps the number of
/// files the comparison has to open down to what a checkpoint can afford.
struct Probes {
    sites: Vec<Site>,
    files: Vec<PathBuf>,
    queries: Vec<String>,
    /// Set once the reference server has been asked how big each query's answer
    /// is; queries near the cap are dropped, not compared.
    trimmed: bool,
}

impl Probes {
    fn discover(root: &Path, want: usize) -> Self {
        let mut files = source_files(root);
        // Deterministic: size descending, path ascending, the bench's order.
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|(_, p)| p.clone())
            .filter(|p| is_source_ish(p, root))
            .collect();

        // From the smallest qualifying files up: a checkpoint opens every probe
        // file on both servers, and the corpus's 450 KiB outlier would cost
        // more than it tells us. Three sites per file, so one file with an
        // unusual shape cannot dominate the set.
        let mut sites: Vec<Site> = Vec::new();
        for (_, path) in files.iter().rev() {
            if sites.len() >= want {
                break;
            }
            if !is_source_ish(path, root) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            sites.extend(project_sites(&text, path, root, &paths, 3));
        }
        sites.truncate(want);

        let mut files: Vec<PathBuf> = Vec::new();
        for site in &sites {
            if !files.contains(&site.file) {
                files.push(site.file.clone());
            }
        }
        // The name half of each probed token: a real project name, so the query
        // exercises the index the way Cmd+T does, and short enough that the
        // fuzzy tiers below exact are part of what is compared.
        let mut queries: Vec<String> = sites
            .iter()
            .filter_map(|s| s.token.split_once('/').map(|(_, name)| name.to_string()))
            .collect();
        queries.sort();
        queries.dedup();

        Self {
            sites,
            files,
            queries,
            trimmed: false,
        }
    }

    /// Drops the queries whose answers sit near the result cap, asking the
    /// reference server — the one server in the run that is by definition
    /// right. Done once: the corpus is back at its baseline at every
    /// checkpoint, so the counts do not move.
    fn trim_queries(&mut self, reference: &mut Session) {
        if self.trimmed {
            return;
        }
        self.trimmed = true;
        let mut kept = Vec::new();
        for query in std::mem::take(&mut self.queries) {
            let answer = reference.request("workspace/symbol", json!({ "query": query }));
            let count = answer.as_array().map(|a| a.len()).unwrap_or(0);
            if count < QUERY_CAP {
                kept.push(query);
            } else {
                println!("  probe query {query:?} dropped: {count} results, too near the cap");
            }
        }
        self.queries = kept;
    }

    fn print(&self, root: &Path) {
        let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
        println!();
        println!("soak probes");
        println!("  {:<22} {}", "definition sites", self.sites.len());
        for site in &self.sites {
            println!(
                "    {}:{} `{}` -> {}",
                rel(&site.file),
                site.line + 1,
                site.token,
                site.expect.describe()
            );
        }
        println!("  {:<22} {}", "files opened", self.files.len());
        println!("  {:<22} {}", "symbol queries", self.queries.join(", "));
    }
}

// ---------------------------------------------------------------------------
// Asking, and comparing
// ---------------------------------------------------------------------------

/// Everything one server answered at one checkpoint.
struct Answers {
    definition: Vec<Value>,
    references: Vec<Value>,
    document_symbol: Vec<Value>,
    workspace_symbol: Vec<Value>,
    /// Definition probes that landed where the site says they should. A probe
    /// set that resolves nothing compares two `null`s and passes while testing
    /// nothing, so this is reported and checked, not merely collected.
    resolved: usize,
}

fn ask_all(session: &mut Session, probes: &Probes) -> Answers {
    let mut out = Answers {
        definition: Vec::new(),
        references: Vec::new(),
        document_symbol: Vec::new(),
        workspace_symbol: Vec::new(),
        resolved: 0,
    };
    for site in &probes.sites {
        let at = json!({
            "textDocument": { "uri": format!("file://{}", site.file.display()) },
            "position": { "line": site.line, "character": site.character }
        });
        let definition = session.request("textDocument/definition", at.clone());
        if answers(&definition, &site.expect) {
            out.resolved += 1;
        }
        out.definition.push(definition);
        let mut references = at.clone();
        references["context"] = json!({ "includeDeclaration": true });
        out.references
            .push(session.request("textDocument/references", references));
        // Completion and hover are liveness only: completion's ranking reads
        // occurrence counts and the response is explicitly `isIncomplete`, so
        // equality on it would flake for reasons that are not bugs.
        session.request("textDocument/completion", at.clone());
        session.request("textDocument/hover", at);
    }
    for file in &probes.files {
        out.document_symbol.push(session.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": format!("file://{}", file.display()) } }),
        ));
    }
    for query in &probes.queries {
        out.workspace_symbol
            .push(session.request("workspace/symbol", json!({ "query": query })));
    }
    out
}

/// One place the churned server and a fresh one disagree.
struct Divergence {
    request: String,
    site: String,
    churned: String,
    reference: String,
}

impl Divergence {
    fn print(&self) {
        println!("  {} at {}", self.request, self.site);
        println!("    churned:   {}", self.churned);
        println!("    reference: {}", self.reference);
    }
}

fn compare(
    churned: &Answers,
    reference: &Answers,
    probes: &Probes,
    root: &Path,
) -> Vec<Divergence> {
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let mut out = Vec::new();
    for (i, site) in probes.sites.iter().enumerate() {
        let where_ = format!("{}:{} `{}`", rel(&site.file), site.line + 1, site.token);
        // Definition is compared exactly: one answer, one place, and the two
        // servers are the same build reading the same files.
        if let (Some(a), Some(b)) = (churned.definition.get(i), reference.definition.get(i)) {
            if a != b {
                out.push(Divergence {
                    request: "definition".into(),
                    site: where_.clone(),
                    churned: brief(a),
                    reference: brief(b),
                });
            }
        }
        // References is compared as a set: the order is whatever the index
        // iterated in, which is not something two servers owe each other.
        if let (Some(a), Some(b)) = (churned.references.get(i), reference.references.get(i)) {
            let (a, b) = (locations(a), locations(b));
            if a != b {
                out.push(Divergence {
                    request: "references".into(),
                    site: where_,
                    churned: brief_set(&a, &b),
                    reference: brief_set(&b, &a),
                });
            }
        }
    }
    for (i, file) in probes.files.iter().enumerate() {
        if let (Some(a), Some(b)) = (
            churned.document_symbol.get(i),
            reference.document_symbol.get(i),
        ) {
            if a != b {
                out.push(Divergence {
                    request: "documentSymbol".into(),
                    site: rel(file),
                    churned: brief(a),
                    reference: brief(b),
                });
            }
        }
    }
    for (i, query) in probes.queries.iter().enumerate() {
        if let (Some(a), Some(b)) = (
            churned.workspace_symbol.get(i),
            reference.workspace_symbol.get(i),
        ) {
            let (a, b) = (symbol_set(a), symbol_set(b));
            if a != b {
                out.push(Divergence {
                    request: "workspace/symbol".into(),
                    site: format!("query {query:?}"),
                    churned: brief_set(&a, &b),
                    reference: brief_set(&b, &a),
                });
            }
        }
    }
    out
}

/// A `Location[]` answer as a set of `uri@line:col-line:col`.
fn locations(result: &Value) -> BTreeSet<String> {
    result
        .as_array()
        .map(|items| items.iter().map(location_key).collect())
        .unwrap_or_default()
}

/// A `SymbolInformation[]` answer as a set: name plus where it is. Ranking
/// depends on occurrence counts, which a churned index counts in its own order,
/// so only the set is something two servers owe each other.
fn symbol_set(result: &Value) -> BTreeSet<String> {
    result
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    format!(
                        "{} {}",
                        item["name"].as_str().unwrap_or_default(),
                        location_key(&item["location"])
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn location_key(location: &Value) -> String {
    let range = &location["range"];
    format!(
        "{}@{}:{}-{}:{}",
        location["uri"].as_str().unwrap_or_default(),
        range["start"]["line"],
        range["start"]["character"],
        range["end"]["line"],
        range["end"]["character"]
    )
}

/// A JSON answer, short enough to read in a failure report.
fn brief(value: &Value) -> String {
    let text = value.to_string();
    if text.len() <= 300 {
        return text;
    }
    // On a character boundary, not on byte 300: a Clojure identifier or a path
    // can be non-ASCII, and slicing through one would panic in the very report
    // that exists to explain a failure.
    let mut cut = 300;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… ({} bytes)", &text[..cut], text.len())
}

/// What one side has that the other does not — the whole set is unreadable and
/// the difference is the finding.
fn brief_set(mine: &BTreeSet<String>, theirs: &BTreeSet<String>) -> String {
    let extra: Vec<&String> = mine.difference(theirs).take(5).collect();
    format!(
        "{} entries, {} not in the other: {:?}",
        mine.len(),
        mine.difference(theirs).count(),
        extra
    )
}

// ---------------------------------------------------------------------------
// Convergence
// ---------------------------------------------------------------------------

/// Polls until every witness in the batch holds, or the deadline passes. The
/// error names the actions that never landed: a bulk round that converges for
/// its first file and drops its ninetieth is exactly the bug worth catching, so
/// every action is checked, not one per batch.
fn converge(
    session: &mut Session,
    witnesses: &[Witness],
    timeout: Duration,
) -> Result<Duration, Vec<String>> {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut pending: Vec<&Witness> = witnesses.iter().collect();
    loop {
        let mut failures = Vec::new();
        pending.retain(|witness| match witness.check(session) {
            Ok(()) => false,
            Err(why) => {
                failures.push(format!("{} ({why})", witness.action));
                true
            }
        });
        if pending.is_empty() {
            return Ok(started.elapsed());
        }
        if Instant::now() >= deadline {
            failures.truncate(10);
            return Err(failures);
        }
        std::thread::sleep(POLL);
    }
}

// ---------------------------------------------------------------------------
// Panic evidence
// ---------------------------------------------------------------------------

/// The churned server's `.clj-pulse/server.log`, accumulated. `main.rs` opens
/// it with `File::create`, which truncates, and both servers derive the path
/// from the same working directory — so every reference-server start wipes what
/// the churned server has written. The record is taken before each start and
/// again after, and the panic check runs against the record rather than
/// whatever the file happens to hold at the end.
#[derive(Default)]
struct LogRecord {
    text: String,
    seen: usize,
}

impl LogRecord {
    fn absorb(&mut self, root: &Path) {
        let text = LspClient::server_log(root);
        // A truncation that has already been re-grown past the old length would
        // read as an append, and everything before the offset — a panic among
        // it — would be skipped. `reset` is called at the one moment a
        // truncation happens, so `seen` is only ever an offset into a file that
        // has been growing since.
        let from = self.seen.min(text.len());
        self.text.push_str(&text[from..]);
        self.seen = text.len();
    }

    /// Called when the log is about to be truncated by another server starting
    /// in the same directory: whatever the file holds next is new.
    fn reset(&mut self) {
        self.seen = 0;
    }

    /// Every panic the run logged. `install_panic_hook` records payload and
    /// location through `tracing::error!`, and the default level is `WARN`, so
    /// the line is there without `--verbose`.
    fn panics(&self) -> Vec<String> {
        self.text
            .lines()
            .filter(|line| line.contains("panicked at"))
            .map(|line| line.trim().to_string())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The checkpoint
// ---------------------------------------------------------------------------

/// What one checkpoint measured.
struct Checkpoint {
    round: usize,
    rss_kib: Option<u64>,
    definition: Option<Duration>,
    definition_samples: usize,
    divergences: Vec<Divergence>,
    /// Baseline witnesses that never came back after the restore.
    stragglers: Vec<String>,
    resolved: usize,
    reference_resolved: usize,
    reference_note: String,
}

/// Brings the churned server to the same footing a freshly started one has —
/// nothing open, disk back at its pinned contents — and holds the two to the
/// same answers.
fn checkpoint(
    churned: &mut Session,
    churn: &mut Churn,
    guard: &CorpusGuard,
    probes: &mut Probes,
    settings: &Settings,
    log: &mut LogRecord,
    round: usize,
) -> Checkpoint {
    let root = settings.root.as_path();

    // 1. Every buffer closed, so no unsaved edit is part of what is compared.
    for file in churn.open_files() {
        churned.client().did_close(&file);
    }
    // 2-3. Disk back to baseline, and one notification covering all of it.
    let restored = churn.restore_changes();
    let witnesses = churn.restore_witnesses();
    guard.restore();
    notify_watched(churned.client(), &restored);

    // 4. The restore has to land like any other batch: the reference server is
    // started on the baseline, so an index still holding a churned var would
    // diverge for a reason that is not the bug being hunted.
    let stragglers = converge(churned, &witnesses, settings.converge_timeout)
        .err()
        .unwrap_or_default();
    churn.reset_to_baseline();

    // 5-6. Settled and quiet, with nothing open: the only state in which two
    // checkpoints' memory samples mean the same thing.
    let quiesce_deadline = Instant::now() + REQUEST_TIMEOUT;
    let pid = churned.pid();
    loop {
        churned.client().clear_notifications();
        let quiet = quiet_for(churned.client(), &["window/logMessage"], QUIET);
        if (quiet && !has_children(pid)) || Instant::now() >= quiesce_deadline {
            break;
        }
    }
    let rss = rss_kib(pid);

    // 7. The reference server. Its start truncates the shared log, so the
    // record is taken first.
    log.absorb(root);
    let mut reference = Session::production(root);
    // `main.rs` opens the log with `File::create`: the line above is the last
    // moment the churned server's own record can be read intact.
    log.reset();
    reference.client().initialize_no_wait(root);
    let reference_note = settle(&mut reference, Instant::now() + REQUEST_TIMEOUT);
    probes.trim_queries(&mut reference);

    // 8. The probe files open on both, from the text on disk. Definition
    // resolves through the document store at every branch, so with nothing open
    // both servers answer `null` and the comparison would pass while testing
    // nothing.
    for file in &probes.files {
        churned.client().did_open(file);
        reference.client().did_open(file);
    }

    // 9. The comparison, then the latency sample, then the reference is gone.
    let churned_answers = ask_all(churned, probes);
    let reference_answers = ask_all(&mut reference, probes);
    let divergences = compare(&churned_answers, &reference_answers, probes, root);

    let mut latencies = Vec::new();
    if !probes.sites.is_empty() {
        for i in 0..SAMPLES {
            let site = &probes.sites[i % probes.sites.len()];
            let sent = Instant::now();
            let answer = definition(churned.client(), site);
            let took = sent.elapsed();
            // A wrong answer is a divergence, not a sample: timing an index
            // lookup that found nothing measures nothing.
            if answers(&answer, &site.expect) {
                latencies.push(took);
            }
        }
    }

    // The probe files are closed again, so the next checkpoint's memory sample
    // is taken in the same state as this one.
    for file in &probes.files {
        churned.client().did_close(file);
    }
    drop(reference);
    log.absorb(root);

    Checkpoint {
        round,
        rss_kib: rss,
        definition_samples: latencies.len(),
        definition: median(&mut latencies),
        divergences,
        stragglers,
        resolved: churned_answers.resolved,
        reference_resolved: reference_answers.resolved,
        reference_note,
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs CLJ_PULSE_SOAK_ROOT pointing at a large Clojure checkout; run with `bb soak`"]
fn soak_long_session() {
    let Some(root) = std::env::var_os("CLJ_PULSE_SOAK_ROOT") else {
        println!("CLJ_PULSE_SOAK_ROOT is unset — skipping. Run `bb soak`.");
        return;
    };
    let root = PathBuf::from(root)
        .canonicalize()
        .expect("CLJ_PULSE_SOAK_ROOT does not exist");
    let corpus = std::env::var("CLJ_PULSE_SOAK_CORPUS").unwrap_or_else(|_| "(unnamed)".into());
    let settings = Settings::from_env(root, corpus);
    settings.print();

    let mut probes = Probes::discover(&settings.root, settings.probes);
    probes.print(&settings.root);
    assert!(
        !probes.sites.is_empty(),
        "no probe site found under {} — the oracle would compare nothing",
        settings.root.display()
    );

    let mut churn = Churn::new(&settings.root, settings.seed);
    assert!(
        !churn.tracked.is_empty(),
        "no churnable source file under {}/src",
        settings.root.display()
    );
    // Created before the first edit, so a panic anywhere below still leaves the
    // checkout at its pinned commit.
    let guard = CorpusGuard::new(&settings.root, &churn.soak_dir);
    let mut log = LogRecord::default();

    let mut churned = Session::production(&settings.root);
    let started = Instant::now();
    churned.client().initialize_no_wait(&settings.root);
    let note = settle(&mut churned, Instant::now() + REQUEST_TIMEOUT);
    println!();
    println!("churned server settled in {:?} ({note})", started.elapsed());

    let mut failures: Vec<String> = Vec::new();
    let mut checkpoints: Vec<Checkpoint> = Vec::new();
    for round in 1..=settings.rounds {
        let bulk = settings.is_bulk(round);
        let count = if bulk {
            settings.bulk_files
        } else {
            settings.files
        };
        let edits = churn.draw(round, count, bulk);
        let witnesses = churn.apply(churned.client(), &edits);
        match converge(&mut churned, &witnesses, settings.converge_timeout) {
            Ok(took) => println!(
                "round {round}{}: {} edits, {} witnesses, converged in {:?}",
                if bulk { " (bulk)" } else { "" },
                edits.len(),
                witnesses.len(),
                took
            ),
            Err(stragglers) => {
                println!(
                    "round {round}: {} of {} witnesses never landed:",
                    stragglers.len(),
                    witnesses.len()
                );
                for straggler in &stragglers {
                    println!("    {straggler}");
                }
                failures.push(format!(
                    "round {round}: {} witnesses never landed (first: {})",
                    stragglers.len(),
                    stragglers.first().map(String::as_str).unwrap_or("-")
                ));
            }
        }
        if !churned.alive() {
            failures.push(format!("the server exited during round {round}"));
            break;
        }
        if settings.is_checkpoint(round) {
            let point = checkpoint(
                &mut churned,
                &mut churn,
                &guard,
                &mut probes,
                &settings,
                &mut log,
                round,
            );
            point.print(&settings);
            point.print_json(&settings);
            checkpoints.push(point);
        }
    }

    // A run of zero rounds is the oracle checking itself: two servers on
    // identical input have to agree, and agree about something, before any
    // divergence can be blamed on churn.
    if checkpoints.is_empty() && churned.alive() {
        let point = checkpoint(
            &mut churned,
            &mut churn,
            &guard,
            &mut probes,
            &settings,
            &mut log,
            0,
        );
        point.print(&settings);
        point.print_json(&settings);
        checkpoints.push(point);
    }

    log.absorb(&settings.root);
    failures.extend(verdicts(&checkpoints, &settings, &churned, &log));

    println!();
    println!("soak summary");
    println!(
        "  {:<22} {} rounds, {} checkpoints, {:?} wall clock",
        "ran",
        settings.rounds,
        checkpoints.len(),
        started.elapsed()
    );
    if let (Some(first), Some(last)) = (checkpoints.first(), checkpoints.last()) {
        println!(
            "  {:<22} {} -> {} ({:.2}x)",
            "rss",
            mib(first.rss_kib),
            mib(last.rss_kib),
            growth(first.rss_kib, last.rss_kib).unwrap_or(f64::NAN)
        );
        println!(
            "  {:<22} {} -> {}",
            "definition median",
            us(first.definition),
            us(last.definition)
        );
    }
    println!("  {:<22} {}", "seed", settings.seed);

    println!();
    if failures.is_empty() {
        println!("PASS");
    } else {
        println!("FAIL");
        for failure in &failures {
            println!("  {failure}");
        }
        // The corpus is restored by `guard`'s `Drop` on the way out of this
        // panic, and the exit code is what makes `bb soak` a gate.
        panic!(
            "{} oracle failure(s); replay with CLJ_PULSE_SOAK_SEED={}",
            failures.len(),
            settings.seed
        );
    }
}

/// Everything the oracles have to say once the rounds are done. Separate from
/// the loop so a failure reads as one list rather than as scattered output.
fn verdicts(
    checkpoints: &[Checkpoint],
    settings: &Settings,
    churned: &Session,
    log: &LogRecord,
) -> Vec<String> {
    let mut out = Vec::new();
    for point in checkpoints {
        for divergence in &point.divergences {
            out.push(format!(
                "round {}: {} diverged at {}",
                point.round, divergence.request, divergence.site
            ));
        }
        for straggler in &point.stragglers {
            out.push(format!(
                "round {}: the restore never landed: {straggler}",
                point.round
            ));
        }
        if point.reference_resolved == 0 {
            out.push(format!(
                "round {}: no definition probe resolved on the reference server — the probe set is broken, not the index",
                point.round
            ));
        }
    }
    // Memory is compared at the first and last checkpoint alone, both sampled
    // quiesced with nothing open and the disk at baseline: the only two states
    // in the run that mean the same thing.
    if let (Some(first), Some(last)) = (checkpoints.first(), checkpoints.last()) {
        if let Some(growth) = growth(first.rss_kib, last.rss_kib) {
            if growth > settings.rss_growth {
                out.push(format!(
                    "RSS grew {growth:.2}x across checkpoints ({} -> {}), over the {}x ceiling",
                    mib(first.rss_kib),
                    mib(last.rss_kib),
                    settings.rss_growth
                ));
            }
        }
    }
    for error in &churned.errors {
        out.push(format!("the server answered an error: {error}"));
    }
    for panic in log.panics() {
        out.push(format!("a handler panicked: {panic}"));
    }
    out
}

fn growth(first: Option<u64>, last: Option<u64>) -> Option<f64> {
    match (first, last) {
        (Some(first), Some(last)) if first > 0 => Some(last as f64 / first as f64),
        _ => None,
    }
}

impl Settings {
    fn print(&self) {
        println!();
        println!("soak settings");
        println!("  {:<22} {}", "corpus", self.corpus);
        println!("  {:<22} {}", "root", self.root.display());
        println!("  {:<22} {}", "seed", self.seed);
        println!("  {:<22} {}", "rounds", self.rounds);
        println!("  {:<22} {}", "files per round", self.files);
        println!(
            "  {:<22} every {} rounds, {} files",
            "bulk round", self.bulk_every, self.bulk_files
        );
        println!(
            "  {:<22} every {} rounds",
            "checkpoint", self.checkpoint_every
        );
        println!("  {:<22} {:?}", "converge timeout", self.converge_timeout);
        println!("  {:<22} {}x", "rss growth ceiling", self.rss_growth);
        println!("  {:<22} {}", "probe sites", self.probes);
    }
}

impl Checkpoint {
    fn print(&self, settings: &Settings) {
        println!();
        println!("checkpoint after round {}", self.round);
        println!("  {:<22} {}", "rss", mib(self.rss_kib));
        println!(
            "  {:<22} {} ({} of {SAMPLES} samples)",
            "definition median",
            us(self.definition),
            self.definition_samples
        );
        println!(
            "  {:<22} {} of {} probes (reference: {})",
            "definitions resolved",
            self.resolved,
            settings
                .probes
                .min(self.resolved.max(self.reference_resolved)),
            self.reference_resolved
        );
        println!("  {:<22} {}", "reference settled by", self.reference_note);
        if self.divergences.is_empty() && self.stragglers.is_empty() {
            println!("  {:<22} none", "divergences");
        } else {
            println!("  {:<22} {}", "divergences", self.divergences.len());
            for divergence in &self.divergences {
                divergence.print();
            }
            for straggler in &self.stragglers {
                println!("  restore never landed: {straggler}");
            }
        }
    }

    /// One line per checkpoint, in the shape `bb bench` prints, so a later run
    /// can be diffed against this one.
    fn print_json(&self, settings: &Settings) {
        println!(
            "SOAK_JSON {}",
            json!({
                "corpus": settings.corpus,
                "seed": settings.seed,
                "round": self.round,
                "rss_kib": self.rss_kib,
                "definition_us": self.definition.map(|d| d.as_micros() as u64),
                "definition_samples": self.definition_samples,
                "divergences": self.divergences.len(),
                "stragglers": self.stragglers.len(),
                "definitions_resolved": self.resolved,
                "reference_definitions_resolved": self.reference_resolved,
                "reference_settled_by": self.reference_note,
            })
        );
    }
}

/// Latency, in microseconds: a definition on a warm index is well under a
/// millisecond, and "0 ms -> 0 ms" would report nothing about drift, which is
/// the whole reason the number is here.
fn us(d: Option<Duration>) -> String {
    match d {
        Some(d) => format!("{} µs", d.as_micros()),
        None => "n/a".to_string(),
    }
}

fn mib(kib: Option<u64>) -> String {
    match kib {
        Some(kib) => format!("{:.1} MiB", kib as f64 / 1024.0),
        None => "n/a".to_string(),
    }
}

// ---------------------------------------------------------------------------
// The one part of the soak that cannot wait for a manual run
// ---------------------------------------------------------------------------

/// A guard that fails to restore corrupts the corpus for every later run, and
/// nothing downstream would notice, so this runs in `bb check` rather than
/// under `--ignored` with the soak itself.
#[test]
fn churn_is_restored_by_the_guard() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_temp_corpus(&root);

    let seed = 20260910;
    let soak_dir = root.join("src").join(format!("soak_{seed:x}"));
    let mut session = Session::new(LspClient::start(&root));
    session.client().initialize(&root);

    {
        let _guard = CorpusGuard::new(&root, &soak_dir);
        let mut churn = Churn::new(&root, seed);
        let edits = churn.draw(1, 12, false);
        let witnesses = churn.apply(session.client(), &edits);
        assert!(
            !witnesses.is_empty(),
            "a round of churn produced no witness: {edits:?}"
        );
        // Every witness is a promise the generator makes to the driver, so
        // this is where it is checked that the server keeps it.
        for witness in &witnesses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match witness.check(&mut session) {
                    Ok(()) => break,
                    Err(why) => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "witness never landed: {} ({why})",
                            witness.action
                        );
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
        }
        assert!(
            !git(&root, &["status", "--porcelain", "-uno"])
                .unwrap()
                .is_empty()
                || soak_dir.exists(),
            "a round of churn changed nothing on disk"
        );
    }

    assert!(
        session.errors.is_empty(),
        "the server answered with an error: {:?}",
        session.errors
    );
    assert_eq!(
        git(&root, &["status", "--porcelain", "-uno"]).unwrap(),
        "",
        "the guard left tracked files modified"
    );
    assert!(
        !soak_dir.exists(),
        "the guard left {} behind",
        soak_dir.display()
    );
}

/// A buffer edit at the end of a file that does *not* end in a newline. Get the
/// end position wrong and the server rejects the change while the client's copy
/// of the buffer grows anyway, so every later edit in that file is applied at
/// the wrong place and the round measures nothing.
#[test]
fn a_buffer_edit_lands_on_a_file_without_a_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_temp_corpus(&root);
    let file = root.join("src/app/mod2.clj");
    assert!(!std::fs::read_to_string(&file).unwrap().ends_with('\n'));

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    let mut churn = Churn::new(&root, 1);
    let var = "soak-witness-no-trailing-newline".to_string();
    churn.apply(
        &mut client,
        &[
            Edit::Buffer {
                file: file.clone(),
                var: var.clone(),
            },
            // A second edit lands past the first: the position for it is only
            // right if the first one was applied where the client thinks.
            Edit::Buffer {
                file: file.clone(),
                var: format!("{var}-2"),
            },
        ],
    );

    // `documentSymbol` reads the open buffer, so it is the client's view of
    // what the server's copy of the document now holds.
    let symbols = client.document_symbols(&file);
    let names: Vec<&str> = symbols
        .as_array()
        .map(|items| items.iter().filter_map(|i| i["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        names.contains(&var.as_str()) && names.contains(&format!("{var}-2").as_str()),
        "the buffer edits did not reach the server: {names:?}"
    );
}

/// A minimal git-tracked project: a deps.edn with `src` on `:paths` and a
/// handful of namespaces for the churn to draw from.
fn write_temp_corpus(root: &Path) {
    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(root.join("deps.edn"), "{:paths [\"src\"]}\n").unwrap();
    for i in 0..5 {
        // `mod2` deliberately ends without a newline: a real corpus has such
        // files, and an edit at the wrong end position is rejected there.
        let tail = if i == 2 { "" } else { "\n" };
        std::fs::write(
            root.join(format!("src/app/mod{i}.clj")),
            format!("(ns app.mod{i})\n\n(defn f{i} [x] (inc x)){tail}"),
        )
        .unwrap();
    }
    assert!(git(root, &["init", "-q"]).is_some());
    assert!(git(root, &["config", "user.email", "soak@example.com"]).is_some());
    assert!(git(root, &["config", "user.name", "soak"]).is_some());
    assert!(git(root, &["add", "-A"]).is_some());
    assert!(git(root, &["commit", "-qm", "corpus"]).is_some());
}
