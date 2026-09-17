//! clj-kondo's analysis as an oracle: one run per corpus, parsed into typed
//! entries, then turned into probes — a cursor position plus the answer
//! clj-pulse owes at it. The probes are oracle entries, not tree-sitter tokens;
//! tree-sitter only counts, per file, the symbol and keyword tokens no oracle
//! entry covers, so the report can say what neither side judged.
//!
//! Both test binaries compile this module, and neither uses all of it, so
//! `dead_code` is off here rather than per item.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::sites::Dialect;

/// The clj-kondo release the expectations were checked against. Another
/// version is run, with a warning: its analysis shape has been stable for
/// years, but a divergence found under a different one is worth a second look.
pub const PINNED_KONDO: &str = "2026.08.04";

/// kondo's `to` for a usage it could not resolve.
const UNKNOWN_NS: &str = "clj-kondo/unknown-namespace";

/// Names kondo attributes to `clojure.core` that no source form defines: the
/// special forms, and the vars `RT.java` creates that `core.clj` only
/// documents through `add-doc-and-meta`. A definition on one is a question
/// with no answer.
const NO_SOURCE_DEFINITION: &[&str] = &[
    "*file*",
    "*command-line-args*",
    "*warn-on-reflection*",
    "*compile-path*",
    "*compile-files*",
    "*unchecked-math*",
    "*compiler-options*",
    "*ns*",
    "*in*",
    "*out*",
    "*err*",
    "*flush-on-newline*",
    "*print-meta*",
    "*print-dup*",
    "*print-readably*",
    "*read-eval*",
    "*clojure-version*",
    "def",
    "if",
    "do",
    "let*",
    "quote",
    "var",
    "fn*",
    "loop*",
    "recur",
    "throw",
    "try",
    "catch",
    "finally",
    "monitor-enter",
    "monitor-exit",
    "new",
    "set!",
    ".",
    "letfn*",
    "case*",
    "deftype*",
    "reify*",
    "import*",
    "&",
];

// ---------------------------------------------------------------------------
// Running kondo
// ---------------------------------------------------------------------------

