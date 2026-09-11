//! Finding the places in a real corpus a request can be asked about, and
//! judging what came back. The bench times a definition at one such place; the
//! soak asks a whole probe set of them twice and compares. Both need the same
//! rules, and those rules were tuned against the pinned corpora — a site the
//! rules pick badly is a probe that answers `null` on every server, which times
//! nothing and compares nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::LspClient;

/// A cursor position whose definition answer is checked, not just timed.
pub struct Site {
    pub file: PathBuf,
    pub line: u32,
    pub character: u32,
    pub expect: Expect,
    /// The `alias/name` under the cursor, for the report.
    pub token: String,
}

/// Where a right answer has to land.
pub enum Expect {
    /// A file in the project, by path suffix.
    File(PathBuf),
    /// An entry inside a dependency archive, by entry path without its
    /// extension: a classpath can hold `clojure/string.clj` and
    /// `clojure/string.cljs` at once, and either is a library definition.
    Archive(String),
}

impl Expect {
    pub fn describe(&self) -> String {
        match self {
            Expect::File(p) => p.display().to_string(),
            Expect::Archive(entry) => format!("<dependency>/{entry}.clj[cs]"),
        }
    }
}

/// Every `.clj`/`.cljc` under `root` with its size, skipping dot-directories
/// (`.git`, `.cpcache`, `.clj-pulse`).
pub fn source_files(root: &Path) -> Vec<(u64, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
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
                if let Ok(size) = entry.metadata().map(|m| m.len()) {
                    out.push((size, path));
                }
            }
        }
    }
    out
}

/// Whether a path sits under the corpus's own top-level `src` or `test` — the
/// only files a definition probe is taken from or points at. It has to be the
/// *first* component: clj-kondo's `corpus/` holds deliberately broken sample
/// projects, `src` directories and all, which are on no server's source path,
/// so a definition into one would never resolve and the probe would burn the
/// whole ceiling waiting for it.
pub fn is_source_ish(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .next()
        .is_some_and(|c| matches!(c.as_os_str().to_str(), Some("src") | Some("test")))
}

/// The first `alias/name` usage in `text` whose alias names a namespace that
/// has a file in this corpus — so the right answer is a known path, and a
/// server that answers something else is not credited with a sample.
pub fn project_site(text: &str, file: &Path, root: &Path, paths: &[PathBuf]) -> Option<Site> {
    usage_site(text, file, |ns, name| {
        namespace_file(ns, name, root, paths).map(Expect::File)
    })
}

/// [`project_site`], but up to `limit` of them: the soak asks every request in
/// its probe set at several sites, so one badly chosen one cannot make a
/// checkpoint pass by comparing two `null`s.
pub fn project_sites(
    text: &str,
    file: &Path,
    root: &Path,
    paths: &[PathBuf],
    limit: usize,
) -> Vec<Site> {
    usage_sites(
        text,
        file,
        |ns, name| namespace_file(ns, name, root, paths).map(Expect::File),
        limit,
    )
}

/// The first `alias/name` usage of `clojure.string` — a var from a JAR on the
/// classpath, so the answer has to come out of a dependency archive.
pub fn library_site(text: &str, file: &Path) -> Option<Site> {
    usage_site(text, file, |ns, _| {
        (ns == "clojure.string").then(|| Expect::Archive("clojure/string".to_string()))
    })
}

/// Up to `limit` `alias/name` usages in `text` that `expect` accepts, in file
/// order. The bench wants the first one; the soak wants a probe set, and taking
/// them from one pass keeps both looking at the same kind of site.
pub fn usage_sites(
    text: &str,
    file: &Path,
    expect: impl Fn(&str, &str) -> Option<Expect>,
    limit: usize,
) -> Vec<Site> {
    let mut sites = Vec::new();
    let aliases = as_aliases(text);
    if aliases.is_empty() || limit == 0 {
        return sites;
    }
    for (line_no, line) in text.lines().enumerate() {
        // The ns form itself is full of `:as` pairs that are not usages, and a
        // non-ASCII line would make byte columns disagree with the UTF-16 ones
        // the protocol counts.
        if line.contains(":require") || line.contains(":as ") || !line.is_ascii() {
            continue;
        }
        for (col, token) in tokens(line) {
            let Some((alias, name)) = token.split_once('/') else {
                continue;
            };
            if name.is_empty() || !is_symbol_start(alias) {
                continue;
            }
            let Some(ns) = aliases.get(alias) else {
                continue;
            };
            let Some(expect) = expect(ns, name) else {
                continue;
            };
            sites.push(Site {
                file: file.to_path_buf(),
                line: line_no as u32,
                // The middle of the *name* part: unambiguous for any server,
                // whatever it does with the namespace half of the token.
                character: (col + alias.len() + 1 + name.len() / 2) as u32,
                expect,
                token: token.to_string(),
            });
            if sites.len() == limit {
                return sites;
            }
        }
    }
    sites
}

