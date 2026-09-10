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
    fn check(&self, client: &mut LspClient) -> Result<(), String> {
        // The query is fuzzy (`workspace/symbol` matches by subsequence too),
        // so `soak-witness-…-7` also matches `soak-witness-…-17`. Only an
        // exact name counts, and the exact tier sorts first, so the 128-result
        // cap can never drop it.
        let answer = client.workspace_symbols(&self.name);
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
    let mut client = LspClient::start(&root);
    client.initialize(&root);

    {
        let _guard = CorpusGuard::new(&root, &soak_dir);
        let mut churn = Churn::new(&root, seed);
        let edits = churn.draw(1, 12, false);
        let witnesses = churn.apply(&mut client, &edits);
        assert!(
            !witnesses.is_empty(),
            "a round of churn produced no witness: {edits:?}"
        );
        // Every witness is a promise the generator makes to the driver, so
        // this is where it is checked that the server keeps it.
        for witness in &witnesses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match witness.check(&mut client) {
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