/// The version line of the `clj-kondo` on PATH, or `None` when there is none.
pub fn kondo_available() -> Option<String> {
    let out = Command::new("clj-kondo").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Runs kondo over `paths` (relative to `root`) with locals and keywords in
/// the analysis, and returns the parsed analysis with every filename absolute.
///
/// kondo is spawned from the *current* directory with absolute lint paths and
/// `--config-dir root/.clj-kondo`: the corpus's own config (`:lint-as`
/// included) applies to the oracle the way it applies to clj-pulse, and a
/// `clj-kondo` that is a version-manager shim still resolves — a shim needs a
/// tool config in the working directory's ancestry, and a temp-dir copy of a
/// fixture has none. The exit code is not a verdict: kondo exits 2 or 3 when
/// it merely found something to lint.
///
/// `extra_config` is a second `--config` map merged over the corpus's — how a
/// fixture without a committed `.clj-kondo/config.edn` gets the `:lint-as`
/// a real project ships (`""` for none).
pub fn run(root: &Path, paths: &[&str], extra_config: &str) -> Result<Analysis, String> {
    let mut cmd = Command::new("clj-kondo");
    cmd.arg("--lint");
    for p in paths {
        cmd.arg(root.join(p));
    }
    cmd.arg("--config-dir").arg(root.join(".clj-kondo"));
    // `:skip-comments`: clj-pulse indexes nothing inside `(comment …)`
    // (`handlers::ignored_forms`), and kondo's default is to analyze it.
    cmd.arg("--config").arg(
        "{:analysis {:locals true :keywords true} :output {:format :json} :skip-comments true}",
    );
    if !extra_config.is_empty() {
        cmd.arg("--config").arg(extra_config);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("spawning clj-kondo: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let wrapped: Output = serde_json::from_str(&stdout).map_err(|e| {
        format!(
            "clj-kondo output is not analysis JSON ({e}); stderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })?;
    let mut analysis = wrapped.analysis;
    let absolute = |f: &mut String| {
        let p = Path::new(f);
        if !p.is_absolute() {
            *f = root.join(p).to_string_lossy().to_string();
        }
    };
    analysis
        .var_definitions
        .iter_mut()
        .for_each(|e| absolute(&mut e.filename));
    analysis
        .var_usages
        .iter_mut()
        .for_each(|e| absolute(&mut e.filename));
    analysis
        .locals
        .iter_mut()
        .for_each(|e| absolute(&mut e.filename));
    analysis
        .local_usages
        .iter_mut()
        .for_each(|e| absolute(&mut e.filename));
    analysis
        .keywords
        .iter_mut()
        .for_each(|e| absolute(&mut e.filename));
    analysis
        .namespace_definitions
        .iter_mut()
        .for_each(|e| absolute(&mut e.filename));
    analysis
        .namespace_usages
        .iter_mut()
        .for_each(|e| absolute(&mut e.filename));
    Ok(analysis)
}

#[derive(Deserialize)]
struct Output {
    analysis: Analysis,
}

/// The sections of kondo's analysis this gate reads. Rows and columns are
/// kept as kondo prints them, 1-based; [`probes`] converts.
#[derive(Deserialize, Default)]
pub struct Analysis {
    #[serde(rename = "var-definitions", default)]
    pub var_definitions: Vec<VarDef>,
    #[serde(rename = "var-usages", default)]
    pub var_usages: Vec<VarUsage>,
    #[serde(default)]
    pub locals: Vec<Local>,
    #[serde(rename = "local-usages", default)]
    pub local_usages: Vec<LocalUsage>,
    #[serde(default)]
    pub keywords: Vec<Keyword>,
    #[serde(rename = "namespace-definitions", default)]
    pub namespace_definitions: Vec<NsDef>,
    #[serde(rename = "namespace-usages", default)]
    pub namespace_usages: Vec<NsUsage>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct VarDef {
    pub ns: String,
    pub name: String,
    pub filename: String,
    pub row: u32,
    pub col: u32,
    #[serde(rename = "name-row")]
    pub name_row: Option<u32>,
    #[serde(rename = "name-col")]
    pub name_col: Option<u32>,
    #[serde(rename = "defined-by")]
    pub defined_by: Option<String>,
}

fn unknown_ns() -> String {
    UNKNOWN_NS.to_string()
}

#[derive(Deserialize, Debug, Clone)]
pub struct VarUsage {
    pub from: Option<String>,
    /// Absent for a JS interop call (`js/parseInt`), which resolves to nothing
    /// the analysis names.
    #[serde(default = "unknown_ns")]
    pub to: String,
    pub name: String,
    pub filename: String,
    /// Absent for a usage a macro expansion produced (`are` → `is`): there is
    /// no token in the file for it.
    pub row: Option<u32>,
    pub col: Option<u32>,
    #[serde(rename = "name-row")]
    pub name_row: Option<u32>,
    #[serde(rename = "name-col")]
    pub name_col: Option<u32>,
    #[serde(rename = "macro")]
    pub is_macro: Option<bool>,
    pub refer: Option<bool>,
    pub alias: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Local {
    pub id: u64,
    pub name: String,
    pub filename: String,
    pub row: u32,
    pub col: u32,
    /// Non-null for a binding kondo synthesized rather than read.
    #[serde(rename = "derived-location")]
    pub derived_location: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LocalUsage {
    /// Absent for `%` inside `#(…)`, which binds nothing kondo names.
    pub id: Option<u64>,
    pub name: Option<String>,
    pub filename: String,
    pub row: u32,
    pub col: u32,
    #[serde(rename = "name-row")]
    pub name_row: Option<u32>,
    #[serde(rename = "name-col")]
    pub name_col: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Keyword {
    pub name: String,
    pub filename: String,
    pub row: u32,
    pub col: u32,
    pub ns: Option<String>,
    pub alias: Option<String>,
    #[serde(rename = "keys-destructuring")]
    pub keys_destructuring: Option<bool>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NsDef {
    pub name: String,
    pub filename: String,
    pub row: u32,
    pub col: u32,
    #[serde(rename = "name-row")]
    pub name_row: Option<u32>,
    #[serde(rename = "name-col")]
    pub name_col: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NsUsage {
    #[serde(default = "unknown_ns")]
    pub to: String,
    pub filename: String,
    pub row: u32,
    pub col: u32,
    #[serde(rename = "name-row")]
    pub name_row: Option<u32>,
    #[serde(rename = "name-col")]
    pub name_col: Option<u32>,
    #[serde(rename = "alias-row")]
    pub alias_row: Option<u32>,
    #[serde(rename = "alias-col")]
    pub alias_col: Option<u32>,
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

/// One question and the answer the oracle gives for it. `line` and
/// `character` are 0-based, at the name part of the token (kondo's
/// `name-row`/`name-col`), so a cursor never sits on the alias half of
/// `h/greet`.
#[derive(Debug)]
pub struct Probe {
    pub file: PathBuf,
    pub line: u32,
    pub character: u32,
    /// The token under the cursor, for the report.
    pub token: String,
    /// `var-usage/project/aliased`, `local/plain`, `keyword/keys`, …
    pub bucket: String,
    pub expect: Expectation,
}

#[derive(Debug)]
pub enum Expectation {
    /// Definition must land on this declaration (file, 0-based line of its
    /// name).
    Definition { file: PathBuf, line: u32 },
    /// Definition must land in a library file of this namespace, in a dialect
    /// the asking file loads.
    LibraryDefinition { ns: String, dialect: Dialect },
    /// References (`includeDeclaration: true`) must equal this multiset of
    /// sites.
    References(Sites),
    /// Rename must be refused: `prepareRename` or `rename` answers `invalid
    /// params`, or `null`.
    RenameRefused,
    /// Rename edits must touch exactly this multiset of sites.
    RenameSites(Sites),
}

impl Expectation {
    /// The request this expectation is about, as the report names it.
    pub fn request(&self) -> &'static str {
        match self {
            Expectation::Definition { .. } | Expectation::LibraryDefinition { .. } => "definition",
            Expectation::References(_) => "references",
            Expectation::RenameRefused | Expectation::RenameSites(_) => "rename",
        }
    }
}

/// Expected sites: how many occurrences each `(file, line)` holds, plus the
/// exact `(file, line, col)` set for the soft column check. Two usages of `f`
/// on one line are two sites, and a server answering one of them is wrong;
/// columns alone are never a divergence, since clj-pulse's ranges and kondo's
/// `col`/`name-col` differ on alias-qualified tokens by design.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sites {
    pub per_line: BTreeMap<(PathBuf, u32), usize>,
    pub exact: BTreeSet<(PathBuf, u32, u32)>,
}

impl Sites {
    pub fn from_exact(exact: BTreeSet<(PathBuf, u32, u32)>) -> Self {
        let mut per_line = BTreeMap::new();
        for (file, line, _) in &exact {
            *per_line.entry((file.clone(), *line)).or_insert(0) += 1;
        }
        Sites { per_line, exact }
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }
}

/// The file texts a probe build reads: one read per file, for the token under
/// each cursor and the destructuring checks. Columns everywhere here are
/// UTF-16 units — kondo counts Java chars and the server negotiates UTF-16 —
/// and are converted to chars or bytes only at the edge of a `str` or a tree.
#[derive(Default)]
struct Texts {
    by_file: BTreeMap<PathBuf, Option<(String, Option<tree_sitter::Tree>)>>,
    /// Where every local kondo reported starts, per file: what tells a
    /// destructuring directive from a map that merely spells `:keys`.
    locals: BTreeMap<PathBuf, BTreeSet<(u32, u32)>>,
}

/// The char index of a UTF-16 column in `line`, clamped to its end.
fn utf16_to_char(line: &str, col: usize) -> usize {
    let mut units = 0;
    for (i, c) in line.chars().enumerate() {
        if units >= col {
            return i;
        }
        units += c.len_utf16();
    }
    line.chars().count()
}

/// The UTF-16 column of a byte offset in `line` (clamped).
fn byte_to_utf16(line: &str, byte: usize) -> usize {
    line[..byte.min(line.len())].encode_utf16().count()
}

/// The byte offset of a UTF-16 column in `line`, clamped to its end.
fn utf16_to_byte(line: &str, col: usize) -> usize {
    let mut units = 0;
    for (b, c) in line.char_indices() {
        if units >= col {
            return b;
        }
        units += c.len_utf16();
    }
    line.len()
}

impl Texts {
    fn with_locals(analysis: &Analysis) -> Self {
        let mut texts = Texts::default();
        for local in &analysis.locals {
            texts
                .locals
                .entry(PathBuf::from(&local.filename))
                .or_default()
                .insert((local.row - 1, local.col - 1));
        }
        texts
    }

    fn get(&mut self, file: &Path) -> Option<&(String, Option<tree_sitter::Tree>)> {
        self.by_file
            .entry(file.to_path_buf())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(file).ok()?;
                let tree = clj_pulse::index::extractor::parse_tree(&text);
                Some((text, tree))
            })
            .as_ref()
    }

    /// The token's char span `[start, end)` around a UTF-16 column, kondo's
    /// idea of one: up to whitespace or a bracket, so `alias/name` stays
    /// whole and `::k` keeps its colons.
    fn token_span(line_text: &str, character: u32) -> (Vec<char>, usize, usize) {
        let chars: Vec<char> = line_text.chars().collect();
        let boundary = |c: char| c.is_whitespace() || "()[]{}\"'`~@^,".contains(c);
        let at = utf16_to_char(line_text, character as usize).min(chars.len());
        let mut start = at;
        while start > 0 && !boundary(chars[start - 1]) {
            start -= 1;
        }
        let mut end = at;
        while end < chars.len() && !boundary(chars[end]) {
            end += 1;
        }
        (chars, start, end)
    }

    /// The token at a 0-based `(line, character)`. Starts from the token's
    /// own start, not the position, since kondo's `name-col` points inside an
    /// aliased token.
    fn token_at(&mut self, file: &Path, line: u32, character: u32) -> String {
        let Some((text, _)) = self.get(file) else {
            return String::new();
        };
        let Some(line_text) = text.lines().nth(line as usize) else {
            return String::new();
        };
        let (chars, start, end) = Self::token_span(line_text, character);
        chars[start..end].iter().collect()
    }

    /// The 0-based UTF-16 column of the *name part* of the token at a
    /// position: kondo's `name-col` is where the whole token starts, alias
    /// included, so for `core/add` it points at `c`, and for `::local` at the
    /// colons. A cursor on the alias asks about the alias, which is a
    /// different question (and one whose answer is known to differ, ROADMAP
    /// 2026-09-16), and clj-pulse ranges a keyword occurrence by its name — so
    /// both the probe and the expected sites use the first character of `add`
    /// / `local`.
    fn name_part(&mut self, file: &Path, line: u32, character: u32) -> u32 {
        let Some((text, _)) = self.get(file) else {
            return character;
        };
        let Some(line_text) = text.lines().nth(line as usize) else {
            return character;
        };
        let (chars, start, end) = Self::token_span(line_text, character);
        let token: String = chars[start..end].iter().collect();
        let colons = token.len() - token.trim_start_matches(':').len();
        // `/` alone and a leading `/` are names, not separators.
        let after_slash = token
            .char_indices()
            .find(|(i, c)| *c == '/' && *i > colons)
            .map(|(i, _)| i + 1)
            .filter(|i| *i < token.len());
        let offset = after_slash.unwrap_or(colons);
        let prefix: String = chars[..start].iter().collect();
        (prefix.encode_utf16().count() + token[..offset].encode_utf16().count()) as u32
    }

    /// Whether the symbol at a 0-based position is an entry of a `:keys`,
    /// `:strs` or `:syms` destructuring vector — whatever the keyword's
    /// namespace (`::keys`, `::a/keys`, `:a/keys`). kondo reports such a
    /// binding as a plain local; clj-pulse refuses to rename it, since its
    /// name is also the key being read.
    fn destructured_at(&mut self, file: &Path, line: u32, character: u32) -> bool {
        let Some((text, Some(tree))) = self.get(file) else {
            return false;
        };
        let Some(node) = node_at(tree, text, line, character) else {
            return false;
        };
        let mut sym = node;
        while sym.kind() != "sym_lit" {
            let Some(parent) = sym.parent() else {
                return false;
            };
            sym = parent;
        }
        let Some(vec) = sym.parent().filter(|p| p.kind() == "vec_lit") else {
            return false;
        };
        let Some(key) = vec.prev_named_sibling().filter(|k| k.kind() == "kwd_lit") else {
            return false;
        };
        let name = key
            .child_by_field_name("name")
            .map(|n| n.utf8_text(text.as_bytes()).unwrap_or_default())
            .unwrap_or_default();
        matches!(name, "keys" | "strs" | "syms")
    }

    /// Whether the keyword at a 0-based position is the `:keys`/`:strs`/
    /// `:syms` directive of a destructuring map — `::c/keys` counts as a
    /// keyword of `simple.core` for kondo, but it names no key anyone reads.
    /// Binding position is kondo's call, not a guess from the tree: the
    /// vector after the keyword holds a local it reported. A map that is a
    /// value — `(def m {::keys [:a]})` — binds nothing there and reads its key.
    fn directive_at(&mut self, file: &Path, line: u32, character: u32) -> bool {
        let locals = self.locals.get(file).cloned().unwrap_or_default();
        let Some((text, Some(tree))) = self.get(file) else {
            return false;
        };
        let Some(node) = node_at(tree, text, line, character) else {
            return false;
        };
        let mut kwd = node;
        while kwd.kind() != "kwd_lit" {
            let Some(parent) = kwd.parent() else {
                return false;
            };
            kwd = parent;
        }
        let name = kwd
            .child_by_field_name("name")
            .map(|n| n.utf8_text(text.as_bytes()).unwrap_or_default())
            .unwrap_or_default();
        if !matches!(name, "keys" | "strs" | "syms")
            || !kwd.parent().is_some_and(|p| p.kind() == "map_lit")
        {
            return false;
        }
        let Some(vec) = kwd.next_named_sibling().filter(|v| v.kind() == "vec_lit") else {
            return false;
        };
        let lines: Vec<&str> = text.lines().collect();
        let point = |p: tree_sitter::Point| {
            let col = lines
                .get(p.row)
                .map(|l| byte_to_utf16(l, p.column))
                .unwrap_or(p.column);
            (p.row as u32, col as u32)
        };
        let (start, end) = (point(vec.start_position()), point(vec.end_position()));
        locals.range(start..end).next().is_some()
    }
}

/// The smallest node at a 0-based `(line, character)`, character in UTF-16
/// units and tree-sitter's bytes derived from the line.
fn node_at<'t>(
    tree: &'t tree_sitter::Tree,
    text: &str,
    line: u32,
    character: u32,
) -> Option<tree_sitter::Node<'t>> {
    let byte_col = text
        .lines()
        .nth(line as usize)
        .map(|l| utf16_to_byte(l, character as usize))?;
    let point = tree_sitter::Point::new(line as usize, byte_col);
    tree.root_node().descendant_for_point_range(point, point)
}

/// Every probe the analysis supports, in a fixed order: by file, line,
/// character, then request, so a limit takes the same ones on every run.
pub fn probes(analysis: &Analysis) -> Vec<Probe> {
    let mut texts = Texts::with_locals(analysis);
    let mut out = Vec::new();

    // --- var definitions -------------------------------------------------
    // The primary definition of each fqn: the non-`declare` one, or the first
    // declare when that is all there is. Every definition of the fqn is a site.
    let mut defs_by_fqn: BTreeMap<(&str, &str), Vec<&VarDef>> = BTreeMap::new();
    for d in &analysis.var_definitions {
        defs_by_fqn
            .entry((d.ns.as_str(), d.name.as_str()))
            .or_default()
            .push(d);
    }
    let is_declare = |d: &VarDef| d.defined_by.as_deref() == Some("clojure.core/declare");
    let primary: BTreeMap<(&str, &str), &VarDef> = defs_by_fqn
        .iter()
        .filter_map(|(fqn, defs)| {
            defs.iter()
                .find(|d| !is_declare(d))
                .or_else(|| defs.first())
                .map(|d| (*fqn, *d))
        })
        .collect();
    let defined_namespaces: BTreeSet<&str> = analysis
        .namespace_definitions
        .iter()
        .map(|n| n.name.as_str())
        .chain(analysis.var_definitions.iter().map(|d| d.ns.as_str()))
        .collect();
    // Required but defined nowhere in the corpus: a library's. The one kind
    // of keyword namespace `rename_target` refuses; `:db/id` under a namespace
    // nobody defines is renamable, its sites are all in the project.
    let library_namespaces: BTreeSet<&str> = analysis
        .namespace_usages
        .iter()
        .map(|n| n.to.as_str())
        .filter(|ns| !defined_namespaces.contains(ns))
        .collect();

    let mut usages_by_fqn: BTreeMap<(&str, &str), Vec<&VarUsage>> = BTreeMap::new();
    for u in &analysis.var_usages {
        usages_by_fqn
            .entry((u.to.as_str(), u.name.as_str()))
            .or_default()
            .push(u);
    }

    // --- var usages: definition ----------------------------------------------
    for u in &analysis.var_usages {
        let (Some(row), Some(col)) = (u.name_row, u.name_col) else {
            // A synthesized usage (`fn*` behind `#(…)`) has no token of its own.
            continue;
        };
        if u.to == UNKNOWN_NS
            || (u.to == "clojure.core" && NO_SOURCE_DEFINITION.contains(&u.name.as_str()))
        {
            continue;
        }
        let file = PathBuf::from(&u.filename);
        let line = row - 1;
        let character = texts.name_part(&file, line, col - 1);
        let expect = match primary.get(&(u.to.as_str(), u.name.as_str())) {
            Some(def) => {
                let Some(def_row) = def.name_row else {
                    continue;
                };
                Expectation::Definition {
                    file: PathBuf::from(&def.filename),
                    line: def_row - 1,
                }
            }
            None if defined_namespaces.contains(u.to.as_str()) => {
                // A namespace this corpus defines, a var it does not (an
                // `import-vars` re-export, a macro-generated def): the oracle
                // has nothing to point at.
                continue;
            }
            None => Expectation::LibraryDefinition {
                ns: u.to.clone(),
                dialect: Dialect::of(&file),
            },
        };
        let kind = if u.to == "clojure.core" {
            "core"
        } else if matches!(expect, Expectation::Definition { .. }) {
            "project"
        } else {
            "library"
        };
        let mut bucket = format!("var-usage/{kind}");
        if u.is_macro == Some(true) {
            bucket.push_str("/macro");
        }
        if u.refer == Some(true) {
            bucket.push_str("/referred");
        } else if u.alias.is_some() {
            bucket.push_str("/aliased");
        }
        out.push(Probe {
            token: texts.token_at(&file, line, character),
            file,
            line,
            character,
            bucket,
            expect,
        });
    }

    // --- var definitions: references and rename --------------------------------
    // Several definitions can share one name token (`defrecord R` also
    // defines `->R` and `map->R` there): only the one spelling the token gets
    // probes, since a cursor there names it.
    let mut seen_positions: BTreeSet<(PathBuf, u32, u32)> = BTreeSet::new();
    for (fqn, def) in &primary {
        let (Some(row), Some(col)) = (def.name_row, def.name_col) else {
            continue;
        };
        let file = PathBuf::from(&def.filename);
        let (line, character) = (row - 1, col - 1);
        let token = texts.token_at(&file, line, character);
        if token != def.name || !seen_positions.insert((file.clone(), line, character)) {
            continue;
        }
        let mut exact = BTreeSet::new();
        for d in &defs_by_fqn[fqn] {
            if let (Some(r), Some(c)) = (d.name_row, d.name_col) {
                exact.insert((PathBuf::from(&d.filename), r - 1, c - 1));
            }
        }
        for u in usages_by_fqn.get(fqn).into_iter().flatten() {
            if let (Some(r), Some(c)) = (u.name_row, u.name_col) {
                let character = texts.name_part(Path::new(&u.filename), r - 1, c - 1);
                exact.insert((PathBuf::from(&u.filename), r - 1, character));
            }
        }
        let sites = Sites::from_exact(exact);
        let short = def
            .defined_by
            .as_deref()
            .and_then(|d| d.rsplit('/').next())
            .unwrap_or("other");
        let bucket = format!("var-def/{short}");
        out.push(Probe {
            file: file.clone(),
            line,
            character,
            token: token.clone(),
            bucket: bucket.clone(),
            expect: Expectation::References(sites.clone()),
        });
        out.push(Probe {
            file,
            line,
            character,
            token,
            bucket,
            expect: Expectation::RenameSites(sites),
        });
    }

    // --- locals ------------------------------------------------------------
    let mut usages_by_id: BTreeMap<u64, Vec<&LocalUsage>> = BTreeMap::new();
    for u in &analysis.local_usages {
        if let Some(id) = u.id {
            usages_by_id.entry(id).or_default().push(u);
        }
    }
    for local in &analysis.locals {
        if local.name == "_" {
            // A discard by convention: no editor question is asked at it.
            continue;
        }
        let file = PathBuf::from(&local.filename);
        let line = local.row - 1;
        // `{:keys [c/x]}` binds `x`; the binding's name part is the site.
        let character = texts.name_part(&file, line, local.col - 1);
        let destructured =
            local.derived_location.is_some() || texts.destructured_at(&file, line, character);
        let bucket = if destructured {
            "local/destructured"
        } else {
            "local/plain"
        };
        let token = texts.token_at(&file, line, character);
        let mut exact = BTreeSet::new();
        exact.insert((file.clone(), line, character));
        for u in usages_by_id.get(&local.id).into_iter().flatten() {
            let (r, c) = match (u.name_row, u.name_col) {
                (Some(r), Some(c)) => (r, c),
                _ => (u.row, u.col),
            };
            let uline = r - 1;
            let uchar = texts.name_part(Path::new(&u.filename), uline, c - 1);
            exact.insert((PathBuf::from(&u.filename), uline, uchar));
            out.push(Probe {
                file: PathBuf::from(&u.filename),
                line: uline,
                character: uchar,
                token: texts.token_at(Path::new(&u.filename), uline, uchar),
                bucket: bucket.to_string(),
                expect: Expectation::Definition {
                    file: file.clone(),
                    line,
                },
            });
        }
        let sites = Sites::from_exact(exact);
        out.push(Probe {
            file: file.clone(),
            line,
            character,
            token: token.clone(),
            bucket: bucket.to_string(),
            expect: Expectation::References(sites.clone()),
        });
        out.push(Probe {
            file,
            line,
            character,
            token,
            bucket: bucket.to_string(),
            expect: if destructured {
                Expectation::RenameRefused
            } else {
                Expectation::RenameSites(sites)
            },
        });
    }

    // --- keywords ------------------------------------------------------------
    let mut keywords_by_fqn: BTreeMap<(&str, &str), Vec<&Keyword>> = BTreeMap::new();
    for k in &analysis.keywords {
        if let Some(ns) = &k.ns {
            keywords_by_fqn
                .entry((ns.as_str(), k.name.as_str()))
                .or_default()
                .push(k);
        }
    }
    for ((ns, _), group) in &keywords_by_fqn {
        // Two site sets, because clj-pulse ranges a keyword two ways: a
        // reference spans the whole token (`::c/site`), a rename edits its
        // name suffix (`site`), since that suffix is what every notation of
        // one key shares.
        let mut token_starts = BTreeSet::new();
        let mut name_starts = BTreeSet::new();
        for k in group {
            let file = PathBuf::from(&k.filename);
            if texts.directive_at(&file, k.row - 1, k.col - 1) {
                continue;
            }
            let name = texts.name_part(&file, k.row - 1, k.col - 1);
            token_starts.insert((file.clone(), k.row - 1, k.col - 1));
            name_starts.insert((file, k.row - 1, name));
        }
        if token_starts.is_empty() {
            continue;
        }
        let reference_sites = Sites::from_exact(token_starts);
        let rename_sites = Sites::from_exact(name_starts);
        // All-or-nothing in clj-pulse: one destructuring entry refuses the
        // whole rename, and a keyword of a library namespace is refused by
        // `rename_target`.
        let refused = group.iter().any(|k| k.keys_destructuring == Some(true))
            || library_namespaces.contains(ns);
        for k in group {
            let file = PathBuf::from(&k.filename);
            let line = k.row - 1;
            if texts.directive_at(&file, line, k.col - 1) {
                // `::c/keys` in `{::c/keys [x]}` is a destructuring directive;
                // there is nothing to navigate to or rename at it.
                continue;
            }
            let character = texts.name_part(&file, line, k.col - 1);
            let token = texts.token_at(&file, line, character);
            if k.keys_destructuring == Some(true) {
                // The cursor is on a local binding that reads the key: the
                // local's probes cover definition and references, and the
                // rename is refused for the destructuring.
                out.push(Probe {
                    file,
                    line,
                    character,
                    token,
                    bucket: "keyword/keys".to_string(),
                    expect: Expectation::RenameRefused,
                });
                continue;
            }
            let bucket = if k.alias.is_some() {
                "keyword/alias"
            } else {
                "keyword/qualified"
            };
            out.push(Probe {
                file: file.clone(),
                line,
                character,
                token: token.clone(),
                bucket: bucket.to_string(),
                expect: Expectation::References(reference_sites.clone()),
            });
            out.push(Probe {
                file,
                line,
                character,
                token,
                bucket: bucket.to_string(),
                expect: if refused {
                    Expectation::RenameRefused
                } else {
                    Expectation::RenameSites(rename_sites.clone())
                },
            });
        }
    }

    out.sort_by(|a, b| {
        (&a.file, a.line, a.character, a.expect.request(), &a.bucket).cmp(&(
            &b.file,
            b.line,
            b.character,
            b.expect.request(),
            &b.bucket,
        ))
    });
    out
}

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

/// Every 0-based `(line, character)` in `file` that some oracle entry sits at
/// — the token's start and its name part — except a usage kondo could not
/// resolve, which the report counts as unjudged.
pub fn covered(analysis: &Analysis, file: &Path) -> BTreeSet<(u32, u32)> {
    let mut out = BTreeSet::new();
    let same = |f: &str| Path::new(f) == file;
    let mut add_opt = |row: Option<u32>, col: Option<u32>| {
        if let (Some(r), Some(c)) = (row, col) {
            out.insert((r - 1, c - 1));
        }
    };
    for e in analysis
        .var_definitions
        .iter()
        .filter(|e| same(&e.filename))
    {
        add_opt(Some(e.row), Some(e.col));
        add_opt(e.name_row, e.name_col);
    }
    for e in analysis
        .var_usages
        .iter()
        .filter(|e| same(&e.filename) && e.to != UNKNOWN_NS)
    {
        add_opt(e.row, e.col);
        add_opt(e.name_row, e.name_col);
    }
    for e in analysis.locals.iter().filter(|e| same(&e.filename)) {
        add_opt(Some(e.row), Some(e.col));
    }
    for e in analysis.local_usages.iter().filter(|e| same(&e.filename)) {
        add_opt(Some(e.row), Some(e.col));
        add_opt(e.name_row, e.name_col);
    }
    for e in analysis.keywords.iter().filter(|e| same(&e.filename)) {
        add_opt(Some(e.row), Some(e.col));
    }
    for e in analysis
        .namespace_definitions
        .iter()
        .filter(|e| same(&e.filename))
    {
        add_opt(Some(e.row), Some(e.col));
        add_opt(e.name_row, e.name_col);
    }
    for e in analysis
        .namespace_usages
        .iter()
        .filter(|e| same(&e.filename))
    {
        add_opt(Some(e.row), Some(e.col));
        add_opt(e.name_row, e.name_col);
        add_opt(e.alias_row, e.alias_col);
    }
    out
}

/// How many `sym_lit`/`kwd_lit` tokens in `text` start at a position not in
/// `covered` — what neither side can judge. Columns are counted in UTF-16
/// units, kondo's unit.
pub fn unjudged(text: &str, covered: &BTreeSet<(u32, u32)>) -> usize {
    let Some(tree) = clj_pulse::index::extractor::parse_tree(text) else {
        return 0;
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut count = 0;
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "sym_lit" | "kwd_lit") {
            let start = node.start_position();
            let char_col = lines
                .get(start.row)
                .map(|l| l[..start.column.min(l.len())].encode_utf16().count())
                .unwrap_or(start.column);
            if !covered.contains(&(start.row as u32, char_col as u32)) {
                count += 1;
            }
            continue;
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    count
}