pub fn usage_site(
    text: &str,
    file: &Path,
    expect: impl Fn(&str, &str) -> Option<Expect>,
) -> Option<Site> {
    usage_sites(text, file, expect, 1).into_iter().next()
}

pub fn is_symbol_start(alias: &str) -> bool {
    alias
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
}

/// The file a namespace name would live in, when this corpus holds one *and*
/// that file defines `name` itself. The match is anchored at the source root,
/// not merely a path suffix: clj-kondo has a
/// `src/clj_kondo/impl/types/clojure/string.clj`, which a suffix match would
/// happily offer as the definition of `clojure.string`. The `name` check rules
/// out a facade namespace — metabase's `metabase.events.core` re-exports
/// `derive!` through `potemkin/import-vars`, so a definition on it lands
/// wherever the original is, or nowhere, and either way not in the file the
/// require names.
pub fn namespace_file(ns: &str, name: &str, root: &Path, paths: &[PathBuf]) -> Option<PathBuf> {
    let rel = ns.replace('-', "_").replace('.', "/");
    paths
        .iter()
        .find(|p| {
            let Ok(from_root) = p.strip_prefix(root) else {
                return false;
            };
            let mut components = from_root.components();
            // The source root itself (`src`, `test`); `is_source_ish` has
            // already established that it is one.
            components.next();
            let under_root = components.as_path().to_string_lossy().to_string();
            if under_root != format!("{rel}.clj") && under_root != format!("{rel}.cljc") {
                return false;
            }
            std::fs::read_to_string(p).is_ok_and(|text| defines(&text, name))
        })
        .cloned()
}

/// Whether `text` holds a top-level `(def… name …)` form — the cheap test for
/// "this file really is where that var is written".
pub fn defines(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        let tokens = tokens(line);
        let Some((_, head)) = tokens.first() else {
            return false;
        };
        let bare = head.rsplit('/').next().unwrap_or(head);
        bare.starts_with("def") && tokens.get(1).is_some_and(|(_, t)| *t == name)
    })
}

/// Every `:as <alias>` pair in the file's ns form, alias -> namespace.
pub fn as_aliases(text: &str) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    let trim = |w: &str| w.trim_matches(|c: char| "[]()".contains(c)).to_string();
    for (i, window) in words.windows(2).enumerate() {
        if window[0] != ":as" {
            continue;
        }
        let alias = trim(window[1]);
        // The namespace is the last symbol before the `:as`, which is where a
        // require vector puts it: `[clojure.string :as str]`.
        let Some(ns) = words.get(i.wrapping_sub(1)).map(|w| trim(w)) else {
            continue;
        };
        if alias.is_empty() || alias.starts_with(':') || ns.is_empty() || ns.starts_with(':') {
            continue;
        }
        aliases.entry(alias).or_insert(ns);
    }
    aliases
}

/// `(column, token)` for every symbol-ish token in `line`, ignoring anything
/// after a `;` comment.
pub fn tokens(line: &str) -> Vec<(usize, &str)> {
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

// ---------------------------------------------------------------------------
// Reading what a server answered
// ---------------------------------------------------------------------------

/// `textDocument/definition` at a site, as the raw result — the full response,
/// errors and all, is the caller's business.
pub fn definition(client: &mut LspClient, site: &Site) -> Value {
    client.request_full(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": format!("file://{}", site.file.display()) },
            "position": { "line": site.line, "character": site.character }
        }),
    )["result"]
        .clone()
}

/// Every URI in a definition answer, whichever of the three shapes the server
/// chose (`Location`, `Location[]`, `LocationLink[]`).
pub fn definition_uris(result: &Value) -> Vec<String> {
    let one = |v: &Value| {
        v["uri"]
            .as_str()
            .or_else(|| v["targetUri"].as_str())
            .map(str::to_string)
    };
    match result {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

/// Whether the answer landed where it should. A wrong or empty answer is a
/// retry, never a sample: a server that answers `null` in 2 ms is not fast.
pub fn answers(result: &Value, expect: &Expect) -> bool {
    let uris = definition_uris(result);
    match expect {
        Expect::File(path) => {
            let tail = path.to_string_lossy().to_string();
            uris.iter().any(|u| u.ends_with(&tail))
        }
        // clj-pulse navigates a JAR entry as `jar:`, clojure-lsp as `zipfile:`
        // (its `:dependency-scheme` default). Both are "inside a dependency".
        Expect::Archive(entry) => uris.iter().any(|u| {
            (u.starts_with("jar:") || u.starts_with("zipfile:"))
                && ["clj", "cljc", "cljs"]
                    .iter()
                    .any(|ext| u.ends_with(&format!("{entry}.{ext}")))
        }),
    }
}
