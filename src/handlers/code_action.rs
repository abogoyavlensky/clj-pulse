use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use tower_lsp::lsp_types::*;
use tree_sitter::{Node, Parser};

use crate::document::DocumentStore;
use crate::index::{extractor, Index, NsMeta};

/// Aggregates the file's code actions: an "Add require …" quickfix when the
/// cursor sits on a qualified symbol whose namespace is not yet required, and a
/// "Clean namespace" source action that removes unused requires. Each action is
/// only built when the request's `context.only` permits its kind.
pub fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: CodeActionParams,
) -> Result<Option<CodeActionResponse>> {
    let uri = params.text_document.uri.clone();
    let Some(text) = documents.text(&uri) else {
        return Ok(None);
    };
    let Ok(path) = uri.to_file_path() else {
        return Ok(None);
    };
    let only = params.context.only.as_deref();

    let mut actions: Vec<CodeActionOrCommand> = Vec::new();

    if kind_allowed(only, &CodeActionKind::QUICKFIX) {
        actions.extend(add_require_actions(
            index, documents, &uri, &text, &path, &params,
        ));
    }

    if kind_allowed(only, &CodeActionKind::SOURCE_ORGANIZE_IMPORTS) {
        if let Some(edits) = clean_ns_edits(&text) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), edits);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Clean namespace".to_string(),
                kind: Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                ..Default::default()
            }));
        }
    }

    if actions.is_empty() {
        Ok(None)
    } else {
        Ok(Some(actions))
    }
}

/// Whether a `context.only` filter permits offering an action of `kind`. LSP
/// matching is hierarchical: a requested `source` matches `source.organizeImports`.
/// `None`/empty means the client wants everything.
fn kind_allowed(only: Option<&[CodeActionKind]>, kind: &CodeActionKind) -> bool {
    match only {
        None | Some([]) => true,
        Some(kinds) => kinds.iter().any(|req| {
            let req = req.as_str();
            let k = kind.as_str();
            k == req || k.starts_with(&format!("{}.", req))
        }),
    }
}

/// The "Add require …" quickfixes for the qualified symbol under the cursor.
fn add_require_actions(
    index: &Index,
    documents: &DocumentStore,
    uri: &Url,
    text: &str,
    path: &Path,
    params: &CodeActionParams,
) -> Vec<CodeActionOrCommand> {
    let Some(token) = documents.word_at(uri, params.range.start) else {
        return vec![];
    };
    // Resolve against the live buffer: its requires may differ from the index.
    let Ok((ns_meta, _)) = extractor::extract(text, path) else {
        return vec![];
    };

    // The unresolved-namespace diagnostics at this position, so VS Code binds
    // the fix to the squiggle (and marks the diagnostic resolved on apply).
    let fixed: Vec<Diagnostic> = params
        .context
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(NumberOrString::String("unresolved-namespace".to_string())))
        .cloned()
        .collect();

    candidates(index, &ns_meta, &token)
        .into_iter()
        .filter_map(|candidate| {
            let spec = candidate.spec();
            let edit = require_edit(text, &spec)?;
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![edit]);
            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Add require `{}`", spec),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: if fixed.is_empty() {
                    None
                } else {
                    Some(fixed.clone())
                },
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                ..Default::default()
            }))
        })
        .collect()
}

/// Conventional aliases whose namespace is not simply the last dot-segment.
/// Used as the first source of candidates for a missing alias.
const CURATED_ALIASES: &[(&str, &str)] = &[
    ("str", "clojure.string"),
    ("set", "clojure.set"),
    ("io", "clojure.java.io"),
    ("edn", "clojure.edn"),
    ("walk", "clojure.walk"),
    ("pp", "clojure.pprint"),
    ("async", "clojure.core.async"),
    ("sh", "clojure.java.shell"),
];

/// A namespace that, if required, would resolve a qualified usage.
/// `alias = Some(a)` renders as `[ns :as a]`; `None` as `[ns]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub namespace: String,
    pub alias: Option<String>,
}

impl Candidate {
    /// The require spec as it appears inside `(:require …)`.
    pub fn spec(&self) -> String {
        match &self.alias {
            Some(alias) => format!("[{} :as {}]", self.namespace, alias),
            None => format!("[{}]", self.namespace),
        }
    }
}

// Source ranks: lower sorts first in the action list.
const RANK_CURATED: u8 = 0;
const RANK_FULLY_QUALIFIED: u8 = 1;
const RANK_LAST_SEGMENT: u8 = 2;

