use std::collections::HashMap;

use anyhow::Result;
use tower_lsp::lsp_types::*;
use tree_sitter::{Node, Parser};

use crate::document::DocumentStore;
use crate::index::{extractor, Index, NsMeta};

/// Offers an "Add require …" quickfix when the cursor sits on a qualified
/// symbol (`alias/name` or `some.ns/name`) whose namespace is not yet required.
pub fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: CodeActionParams,
) -> Result<Option<CodeActionResponse>> {
    let uri = params.text_document.uri;
    let pos = params.range.start;

    let Some(token) = documents.word_at(&uri, pos) else {
        return Ok(None);
    };
    let Some(text) = documents.text(&uri) else {
        return Ok(None);
    };
    let Ok(path) = uri.to_file_path() else {
        return Ok(None);
    };
    // Resolve against the live buffer: its requires may differ from the index.
    let Ok((ns_meta, _)) = extractor::extract(&text, &path) else {
        return Ok(None);
    };

    let actions: Vec<CodeActionOrCommand> = candidates(index, &ns_meta, &token)
        .into_iter()
        .filter_map(|candidate| {
            let spec = candidate.spec();
            let edit = require_edit(&text, &spec)?;
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![edit]);
            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Add require `{}`", spec),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                ..Default::default()
            }))
        })
        .collect();

    if actions.is_empty() {
        Ok(None)
    } else {
        Ok(Some(actions))
    }
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

    // Already available in this file: the file's own namespace, aliased, or
    // required (with or without an alias — `requires` captures plain
    // `[clojure.set]` too).
    if prefix == ns_meta.name
        || ns_meta.aliases.contains_key(prefix)
        || ns_meta.requires.iter().any(|r| r == prefix)
    {
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
}
