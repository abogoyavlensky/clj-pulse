use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Node, Parser};
use tree_sitter_clojure::LANGUAGE;
use tree_sitter_language::LanguageFn;

use super::{DefKind, NsMeta, Symbol};

static LANGUAGE_REF: OnceLock<tree_sitter::Language> = OnceLock::new();

fn language() -> &'static tree_sitter::Language {
    LANGUAGE_REF.get_or_init(|| {
        let lang_fn: LanguageFn = LANGUAGE;
        lang_fn.into()
    })
}

pub fn extract(source: &str, file: &Path) -> Result<(NsMeta, Vec<Symbol>)> {
    let mut parser = Parser::new();
    parser
        .set_language(language())
        .map_err(|e| anyhow!("failed to set language: {}", e))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("failed to parse"))?;

    let root = tree.root_node();
    let mut ns_meta = NsMeta {
        name: String::new(),
        file: file.to_path_buf(),
        aliases: HashMap::new(),
        refers: HashMap::new(),
    };
    let mut symbols = Vec::new();

    for i in 0..root.named_child_count() {
        let child = root.named_child(i).unwrap();
        match child.kind() {
            "list_lit" => process_top_level_list(child, source, file, &mut ns_meta, &mut symbols),
            "read_cond_lit" => {
                process_reader_conditional(child, source, file, &mut ns_meta, &mut symbols);
            }
            _ => {}
        }
    }

    Ok((ns_meta, symbols))
}

fn process_reader_conditional(
    node: Node,
    source: &str,
    file: &Path,
    ns_meta: &mut NsMeta,
    symbols: &mut Vec<Symbol>,
) {
    let children: Vec<Node> = named_children(node);
    // read_cond_lit contains alternating kwd_lit and form pairs
    let mut i = 0;
    while i + 1 < children.len() {
        let form = &children[i + 1];
        if form.kind() == "list_lit" {
            process_top_level_list(*form, source, file, ns_meta, symbols);
        }
        i += 2;
    }
}

fn process_top_level_list(
    node: Node,
    source: &str,
    file: &Path,
    ns_meta: &mut NsMeta,
    symbols: &mut Vec<Symbol>,
) {
    let children: Vec<Node> = named_children(node);
    if children.is_empty() {
        return;
    }

    let first = children[0];
    if first.kind() != "sym_lit" {
        return;
    }

    let first_text = node_text(first, source);

    if first_text == "ns" {
        extract_ns(&children, source, ns_meta);
    } else if let Some(kind) = str_to_defkind(first_text) {
        extract_def(node, &children, source, file, &ns_meta.name, kind, symbols);
    }
}

fn extract_ns(children: &[Node], source: &str, ns_meta: &mut NsMeta) {
    if children.len() < 2 {
        return;
    }

    let name_node = children[1];
    if name_node.kind() == "sym_lit" {
        ns_meta.name = node_text(name_node, source).to_string();
    }

    // Look for (:require ...) forms
    for child in &children[2..] {
        if child.kind() == "list_lit" {
            let inner = named_children(*child);
            if inner.is_empty() {
                continue;
            }
            let kw = inner[0];
            if kw.kind() == "kwd_lit" && node_text(kw, source) == ":require" {
                for require_spec in &inner[1..] {
                    if require_spec.kind() == "vec_lit" {
                        parse_require_vector(*require_spec, source, ns_meta);
                    }
                }
            }
        }
    }
}

fn parse_require_vector(vec_node: Node, source: &str, ns_meta: &mut NsMeta) {
    let items: Vec<Node> = named_children(vec_node);
    if items.is_empty() {
        return;
    }

    let ns_name = if items[0].kind() == "sym_lit" {
        node_text(items[0], source).to_string()
    } else {
        return;
    };

    let mut i = 1;
    while i < items.len() {
        let item = items[i];
        if item.kind() == "kwd_lit" {
            let kw_text = node_text(item, source);
            match kw_text {
                ":as" => {
                    if i + 1 < items.len() && items[i + 1].kind() == "sym_lit" {
                        let alias = node_text(items[i + 1], source).to_string();
                        ns_meta.aliases.insert(alias, ns_name.clone());
                        i += 2;
                        continue;
                    }
                }
                ":refer" => {
                    if i + 1 < items.len() && items[i + 1].kind() == "vec_lit" {
                        let refer_vec = named_children(items[i + 1]);
                        for refer_node in refer_vec {
                            if refer_node.kind() == "sym_lit" {
                                let refer_name = node_text(refer_node, source).to_string();
                                let fqn = format!("{}/{}", ns_name, refer_name);
                                ns_meta.refers.insert(refer_name, fqn);
                            }
                        }
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
}

fn extract_def(
    form_node: Node,
    children: &[Node],
    source: &str,
    file: &Path,
    ns_name: &str,
    kind: DefKind,
    symbols: &mut Vec<Symbol>,
) {
    if children.len() < 2 {
        return;
    }

    let name_node = children[1];
    if name_node.kind() != "sym_lit" {
        return;
    }

    let name = node_text(name_node, source).to_string();
    let fqn = if ns_name.is_empty() {
        name.clone()
    } else {
        format!("{}/{}", ns_name, name)
    };

    let mut doc: Option<String> = None;
    let mut params: Vec<String> = Vec::new();

    // Walk remaining children to find docstring, params, and multi-arity bodies
    let mut rest_start = 2;

    // Check for docstring (str_lit right after name)
    if rest_start < children.len() && children[rest_start].kind() == "str_lit" {
        let raw = node_text(children[rest_start], source);
        doc = Some(strip_string_quotes(raw));
        rest_start += 1;
    }

    // Check for params: either a direct vec_lit (single arity) or list_lit children (multi-arity)
    let mut found_params = false;
    for child in &children[rest_start..] {
        match child.kind() {
            "vec_lit" if !found_params => {
                params.push(node_text(*child, source).to_string());
                found_params = true;
            }
            "list_lit" => {
                // Multi-arity: each list_lit contains a vec_lit as first child
                let inner = named_children(*child);
                for inner_child in &inner {
                    if inner_child.kind() == "vec_lit" {
                        params.push(node_text(*inner_child, source).to_string());
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    symbols.push(Symbol {
        name,
        fqn,
        ns: ns_name.to_string(),
        kind,
        params,
        doc,
        file: file.to_path_buf(),
        range: node_to_lsp_range(form_node),
        name_range: node_to_lsp_range(name_node),
    });
}

fn strip_string_quotes(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn named_children(node: Node) -> Vec<Node> {
    let mut result = Vec::new();
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            result.push(child);
        }
    }
    result
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

fn node_to_lsp_range(node: Node) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start: Position {
            line: start.row as u32,
            character: start.column as u32,
        },
        end: Position {
            line: end.row as u32,
            character: end.column as u32,
        },
    }
}

fn str_to_defkind(s: &str) -> Option<DefKind> {
    match s {
        "def" => Some(DefKind::Def),
        "defn" => Some(DefKind::Defn),
        "defn-" => Some(DefKind::DefnPrivate),
        "defmacro" => Some(DefKind::Defmacro),
        "defmulti" => Some(DefKind::Defmulti),
        "defmethod" => Some(DefKind::Defmethod),
        "defprotocol" => Some(DefKind::Defprotocol),
        "defrecord" => Some(DefKind::Defrecord),
        "deftype" => Some(DefKind::Deftype),
        _ => None,
    }
}