/// Given a qualified usage `token` (`prefix/name`) and the file's current
/// namespace metadata, returns the namespaces we could require to resolve it.
/// Returns empty when the prefix is already available, the token is not
/// qualified, or no indexed namespace both matches the prefix and defines
/// `name`.
pub fn candidates(index: &Index, ns_meta: &NsMeta, token: &str) -> Vec<Candidate> {
    let Some((prefix, name)) = token.split_once('/') else {
        return vec![];
    };
    if prefix.is_empty() || name.is_empty() || prefix == "clojure.core" {
        return vec![];
    }

    // Already available in this file: own namespace, an alias, or a required
    // namespace (see `NsMeta::resolves_prefix`). Shared with the
    // unresolved-namespace diagnostic so squiggle and fix never disagree.
    if ns_meta.resolves_prefix(prefix) {
        return vec![];
    }

    // Collect (rank, namespace, alias) from each source, keeping the best
    // (lowest) rank per namespace.
    let mut best: HashMap<String, (u8, Option<String>)> = HashMap::new();
    let mut consider = |ns: String, rank: u8, alias: Option<String>| match best.get(&ns) {
        Some((r, _)) if *r <= rank => {}
        _ => {
            best.insert(ns, (rank, alias));
        }
    };

    for (a, ns) in CURATED_ALIASES {
        if *a == prefix {
            consider(ns.to_string(), RANK_CURATED, Some(prefix.to_string()));
        }
    }
    if index.namespaces.contains_key(prefix) {
        consider(prefix.to_string(), RANK_FULLY_QUALIFIED, None);
    }
    for entry in index.namespaces.iter() {
        let ns = entry.key();
        if ns != prefix && ns.rsplit('.').next() == Some(prefix) {
            consider(ns.clone(), RANK_LAST_SEGMENT, Some(prefix.to_string()));
        }
    }

    // Keep only namespaces that actually define `name`, never the file's own
    // namespace (a last-segment match can resolve back to it), then order by
    // rank (curated first), breaking ties by namespace for determinism.
    let mut ranked: Vec<(u8, Candidate)> = best
        .into_iter()
        .filter(|(ns, _)| ns != &ns_meta.name && index.lookup_in_ns(ns, name).is_some())
        .map(|(namespace, (rank, alias))| (rank, Candidate { namespace, alias }))
        .collect();
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.namespace.cmp(&b.1.namespace))
    });
    ranked.into_iter().map(|(_, c)| c).collect()
}

/// Builds the edit that inserts `spec` (e.g. `[clojure.string :as str]`) into
/// `source`'s `ns` form. Appends to an existing `(:require …)` clause when one
/// is present, otherwise inserts a new `(:require …)` clause after the ns
/// form's last element (name / docstring / attr-map). Returns `None` when
/// there is no `ns` form to edit.
pub fn require_edit(source: &str, spec: &str) -> Option<TextEdit> {
    let mut parser = Parser::new();
    parser.set_language(extractor::language()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();

    let ns_form = ns_form(root, source)?;

    if let Some(require) = require_clause(ns_form, source) {
        let kids = named_children(require);
        let specs: Vec<Node> = kids
            .iter()
            .copied()
            .filter(|n| n.kind() != "kwd_lit")
            .collect();
        // Insert after the last spec, indented to match the first spec. An
        // empty `(:require)` is degenerate; anchor after the keyword so the
        // result stays inside the parens.
        let (anchor, indent) = match specs.first() {
            Some(first) => (*specs.last().unwrap(), first.start_position().column),
            None => (kids[0], require.start_position().column + 1),
        };
        let pos = end_position(anchor, source);
        let new_text = format!("\n{}{}", " ".repeat(indent), spec);
        Some(TextEdit {
            range: empty_range(pos),
            new_text,
        })
    } else {
        let last = named_children(ns_form)
            .into_iter()
            .last()
            .unwrap_or(ns_form);
        let pos = end_position(last, source);
        let new_text = format!("\n  (:require {})", spec);
        Some(TextEdit {
            range: empty_range(pos),
            new_text,
        })
    }
}

/// What to do with one `:require` spec when cleaning the namespace.
enum Plan {
    /// Drop the whole spec (unused, or an exact duplicate of an earlier one).
    Remove,
    /// Keep the spec, emitting this text (verbatim, or rebuilt with unused
    /// `:refer` names pruned).
    Keep(String),
}

/// Builds the edits for the "Clean namespace" source action: removes `:require`
/// libspecs the file never uses (unused `:as` alias and no fully-qualified use),
/// prunes unused `:refer` names, and drops exact-duplicate specs. Plain
/// side-effecting requires (`[some.ns]` / bare `some.ns`, with no `:as`/`:refer`)
/// and reader-conditional specs are left untouched, and surviving specs keep
/// their original order and formatting. Returns `None` when nothing changes.
pub fn clean_ns_edits(source: &str) -> Option<Vec<TextEdit>> {
    let mut parser = Parser::new();
    parser.set_language(extractor::language()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();

    let ns_form = ns_form(root, source)?;
    let ns_children = named_children(ns_form);
    // An ns form may carry more than one `(:require …)` clause; clean them all.
    let requires: Vec<Node> = ns_children
        .iter()
        .copied()
        .filter(|c| c.kind() == "list_lit" && first_token_is(*c, "kwd_lit", ":require", source))
        .collect();
    if requires.is_empty() {
        return None;
    }

    let mut used_prefixes = used_prefixes(source);
    collect_keyword_prefixes(root, source, &mut used_prefixes);
    let used_bare = used_bare_symbols(root, ns_form, source);

    // `seen` is shared so a duplicate spec is dropped even across clauses.
    let mut seen: HashSet<String> = HashSet::new();
    let mut edits = Vec::new();
    for require in &requires {
        if let Some(edit) = clean_one_clause(
            *require,
            &ns_children,
            source,
            &used_prefixes,
            &used_bare,
            &mut seen,
        ) {
            edits.push(edit);
        }
    }

    if edits.is_empty() {
        None
    } else {
        Some(edits)
    }
}

/// Builds the edit for a single `(:require …)` clause, or `None` if it needs no
/// change. `ns_children` is the enclosing ns form's children (used to delete a
/// fully-emptied clause); `seen` accumulates spec texts across clauses for
/// duplicate detection.
fn clean_one_clause(
    require: Node,
    ns_children: &[Node],
    source: &str,
    used_prefixes: &HashSet<String>,
    used_bare: &HashSet<String>,
    seen: &mut HashSet<String>,
) -> Option<TextEdit> {
    let clause_children = named_children(require);
    // children[0] is the `:require` keyword; the rest are specs.
    if clause_children.len() < 2 {
        return None;
    }
    let specs = &clause_children[1..];

    let mut plans: Vec<Plan> = Vec::with_capacity(specs.len());
    let mut changed = false;
    for spec in specs {
        let plan = plan_spec(*spec, source, used_prefixes, used_bare, seen);
        match &plan {
            Plan::Remove => changed = true,
            Plan::Keep(t) if t.as_str() != node_text(*spec, source) => changed = true,
            Plan::Keep(_) => {}
        }
        plans.push(plan);
    }
    if !changed {
        return None;
    }

    // Every spec removed: drop the whole `(:require …)` clause (and the
    // whitespace before it) rather than leaving an empty `(:require)`.
    if !plans.iter().any(|p| matches!(p, Plan::Keep(_))) {
        let idx = ns_children.iter().position(|n| n.id() == require.id())?;
        let prev = ns_children.get(idx.checked_sub(1)?)?;
        return Some(TextEdit {
            range: tower_lsp::lsp_types::Range {
                start: end_position(*prev, source),
                end: end_position(require, source),
            },
            new_text: String::new(),
        });
    }

    // Rebuild the span from the end of `:require` to the end of the last spec,
    // re-emitting each survivor with its original leading separator so untouched
    // specs keep their indentation.
    let mut replacement = String::new();
    for (j, spec) in specs.iter().enumerate() {
        if let Plan::Keep(text) = &plans[j] {
            let prev = clause_children[j]; // the `:require` keyword when j == 0
            replacement.push_str(&source[prev.end_byte()..spec.start_byte()]);
            replacement.push_str(text);
        }
    }
    Some(TextEdit {
        range: tower_lsp::lsp_types::Range {
            start: end_position(clause_children[0], source),
            end: end_position(*specs.last().unwrap(), source),
        },
        new_text: replacement,
    })
}

/// Namespace prefixes used in qualified symbols (`str/join` → `str`), covering
/// both `:as` aliases and fully-qualified `some.ns/foo` uses.
fn used_prefixes(source: &str) -> HashSet<String> {
    extractor::qualified_usages(source)
        .into_iter()
        .map(|u| u.prefix)
        .collect()
}

/// Adds namespace prefixes referenced by namespaced keywords (`::alias/x`) and
/// namespaced map literals (`#::alias{…}`). Auto-resolved forms reference an
/// `:as` alias that `qualified_usages` (symbols only) never sees, so a require
/// used *solely* through such a keyword would otherwise be removed and break the
/// file. Literal single-colon namespaces are recorded too — harmlessly
/// conservative, since over-keeping a require is always safe.
fn collect_keyword_prefixes(node: Node, source: &str, out: &mut HashSet<String>) {
    match node.kind() {
        "kwd_lit" => {
            if let Some(ns) = node.child_by_field_name("namespace") {
                out.insert(node_text(ns, source).to_string());
            }
        }
        "ns_map_lit" => {
            if let Some(prefix) = node.child_by_field_name("prefix") {
                if prefix.kind() == "kwd_lit" {
                    if let Some(name) = prefix.child_by_field_name("name") {
                        out.insert(node_text(name, source).to_string());
                    }
                }
            }
        }
        _ => {}
    }
    for child in named_children(node) {
        collect_keyword_prefixes(child, source, out);
    }
}

/// Every bare (unqualified) symbol name used outside the `ns` form — the basis
/// for deciding whether a `:refer`'d name is used. Deliberately broad (locals,
/// def names, quoted symbols all count): keeping a still-referenced require is
/// always safe, removing a used one is not.
fn used_bare_symbols(root: Node, ns_form: Node, source: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_bare(root, ns_form.id(), source, &mut out);
    out
}

fn collect_bare(node: Node, ns_id: usize, source: &str, out: &mut HashSet<String>) {
    if node.id() == ns_id {
        return;
    }
    if node.kind() == "sym_lit" && node.child_by_field_name("namespace").is_none() {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
            .unwrap_or_else(|| node_text(node, source));
        if !name.is_empty() {
            out.insert(name.to_string());
        }
    }
    for child in named_children(node) {
        collect_bare(child, ns_id, source, out);
    }
}

/// Plans a single `:require` spec. Exact-duplicate libspecs are dropped; bare
/// side-effecting requires and reader conditionals are kept verbatim; libspecs
/// are analysed for usage.
fn plan_spec(
    spec: Node,
    source: &str,
    used_prefixes: &HashSet<String>,
    used_bare: &HashSet<String>,
    seen: &mut HashSet<String>,
) -> Plan {
    let plan = match spec.kind() {
        "vec_lit" => plan_libspec(spec, source, used_prefixes, used_bare),
        // Bare `some.ns` side-effecting requires, reader conditionals, comments
        // and anything unrecognised are preserved untouched.
        _ => Plan::Keep(node_text(spec, source).to_string()),
    };

    // Deduplicate on the *final* emitted text, so two specs that become
    // identical only after refer-pruning still collapse in a single pass.
    // Reader conditionals / comments (non-spec nodes) are never deduplicated.
    if let Plan::Keep(text) = &plan {
        if matches!(spec.kind(), "vec_lit" | "sym_lit") && !seen.insert(normalize_ws(text)) {
            return Plan::Remove;
        }
    }
    plan
}

/// Plans a `[ns …]` libspec given the file's usage sets.
fn plan_libspec(
    spec: Node,
    source: &str,
    used_prefixes: &HashSet<String>,
    used_bare: &HashSet<String>,
) -> Plan {
    let verbatim = || Plan::Keep(node_text(spec, source).to_string());
    let items = named_children(spec);
    let Some(first) = items.first() else {
        return verbatim();
    };
    if first.kind() != "sym_lit" {
        // e.g. a `[[a] [b]]` splice vector — not a shape we model; keep it.
        return verbatim();
    }
    let ns_name = node_text(*first, source).to_string();

    let mut alias: Option<String> = None;
    let mut refer_kwd: Option<Node> = None;
    let mut refer_vec: Option<Node> = None;
    let mut refer_all = false;

    let mut i = 1;
    while i < items.len() {
        let item = items[i];
        if item.kind() == "kwd_lit" {
            match node_text(item, source) {
                ":as" if i + 1 < items.len() && items[i + 1].kind() == "sym_lit" => {
                    alias = Some(node_text(items[i + 1], source).to_string());
                    i += 2;
                    continue;
                }
                ":refer" if i + 1 < items.len() => {
                    let next = items[i + 1];
                    if next.kind() == "vec_lit" {
                        refer_kwd = Some(item);
                        refer_vec = Some(next);
                    } else if next.kind() == "kwd_lit" && node_text(next, source) == ":all" {
                        refer_all = true;
                    }
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }

    // Libspec options we don't model (`:rename {old new}`, `:reload`, …) can
    // introduce usable names we never see, so removing or pruning such a spec
    // could drop a used require or leave a dangling option. Keep it untouched.
    let has_unknown_opt = items.iter().any(|n| {
        n.kind() == "kwd_lit" && !matches!(node_text(*n, source), ":as" | ":refer" | ":all")
    });
    if has_unknown_opt {
        return verbatim();
    }

    // Plain side-effecting require (`[some.ns]`): never removed (it may load a
    // namespace for its `defmethod`/protocol-extension side effects).
    if alias.is_none() && refer_vec.is_none() && !refer_all {
        return verbatim();
    }

    let full_ns_used = used_prefixes.contains(&ns_name);
    let alias_used = alias
        .as_deref()
        .map(|a| used_prefixes.contains(a))
        .unwrap_or(false);

    let original_refers: Vec<String> = refer_vec
        .map(|v| {
            named_children(v)
                .into_iter()
                .filter(|r| r.kind() == "sym_lit")
                .map(|r| node_text(r, source).to_string())
                .collect()
        })
        .unwrap_or_default();
    let surviving: Vec<String> = original_refers
        .iter()
        .filter(|r| used_bare.contains(*r))
        .cloned()
        .collect();
    let refer_has_use = refer_all || !surviving.is_empty();

    // Nothing the spec provides is used → drop the whole spec.
    if !full_ns_used && !alias_used && !refer_has_use {
        return Plan::Remove;
    }

    // Survives. Prune unused `:refer` names when an explicit vector shrank.
    if let (Some(kwd), Some(vec)) = (refer_kwd, refer_vec) {
        if surviving.len() != original_refers.len() {
            let spec_text = node_text(spec, source);
            let base = spec.start_byte();
            if surviving.is_empty() {
                // Drop the whole `:refer [...]` segment and the space before it.
                let mut cut = kwd.start_byte() - base;
                let bytes = spec_text.as_bytes();
                while cut > 0 && bytes[cut - 1].is_ascii_whitespace() {
                    cut -= 1;
                }
                let after = vec.end_byte() - base;
                return Plan::Keep(format!("{}{}", &spec_text[..cut], &spec_text[after..]));
            }
            let vec_start = vec.start_byte() - base;
            let vec_end = vec.end_byte() - base;
            return Plan::Keep(format!(
                "{}[{}]{}",
                &spec_text[..vec_start],
                surviving.join(" "),
                &spec_text[vec_end..]
            ));
        }
    }

    verbatim()
}

/// Collapses runs of whitespace to single spaces so trivially re-spaced
/// duplicate specs still compare equal.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The top-level `(ns …)` list, if present.
fn ns_form<'a>(root: Node<'a>, source: &str) -> Option<Node<'a>> {
    (0..root.named_child_count())
        .filter_map(|i| root.named_child(i))
        .find(|child| child.kind() == "list_lit" && first_token_is(*child, "sym_lit", "ns", source))
}

/// The `(:require …)` clause inside an ns form, if present.
fn require_clause<'a>(ns_form: Node<'a>, source: &str) -> Option<Node<'a>> {
    named_children(ns_form).into_iter().find(|child| {
        child.kind() == "list_lit" && first_token_is(*child, "kwd_lit", ":require", source)
    })
}

fn first_token_is(node: Node, kind: &str, text: &str, source: &str) -> bool {
    named_children(node)
        .first()
        .map(|first| first.kind() == kind && node_text(*first, source) == text)
        .unwrap_or(false)
}

fn end_position(node: Node, source: &str) -> Position {
    extractor::point_to_position(node.end_position(), node.end_byte(), source)
}

fn empty_range(pos: Position) -> tower_lsp::lsp_types::Range {
    tower_lsp::lsp_types::Range {
        start: pos,
        end: pos,
    }
}

fn named_children(node: Node) -> Vec<Node> {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .collect()
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{DefKind, Occurrence, Symbol, SymbolSource};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tower_lsp::lsp_types::Range;

    fn range() -> Range {
        Range::default()
    }

    fn lib_symbol(ns: &str, name: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            fqn: format!("{}/{}", ns, name),
            ns: ns.to_string(),
            kind: DefKind::Defn,
            params: vec![],
            doc: None,
            file: PathBuf::from(format!("/lib/{}.clj", ns)),
            source: SymbolSource::Dir(PathBuf::from("/lib")),
            range: range(),
            name_range: range(),
        }
    }

    fn lib_meta(ns: &str) -> NsMeta {
        NsMeta {
            name: ns.to_string(),
            file: PathBuf::from(format!("/lib/{}.clj", ns)),
            aliases: HashMap::new(),
            refers: HashMap::new(),
            requires: vec![],
        }
    }

    /// Index with library namespaces clojure.string/join, clojure.set/union,
    /// and a project namespace simple.helpers/greet.
    fn test_index() -> Index {
        let index = Index::new();
        index.insert_lib_file(
            lib_meta("clojure.string"),
            vec![lib_symbol("clojure.string", "join")],
        );
        index.insert_lib_file(
            lib_meta("clojure.set"),
            vec![lib_symbol("clojure.set", "union")],
        );

        let helpers_file = PathBuf::from("/proj/src/simple/helpers.clj");
        let helpers_meta = NsMeta {
            name: "simple.helpers".to_string(),
            file: helpers_file.clone(),
            aliases: HashMap::new(),
            refers: HashMap::new(),
            requires: vec![],
        };
        let greet = Symbol {
            name: "greet".to_string(),
            fqn: "simple.helpers/greet".to_string(),
            ns: "simple.helpers".to_string(),
            kind: DefKind::Defn,
            params: vec![],
            doc: None,
            file: helpers_file,
            source: SymbolSource::Project,
            range: range(),
            name_range: range(),
        };
        index.insert_file(helpers_meta, vec![greet], Vec::<Occurrence>::new());
        index
    }

    fn empty_meta() -> NsMeta {
        NsMeta {
            name: "simple.consumer".to_string(),
            file: PathBuf::from("/proj/src/simple/consumer.clj"),
            aliases: HashMap::new(),
            refers: HashMap::new(),
            requires: vec![],
        }
    }

    #[test]
    fn curated_alias_hit() {
        let index = test_index();
        let got = candidates(&index, &empty_meta(), "str/join");
        assert_eq!(
            got,
            vec![Candidate {
                namespace: "clojure.string".to_string(),
                alias: Some("str".to_string())
            }]
        );
    }

    #[test]
    fn last_segment_hit_for_project_ns() {
        let index = test_index();
        let got = candidates(&index, &empty_meta(), "helpers/greet");
        assert_eq!(
            got,
            vec![Candidate {
                namespace: "simple.helpers".to_string(),
                alias: Some("helpers".to_string())
            }]
        );
    }

    #[test]
    fn fully_qualified_namespace_requires_plainly() {
        let index = test_index();
        let got = candidates(&index, &empty_meta(), "clojure.set/union");
        assert_eq!(
            got,
            vec![Candidate {
                namespace: "clojure.set".to_string(),
                alias: None
            }]
        );
    }

    #[test]
    fn curated_and_last_segment_dedup_to_curated() {
        // `set` is both a curated alias (clojure.set) and the last segment of
        // clojure.set; the result must be a single curated candidate.
        let index = test_index();
        let got = candidates(&index, &empty_meta(), "set/union");
        assert_eq!(
            got,
            vec![Candidate {
                namespace: "clojure.set".to_string(),
                alias: Some("set".to_string())
            }]
        );
    }

    #[test]
    fn no_candidate_when_symbol_absent_in_namespace() {
        let index = test_index();
        assert!(candidates(&index, &empty_meta(), "str/nope").is_empty());
    }

    #[test]
    fn no_candidate_when_alias_already_required() {
        let index = test_index();
        let mut meta = empty_meta();
        meta.aliases
            .insert("str".to_string(), "clojure.string".to_string());
        assert!(candidates(&index, &meta, "str/join").is_empty());
    }

    #[test]
    fn no_candidate_when_namespace_already_required_plainly() {
        let index = test_index();
        let mut meta = empty_meta();
        meta.requires.push("clojure.set".to_string());
        assert!(candidates(&index, &meta, "clojure.set/union").is_empty());
    }

    #[test]
    fn no_candidate_for_clojure_core() {
        let index = test_index();
        assert!(candidates(&index, &empty_meta(), "clojure.core/map").is_empty());
    }

    #[test]
    fn no_candidate_for_unqualified_token() {
        let index = test_index();
        assert!(candidates(&index, &empty_meta(), "join").is_empty());
    }

    #[test]
    fn no_candidate_for_current_namespace() {
        // `simple.helpers/greet` used inside simple.helpers itself must not
        // suggest requiring its own namespace.
        let index = test_index();
        let mut meta = empty_meta();
        meta.name = "simple.helpers".to_string();
        assert!(candidates(&index, &meta, "simple.helpers/greet").is_empty());
    }

    #[test]
    fn no_candidate_for_current_namespace_via_last_segment() {
        // `helpers/greet` inside simple.helpers resolves via last-segment back
        // to the current namespace; it must not suggest requiring itself.
        let index = test_index();
        let mut meta = empty_meta();
        meta.name = "simple.helpers".to_string();
        assert!(candidates(&index, &meta, "helpers/greet").is_empty());
    }

    /// Applies a single insertion `TextEdit` to `source` for assertions.
    fn apply(source: &str, edit: &TextEdit) -> String {
        let start = offset_of(source, edit.range.start);
        let end = offset_of(source, edit.range.end);
        format!("{}{}{}", &source[..start], edit.new_text, &source[end..])
    }

    fn offset_of(source: &str, pos: Position) -> usize {
        let (mut line, mut col) = (0u32, 0u32);
        for (i, ch) in source.char_indices() {
            if line == pos.line && col == pos.character {
                return i;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += ch.len_utf16() as u32;
            }
        }
        source.len()
    }

    #[test]
    fn edit_appends_to_existing_require() {
        let source = "(ns my.app\n  (:require [a.b :as b]))\n\n(b/x)\n";
        let edit = require_edit(source, "[clojure.string :as str]").unwrap();
        assert_eq!(
            apply(source, &edit),
            "(ns my.app\n  (:require [a.b :as b]\n            [clojure.string :as str]))\n\n(b/x)\n"
        );
    }

    #[test]
    fn edit_adds_require_after_docstring() {
        let source = "(ns my.app\n  \"Docs.\")\n\n(foo)\n";
        let edit = require_edit(source, "[clojure.string :as str]").unwrap();
        assert_eq!(
            apply(source, &edit),
            "(ns my.app\n  \"Docs.\"\n  (:require [clojure.string :as str]))\n\n(foo)\n"
        );
    }

    #[test]
    fn edit_adds_require_with_metadata() {
        let source = "(ns ^{:author \"me\"} my.app)\n\n(foo)\n";
        let edit = require_edit(source, "[clojure.string :as str]").unwrap();
        assert_eq!(
            apply(source, &edit),
            "(ns ^{:author \"me\"} my.app\n  (:require [clojure.string :as str]))\n\n(foo)\n"
        );
    }

    #[test]
    fn edit_none_without_ns_form() {
        assert!(require_edit("(defn foo [] 1)\n", "[x]").is_none());
    }

    /// Applies the clean-ns edits (highest position first, so earlier offsets
    /// stay valid) and returns the result, or `None` when nothing would change.
    fn clean(source: &str) -> Option<String> {
        let mut edits = clean_ns_edits(source)?;
        edits.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));
        let mut out = source.to_string();
        for edit in &edits {
            out = apply(&out, edit);
        }
        Some(out)
    }

    #[test]
    fn clean_removes_unused_alias_require() {
        let source = "(ns app\n  (:require [a.b :as b]\n            [c.d :as d]))\n\n(d/run)\n";
        let out = clean(source).expect("expected a clean edit");
        assert!(!out.contains("a.b"), "unused require kept:\n{}", out);
        assert!(
            out.contains("[c.d :as d]"),
            "used require dropped:\n{}",
            out
        );
        assert!(out.contains("(d/run)"), "body changed:\n{}", out);
    }

    #[test]
    fn clean_is_noop_when_alias_used() {
        // `b` is used, so there is nothing to clean → no action.
        let source = "(ns app\n  (:require [a.b :as b]))\n\n(b/run)\n";
        assert!(clean(source).is_none());
    }

    #[test]
    fn clean_prunes_unused_refer_keeping_sibling() {
        let source = "(ns app\n  (:require [c.d :refer [used gone]]))\n\n(used)\n";
        assert_eq!(
            clean(source).as_deref(),
            Some("(ns app\n  (:require [c.d :refer [used]]))\n\n(used)\n")
        );
    }

    #[test]
    fn clean_drops_empty_refer_but_keeps_used_alias() {
        let source = "(ns app\n  (:require [c.d :as d :refer [gone]]))\n\n(d/run)\n";
        assert_eq!(
            clean(source).as_deref(),
            Some("(ns app\n  (:require [c.d :as d]))\n\n(d/run)\n")
        );
    }

    #[test]
    fn clean_removes_spec_whose_only_refer_is_unused() {
        let source =
            "(ns app\n  (:require [c.d :refer [gone]]\n            [e.f :as f]))\n\n(f/go)\n";
        let out = clean(source).expect("expected a clean edit");
        assert!(
            !out.contains("c.d"),
            "unused refer-only spec kept:\n{}",
            out
        );
        assert!(out.contains("[e.f :as f]"), "used spec dropped:\n{}", out);
    }

    #[test]
    fn clean_drops_exact_duplicate_require() {
        let source = "(ns app\n  (:require [c.d :as d]\n            [c.d :as d]))\n\n(d/go)\n";
        assert_eq!(
            clean(source).as_deref(),
            Some("(ns app\n  (:require [c.d :as d]))\n\n(d/go)\n")
        );
    }

    #[test]
    fn clean_keeps_plain_side_effecting_require() {
        // `[c.d]` and bare `some.side` have no alias/refer — possibly loaded for
        // side effects, so they are never removed (and nothing else changes).
        let source = "(ns app\n  (:require [c.d]\n            some.side))\n\n(println 1)\n";
        assert!(clean(source).is_none());
    }

    #[test]
    fn clean_keeps_spec_used_fully_qualified() {
        // The alias `s` is unused, but `clojure.set/union` is — so the spec
        // stays (we don't strip an unused `:as` from an otherwise-used entry).
        let source = "(ns app\n  (:require [clojure.set :as s]))\n\n(clojure.set/union #{} #{})\n";
        assert!(clean(source).is_none());
    }

    #[test]
    fn clean_leaves_reader_conditional_requires_untouched() {
        let source =
            "(ns app\n  (:require #?(:clj [a :as a])\n            [c.d :as d]))\n\n#?(:clj (a/x))\n";
        let out = clean(source).expect("expected a clean edit");
        assert!(
            out.contains("#?(:clj [a :as a])"),
            "reader-conditional require corrupted:\n{}",
            out
        );
        assert!(!out.contains("c.d"), "unused spec kept:\n{}", out);
    }

    #[test]
    fn clean_keeps_alias_used_only_in_keyword() {
        // `s` appears only in the auto-resolved keyword `::s/problem`, which
        // `qualified_usages` (symbols only) never sees. The require must stay.
        let source =
            "(ns app\n  (:require [clojure.spec.alpha :as s]))\n\n(defn f [x] (::s/problem x))\n";
        assert!(clean(source).is_none());
    }

    #[test]
    fn clean_keeps_alias_used_only_in_namespaced_map() {
        // `s` is used only via the namespaced map literal `#::s{…}`.
        let source = "(ns app\n  (:require [my.spec :as s]))\n\n(def x #::s{:a 1})\n";
        assert!(clean(source).is_none());
    }

    #[test]
    fn clean_keeps_libspec_with_unmodeled_option() {
        // `:rename` introduces `j` as a usable name we don't track; the spec
        // must be left untouched rather than wrongly removed/pruned.
        let source =
            "(ns app\n  (:require [clojure.string :refer [join] :rename {join j}]))\n\n(j)\n";
        assert!(clean(source).is_none());
    }

    #[test]
    fn clean_handles_multiple_require_clauses() {
        // First clause is already clean; the second has an unused alias. Both
        // clauses must be considered — the action is offered and only the stale
        // entry is dropped.
        let source = "(ns app\n  (:require [a.b :as b])\n  (:require [c.d :as d]))\n\n(b/x)\n";
        let out = clean(source).expect("expected a clean edit across clauses");
        assert!(
            out.contains("[a.b :as b]"),
            "used require dropped:\n{}",
            out
        );
        assert!(
            !out.contains("c.d"),
            "unused require in 2nd clause kept:\n{}",
            out
        );
    }

    #[test]
    fn clean_dedupes_specs_made_identical_by_refer_pruning() {
        // After dropping the unused `gone`, both specs become `[c.d :refer
        // [used]]`; the duplicate must collapse in this single pass (idempotent).
        let source = "(ns app\n  (:require [c.d :refer [used gone]]\n            \
                      [c.d :refer [used]]))\n\n(used)\n";
        let out = clean(source).expect("expected a clean edit");
        assert_eq!(
            out.matches("c.d").count(),
            1,
            "duplicate after pruning not collapsed:\n{}",
            out
        );
        // And it is genuinely idempotent: a second pass changes nothing.
        assert!(clean(&out).is_none(), "second pass still edits:\n{}", out);
    }

    #[test]
    fn clean_dedupes_across_require_clauses() {
        // The same spec in two clauses: the later one is a duplicate.
        let source = "(ns app\n  (:require [c.d :as d])\n  (:require [c.d :as d]))\n\n(d/go)\n";
        let out = clean(source).expect("expected a clean edit");
        assert_eq!(
            out.matches("[c.d :as d]").count(),
            1,
            "duplicate not dropped:\n{}",
            out
        );
    }

    #[test]
    fn clean_drops_emptied_require_clause_keeping_ns() {
        // Every require is unused → the whole `(:require …)` clause goes, but the
        // `(ns app)` declaration itself is preserved.
        let source = "(ns app\n  (:require [c.d :as d]))\n\n(println 1)\n";
        assert_eq!(clean(source).as_deref(), Some("(ns app)\n\n(println 1)\n"));
    }
}
