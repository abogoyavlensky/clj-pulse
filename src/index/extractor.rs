use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Node, Parser};
use tree_sitter_clojure::LANGUAGE;
use tree_sitter_language::LanguageFn;

use super::{DefKind, ExtractConfig, NsMeta, Occurrence, Symbol};

static LANGUAGE_REF: OnceLock<tree_sitter::Language> = OnceLock::new();

pub(crate) fn language() -> &'static tree_sitter::Language {
    LANGUAGE_REF.get_or_init(|| {
        let lang_fn: LanguageFn = LANGUAGE;
        lang_fn.into()
    })
}

pub fn extract(source: &str, file: &Path) -> Result<(NsMeta, Vec<Symbol>)> {
    extract_full(source, file).map(|(meta, symbols, _)| (meta, symbols))
}

/// A namespace-qualified symbol usage (`str/join`), used by the
/// unresolved-namespace diagnostic. `range` covers the whole symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct QualifiedUsage {
    pub prefix: String,
    pub name: String,
    pub range: Range,
}

/// Collects every namespace-qualified symbol usage in `source`. Skips
/// `'`-quoted data and `(quote …)` forms (which are data, not var usages);
/// syntax-quote is kept, since macro bodies reference real vars.
pub fn qualified_usages(source: &str) -> Vec<QualifiedUsage> {
    let mut parser = Parser::new();
    if parser.set_language(language()).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![];
    };
    let mut out = Vec::new();
    collect_qualified(tree.root_node(), source, &mut out);
    out
}

fn collect_qualified(node: Node, source: &str, out: &mut Vec<QualifiedUsage>) {
    match node.kind() {
        // 'foo/bar is data; #_foo/bar is discarded by the reader.
        "quoting_lit" | "dis_expr" => {}
        "sym_lit" => {
            if let (Some(ns_node), Some(name_node)) = (
                node.child_by_field_name("namespace"),
                node.child_by_field_name("name"),
            ) {
                let prefix = node_text(ns_node, source).to_string();
                let name = node_text(name_node, source).to_string();
                if !prefix.is_empty() && !name.is_empty() {
                    // Range the symbol itself (`foo/bar`), not any leading
                    // metadata/type-hint the sym_lit node also spans.
                    let range = Range {
                        start: point_to_position(
                            ns_node.start_position(),
                            ns_node.start_byte(),
                            source,
                        ),
                        end: point_to_position(
                            name_node.end_position(),
                            name_node.end_byte(),
                            source,
                        ),
                    };
                    out.push(QualifiedUsage {
                        prefix,
                        name,
                        range,
                    });
                }
            }
        }
        "list_lit" => {
            let kids = named_children(node);
            if let Some(first) = kids.first() {
                if first.kind() == "sym_lit" && node_text(*first, source) == "quote" {
                    return; // (quote …) is data
                }
            }
            for child in kids {
                collect_qualified(child, source, out);
            }
        }
        "map_lit" => {
            // Skip `:keys`/`:syms`/`:strs` destructuring vectors: a symbol like
            // `foo/bar` there binds a local from key `:foo/bar`, it isn't a
            // namespace usage. Everything else (including a qualified symbol
            // used as a real map key/value) is still walked.
            let kids = named_children(node);
            let mut i = 0;
            while i < kids.len() {
                let key = kids[i];
                let val = kids.get(i + 1).copied();
                let is_destructure = key.kind() == "kwd_lit"
                    && matches!(node_text(key, source), ":keys" | ":syms" | ":strs")
                    && val.map(|v| v.kind() == "vec_lit").unwrap_or(false);
                collect_qualified(key, source, out);
                if let Some(v) = val {
                    if !is_destructure {
                        collect_qualified(v, source, out);
                    }
                }
                i += 2;
            }
        }
        _ => {
            for child in named_children(node) {
                collect_qualified(child, source, out);
            }
        }
    }
}

/// The reader tag that strongly marks an EDN file as an Integrant system.
const INTEGRANT_REF_TAG: &str = "#ig/ref";

/// Whether `path` is an EDN file that looks like an Integrant system config: not
/// a build manifest, and either containing an `#ig/ref` tag or a top-level map
/// keyed by namespaced keywords. The structural check catches ref-less systems
/// (independent components with no `#ig/ref`); manifests are excluded by name.
pub fn is_integrant_edn(path: &Path, source: &str) -> bool {
    crate::config::is_edn(path)
        && !crate::config::is_build_manifest(path)
        && (source.contains(INTEGRANT_REF_TAG) || has_namespaced_top_level_key(source))
}

/// Whether the first top-level map in `source` has any namespaced-keyword key —
/// the structural signature of an Integrant system map. (Manifests like
/// `deps.edn`/`bb.edn` use unqualified top-level keys.)
fn has_namespaced_top_level_key(source: &str) -> bool {
    let mut parser = Parser::new();
    if parser.set_language(language()).is_err() {
        return false;
    }
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    for top in named_children(tree.root_node()) {
        if top.kind() != "map_lit" {
            continue;
        }
        // map_lit children alternate key, value, …; keys are the even indices.
        return named_children(top)
            .iter()
            .step_by(2)
            .any(|key| key.kind() == "kwd_lit" && key.child_by_field_name("namespace").is_some());
    }
    false
}

/// Extracts qualified keyword occurrences from an EDN file (Integrant/Aero
/// system configs). EDN has no `ns` form or `::` auto-resolution, so only
/// literal `:ns/name` keywords qualify — an empty `NsMeta` makes `keyword_fqn`
/// drop `::`/unqualified keywords. Keywords nested in tagged literals
/// (`#ig/ref :ns/x`), maps, and vectors are all reached by the generic descent.
pub fn extract_edn(source: &str) -> Vec<Occurrence> {
    let mut parser = Parser::new();
    if parser.set_language(language()).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![];
    };
    let empty = NsMeta {
        name: String::new(),
        file: std::path::PathBuf::new(),
        aliases: HashMap::new(),
        refers: HashMap::new(),
        requires: Vec::new(),
        imports: HashMap::new(),
    };
    let mut out = Vec::new();
    collect_edn_keywords(tree.root_node(), source, &empty, &mut out);
    out
}

fn collect_edn_keywords(node: Node, source: &str, ns_meta: &NsMeta, out: &mut Vec<Occurrence>) {
    if node.kind() == "kwd_lit" {
        if let Some(fqn) = keyword_fqn(node, ns_meta, source) {
            out.push(Occurrence {
                fqn,
                name_range: node_to_lsp_range(node, source),
            });
        }
        return;
    }
    for child in named_children(node) {
        collect_edn_keywords(child, source, ns_meta, out);
    }
}

/// Occurrences for any indexed file, dispatching on extension: Integrant EDN
/// configs use [`extract_edn`]; Clojure sources use the full extractor's
/// occurrence pass. Used to re-extract open buffers in references/definition.
///
/// EDN extraction applies the same `#ig/ref` gate as startup/open/save indexing,
/// so an open build manifest (`deps.edn`, `bb.edn`) never leaks keyword
/// occurrences into references.
pub fn file_occurrences(source: &str, path: &Path) -> Vec<Occurrence> {
    file_occurrences_with(source, path, &ExtractConfig::default())
}

/// Like [`file_occurrences`] but honors `cfg` (`:lint-as`) for Clojure sources.
pub fn file_occurrences_with(source: &str, path: &Path, cfg: &ExtractConfig) -> Vec<Occurrence> {
    if crate::config::is_edn(path) {
        if is_integrant_edn(path, source) {
            extract_edn(source)
        } else {
            Vec::new()
        }
    } else {
        extract_full_with(source, path, cfg)
            .map(|(_, _, occs)| occs)
            .unwrap_or_default()
    }
}

/// Like [`extract`] but also collects every resolved symbol usage
/// (occurrences) in a second pass over the same parse tree. Uses the default
/// (empty) [`ExtractConfig`]; call [`extract_full_with`] to honor `:lint-as`.
pub fn extract_full(source: &str, file: &Path) -> Result<(NsMeta, Vec<Symbol>, Vec<Occurrence>)> {
    extract_full_with(source, file, &ExtractConfig::default())
}

/// Like [`extract_full`] but honors `cfg`. The only setting consulted today is
/// `:lint-as`: a list head whose fqn maps to a `def`-family kind is extracted as
/// a definition (and still recorded as a usage), so names introduced by custom
/// macros become navigable.
pub fn extract_full_with(
    source: &str,
    file: &Path,
    cfg: &ExtractConfig,
) -> Result<(NsMeta, Vec<Symbol>, Vec<Occurrence>)> {
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
        requires: Vec::new(),
        imports: HashMap::new(),
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

    // Second pass: occurrences, resolved through the completed ns metadata
    let def_names: HashSet<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    let ctx = OccurrenceCtx {
        source,
        ns_meta: &ns_meta,
        def_names,
        lint_as: &cfg.lint_as,
    };
    let mut occurrences = Vec::new();
    let mut scope: Vec<HashSet<String>> = Vec::new();
    for i in 0..root.named_child_count() {
        let child = root.named_child(i).unwrap();
        walk_occurrences(child, &ctx, &mut scope, &mut occurrences);
    }

    Ok((ns_meta, symbols, occurrences))
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
        let is_defmethod = kind == DefKind::Defmethod;
        extract_def(node, &children, source, file, &ns_meta.name, kind, symbols);
        if is_defmethod {
            extract_integrant_key(node, &children, source, file, ns_meta, symbols);
        }
    }
}

fn extract_ns(children: &[Node], source: &str, ns_meta: &mut NsMeta) {
    if children.len() < 2 {
        return;
    }

    let name_node = children[1];
    if name_node.kind() == "sym_lit" {
        ns_meta.name = sym_text(name_node, source).to_string();
    }

    // Look for (:require …) and (:import …) forms
    for child in &children[2..] {
        if child.kind() == "list_lit" {
            let inner = named_children(*child);
            if inner.is_empty() {
                continue;
            }
            let kw = inner[0];
            if kw.kind() != "kwd_lit" {
                continue;
            }
            match node_text(kw, source) {
                ":require" => {
                    for require_spec in &inner[1..] {
                        process_require_spec(*require_spec, source, ns_meta);
                    }
                }
                ":import" => {
                    for import_spec in &inner[1..] {
                        process_import_spec(*import_spec, source, ns_meta);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Records one `:require` spec into `ns_meta`. Handles plain libspecs
/// (`[a.b :as x]`), bare namespaces (`clojure.set`), and reader conditionals
/// (`#?(:clj [a :as x])` / `#?@(:clj [[a :as x]])`) — every branch's aliases
/// are recorded so conditional requires aren't reported as unresolved. (Legacy
/// prefix-list libspecs `(clojure set)` are still not expanded.)
fn process_require_spec(spec: Node, source: &str, ns_meta: &mut NsMeta) {
    match spec.kind() {
        "vec_lit" => {
            let items = named_children(spec);
            match items.first().map(|n| n.kind()) {
                // [a.b :as x] — a single libspec.
                Some("sym_lit") => parse_require_vector(spec, source, ns_meta),
                // [[a.b :as x] [c.d]] — a vector of specs spliced by #?@.
                Some("vec_lit") => {
                    for item in items {
                        process_require_spec(item, source, ns_meta);
                    }
                }
                _ => {}
            }
        }
        "sym_lit" => ns_meta.requires.push(sym_text(spec, source).to_string()),
        // Reader conditional: descend into each branch's form (skip platform
        // keywords). Other shapes (e.g. legacy prefix-lists `(clojure set)`)
        // are unsupported and intentionally record nothing, so they don't
        // mask real unresolved-namespace diagnostics.
        "read_cond_lit" | "splicing_read_cond_lit" => {
            for child in named_children(spec) {
                if child.kind() != "kwd_lit" {
                    process_require_spec(child, source, ns_meta);
                }
            }
        }
        _ => {}
    }
}

/// Records one `:import` spec into `ns_meta.imports` (class simple name → fully
/// qualified name). Handles the package-grouped forms `[java.util Date List]`
/// and `(java.util Date List)`, and a bare fully-qualified class `java.io.File`.
fn process_import_spec(spec: Node, source: &str, ns_meta: &mut NsMeta) {
    match spec.kind() {
        "vec_lit" | "list_lit" => {
            let items = named_children(spec);
            let Some(pkg) = items.first() else {
                return;
            };
            if pkg.kind() != "sym_lit" {
                return;
            }
            let package = sym_text(*pkg, source);
            for class in &items[1..] {
                if class.kind() == "sym_lit" {
                    let simple = sym_text(*class, source).to_string();
                    let fqn = format!("{}.{}", package, simple);
                    ns_meta.imports.insert(simple, fqn);
                }
            }
        }
        "sym_lit" => {
            let fqn = sym_text(spec, source);
            if let Some((_, simple)) = fqn.rsplit_once('.') {
                ns_meta.imports.insert(simple.to_string(), fqn.to_string());
            }
        }
        _ => {}
    }
}

fn parse_require_vector(vec_node: Node, source: &str, ns_meta: &mut NsMeta) {
    let items: Vec<Node> = named_children(vec_node);
    if items.is_empty() {
        return;
    }

    let ns_name = if items[0].kind() == "sym_lit" {
        sym_text(items[0], source).to_string()
    } else {
        return;
    };
    ns_meta.requires.push(ns_name.clone());

    let mut i = 1;
    while i < items.len() {
        let item = items[i];
        if item.kind() == "kwd_lit" {
            let kw_text = node_text(item, source);
            match kw_text {
                ":as" if i + 1 < items.len() && items[i + 1].kind() == "sym_lit" => {
                    let alias = node_text(items[i + 1], source).to_string();
                    ns_meta.aliases.insert(alias, ns_name.clone());
                    i += 2;
                    continue;
                }
                ":refer" if i + 1 < items.len() && items[i + 1].kind() == "vec_lit" => {
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

    let name = sym_text(name_node, source).to_string();
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
        kind: kind.clone(),
        params,
        doc,
        file: file.to_path_buf(),
        source: super::SymbolSource::Project,
        range: node_to_lsp_range(form_node, source),
        name_range: node_to_lsp_range(sym_name_node(name_node), source),
    });

    // A protocol's method signatures are namespace-level vars too; index each
    // so go-to-definition / hover / completion / references reach them.
    if kind == DefKind::Defprotocol {
        extract_protocol_methods(&children[2..], source, file, ns_name, symbols);
    }
}

/// The Integrant lifecycle multimethod whose `defmethod` we treat as the
/// canonical *definition* of a component keyword. Other lifecycle methods
/// (`halt-key!`, `assert-key`, …) dispatch on the same keyword but are recorded
/// as occurrences, so go-to-definition lands on the constructor.
const INTEGRANT_INIT_KEY: &str = "integrant.core/init-key";

/// Resolves the multimethod a `defmethod` extends to its fqn (e.g.
/// `integrant.core/init-key`) — the single hook point for keyword-defining
/// macros (re-frame `reg-*`, spec `s/def` would slot in here). A qualified head
/// resolves its `:as` alias; a bare head resolves a `:refer`. `None` when the
/// head is missing or unresolvable.
fn defmethod_multifn_fqn(children: &[Node], ns_meta: &NsMeta, source: &str) -> Option<String> {
    let head = children.get(1).filter(|n| n.kind() == "sym_lit")?;
    let name = node_text(sym_name_node(*head), source);
    if let Some(ns_node) = head.child_by_field_name("namespace") {
        let alias = node_text(ns_node, source);
        let ns = ns_meta
            .aliases
            .get(alias)
            .map(String::as_str)
            .unwrap_or(alias);
        Some(format!("{}/{}", ns, name))
    } else {
        ns_meta.refers.get(name).cloned()
    }
}

/// Records `(defmethod ig/init-key ::x …)` as the definition of the Integrant
/// component keyword `:ns/x`. No-op for any other multimethod or a non-qualified
/// dispatch value.
fn extract_integrant_key(
    form_node: Node,
    children: &[Node],
    source: &str,
    file: &Path,
    ns_meta: &NsMeta,
    symbols: &mut Vec<Symbol>,
) {
    if defmethod_multifn_fqn(children, ns_meta, source).as_deref() != Some(INTEGRANT_INIT_KEY) {
        return;
    }
    let Some(dispatch) = children.get(2).filter(|n| n.kind() == "kwd_lit") else {
        return;
    };
    let Some(fqn) = keyword_fqn(*dispatch, ns_meta, source) else {
        return;
    };

    // `fqn` is `:ns/name`; split off the colon to fill the ns/name fields.
    let (ns, name) = fqn[1..].rsplit_once('/').unwrap_or(("", &fqn[1..]));
    symbols.push(Symbol {
        name: name.to_string(),
        fqn: fqn.clone(),
        ns: ns.to_string(),
        kind: DefKind::IntegrantKey,
        params: Vec::new(),
        doc: None,
        file: file.to_path_buf(),
        source: super::SymbolSource::Project,
        range: node_to_lsp_range(form_node, source),
        // Whole-keyword range so goto-definition lands on (and references list)
        // the full `::name` dispatch token.
        name_range: node_to_lsp_range(*dispatch, source),
    });
}

/// Extracts each method signature of a `defprotocol` as a `Defn` symbol.
/// `rest` is the protocol form's children after the name; method signatures are
/// the `list_lit`s among them — a leading doc string and `:option value` pairs
/// are skipped. Each method `list_lit` is `(name [params]+ docstring?)`.
fn extract_protocol_methods(
    rest: &[Node],
    source: &str,
    file: &Path,
    ns_name: &str,
    symbols: &mut Vec<Symbol>,
) {
    for sig in rest.iter().filter(|n| n.kind() == "list_lit") {
        let inner = named_children(*sig);
        let Some(name_node) = inner.first().filter(|n| n.kind() == "sym_lit") else {
            continue;
        };

        let name = sym_text(*name_node, source).to_string();
        let fqn = if ns_name.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", ns_name, name)
        };

        let params: Vec<String> = inner
            .iter()
            .filter(|n| n.kind() == "vec_lit")
            .map(|n| node_text(*n, source).to_string())
            .collect();
        let doc = inner
            .iter()
            .rev()
            .find(|n| n.kind() == "str_lit")
            .map(|n| strip_string_quotes(node_text(*n, source)));

        symbols.push(Symbol {
            name,
            fqn,
            ns: ns_name.to_string(),
            kind: DefKind::Defn,
            params,
            doc,
            file: file.to_path_buf(),
            source: super::SymbolSource::Project,
            range: node_to_lsp_range(*sig, source),
            name_range: node_to_lsp_range(sym_name_node(*name_node), source),
        });
    }
}

/// For a `sym_lit` carrying metadata (`^:private foo`, `^{:doc "…"} my.ns`)
/// the node's text spans the metadata too; the symbol itself is the `name`
/// field. Returns the name node, or the node itself when there is no field.
fn sym_name_node(node: Node) -> Node {
    node.child_by_field_name("name").unwrap_or(node)
}

fn sym_text<'a>(node: Node, source: &'a str) -> &'a str {
    node_text(sym_name_node(node), source)
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

fn node_to_lsp_range(node: Node, source: &str) -> Range {
    Range {
        start: point_to_position(node.start_position(), node.start_byte(), source),
        end: point_to_position(node.end_position(), node.end_byte(), source),
    }
}

/// Tree-sitter columns are bytes; LSP wants UTF-16 code units. Re-measures
/// the line prefix (from line start to the node boundary) in UTF-16.
pub(crate) fn point_to_position(
    point: tree_sitter::Point,
    byte_offset: usize,
    source: &str,
) -> Position {
    let line_start = byte_offset - point.column;
    let character = source
        .get(line_start..byte_offset)
        .map(|prefix| prefix.encode_utf16().count())
        .unwrap_or(point.column);
    Position {
        line: point.row as u32,
        character: character as u32,
    }
}

// --- keyword resolution ----------------------------------------------------

/// Resolves a `kwd_lit` node to its canonical colon-prefixed fqn (`:ns/name`),
/// or `None` for an unqualified keyword (`:foo`, which is too ambiguous to
/// index cross-file). The leading `:` keeps keyword fqns from ever colliding
/// with var fqns (`ns/name`) in the index.
///
/// Auto-resolved keywords (`::`) resolve their namespace: bare `::foo` uses the
/// current namespace, `::alias/foo` resolves the alias. A single-colon
/// namespace (`:lib.ns/foo`) is literal and never alias-resolved.
fn keyword_fqn(node: Node, ns_meta: &NsMeta, source: &str) -> Option<String> {
    let name = node_text(node.child_by_field_name("name")?, source);
    let auto_resolved = node
        .child_by_field_name("marker")
        .map(|m| node_text(m, source) == "::")
        .unwrap_or(false);

    match node.child_by_field_name("namespace") {
        Some(ns_node) => {
            let ns = node_text(ns_node, source);
            let ns = if auto_resolved {
                ns_meta.aliases.get(ns).map(String::as_str).unwrap_or(ns)
            } else {
                ns
            };
            Some(format!(":{}/{}", ns, name))
        }
        None if auto_resolved => {
            if ns_meta.name.is_empty() {
                None
            } else {
                Some(format!(":{}/{}", ns_meta.name, name))
            }
        }
        None => None,
    }
}

// --- occurrence collection -------------------------------------------------

struct OccurrenceCtx<'a> {
    source: &'a str,
    ns_meta: &'a NsMeta,
    def_names: HashSet<&'a str>,
    /// Macro fqn → `def`-family kind, from the merged `:lint-as` config. Read by
    /// `walk_list` to treat a lint-as'd form as a definition. Empty by default.
    lint_as: &'a HashMap<String, DefKind>,
}

static CORE_NAMES: OnceLock<HashSet<String>> = OnceLock::new();

fn core_names() -> &'static HashSet<String> {
    CORE_NAMES.get_or_init(|| {
        super::core::core_symbols()
            .into_iter()
            .map(|c| c.name)
            .collect()
    })
}

/// Forms whose second child is a binding vector with `[pattern expr …]` pairs.
fn is_let_like(head: &str) -> bool {
    // `binding`/`with-redefs` are deliberately NOT here: they rebind
    // existing Vars, so their left-hand symbols are usages, not locals.
    matches!(
        head,
        "let"
            | "loop"
            | "for"
            | "doseq"
            | "when-let"
            | "if-let"
            | "when-some"
            | "if-some"
            | "with-open"
            | "dotimes"
    )
}

fn walk_occurrences(
    node: Node,
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    match node.kind() {
        "sym_lit" => record_occurrence(node, ctx, scope, out),
        // Every qualified keyword is a usage (`:lib/x`, `::x`, `::alias/x`);
        // unqualified ones are skipped by `keyword_fqn`. This powers keyword
        // references and feeds Integrant component navigation.
        "kwd_lit" => record_keyword_occurrence(node, ctx, out),
        "list_lit" => walk_list(node, ctx, scope, out),
        // 'foo quotes data, not a var usage; skip. Syntax-quoted forms in
        // macros do reference real vars, so walk those.
        "quoting_lit" => {}
        _ => {
            for child in named_children(node) {
                walk_occurrences(child, ctx, scope, out);
            }
        }
    }
}

/// Whether a `sym_lit` list head names a core/special form: unqualified, or
/// qualified to `clojure.core` (directly or via an `:as` alias). Keeps
/// `clojure.core/let` binding locals while excluding `s/def` and friends.
fn head_is_core_form(head: Node, ctx: &OccurrenceCtx) -> bool {
    match head.child_by_field_name("namespace") {
        None => true,
        Some(ns_node) => {
            let alias = node_text(ns_node, ctx.source);
            let resolved = ctx
                .ns_meta
                .aliases
                .get(alias)
                .map(String::as_str)
                .unwrap_or(alias);
            resolved == "clojure.core"
        }
    }
}

fn walk_list(
    node: Node,
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    let children = named_children(node);
    let Some(head) = children.first() else { return };

    // A head names a core/special form only when it is unqualified or qualified
    // to `clojure.core`. Matching on the name part alone would misread a
    // qualified call like `s/def` as core `def` (skipping the keyword in its
    // "name" slot); requiring clojure.core still handles a `clojure.core/let`
    // (or an alias to it) as a real binding form. Other qualified heads fall
    // through to the generic walk, which records them and every argument
    // (including keywords) as occurrences.
    let head_text = if head.kind() == "sym_lit" && head_is_core_form(*head, ctx) {
        Some(sym_text(*head, ctx.source))
    } else {
        None
    };

    match head_text {
        Some("ns") => collect_refer_occurrences(&children, ctx, out),
        Some("quote") => {}
        Some("letfn") => {
            record_occurrence(*head, ctx, scope, out);
            walk_letfn_form(&children, ctx, scope, out);
        }
        Some(t) if str_to_defkind(t).is_some() => {
            walk_def_form(str_to_defkind(t).unwrap(), &children, ctx, scope, out);
        }
        Some(t) if is_let_like(t) => {
            record_occurrence(*head, ctx, scope, out);
            walk_let_form(&children, ctx, scope, out);
        }
        Some("fn") => {
            record_occurrence(*head, ctx, scope, out);
            walk_fn_form(&children, ctx, scope, out);
        }
        Some("extend-type") => {
            record_occurrence(*head, ctx, scope, out);
            // (extend-type Type & specs): Type is an occurrence; the specs
            // interleave protocols and their method impls.
            if let Some(ty) = children.get(1) {
                walk_occurrences(*ty, ctx, scope, out);
            }
            if children.len() > 2 {
                walk_type_specs(&children[2..], SpecMode::Interleaved, ctx, scope, out);
            }
        }
        Some("extend-protocol") => {
            record_occurrence(*head, ctx, scope, out);
            // (extend-protocol Proto & specs): one protocol fixed for all
            // methods; the interleaved symbols are types.
            let proto_ns = children.get(1).and_then(|p| {
                walk_occurrences(*p, ctx, scope, out);
                (p.kind() == "sym_lit")
                    .then(|| protocol_ns(*p, ctx))
                    .flatten()
            });
            if children.len() > 2 {
                walk_type_specs(&children[2..], SpecMode::Fixed(proto_ns), ctx, scope, out);
            }
        }
        Some("reify") => {
            record_occurrence(*head, ctx, scope, out);
            if children.len() > 1 {
                walk_type_specs(&children[1..], SpecMode::Interleaved, ctx, scope, out);
            }
        }
        _ => {
            for child in &children {
                walk_occurrences(*child, ctx, scope, out);
            }
        }
    }
}

/// `(def name …)` / `(defn name [params] body…)`: the name is a definition,
/// not an occurrence. Only function-like forms (and record/type field
/// vectors) treat a leading vector as bindings — for plain `def`/`defonce`/
/// `defmulti`/`defprotocol` a vector is an initializer expression whose
/// contents are usages.
fn walk_def_form(
    kind: DefKind,
    children: &[Node],
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    // A defprotocol body is only method *declarations* (signatures, no bodies),
    // each indexed as its own def. Walking it would record those declarations
    // as usages, double-counting them in references/rename. There are no real
    // usages to find, so skip the body entirely.
    if kind == DefKind::Defprotocol {
        return;
    }

    // (defrecord Name [fields] & specs) / (deftype …): bind the fields, then
    // walk the protocol/method specs (impl heads resolve to their protocol).
    if matches!(kind, DefKind::Defrecord | DefKind::Deftype) {
        let mut frame = HashSet::new();
        if let Some(fields) = children.get(2).filter(|n| n.kind() == "vec_lit") {
            collect_binding_names(*fields, ctx, scope, out, &mut frame);
        }
        scope.push(frame);
        if children.len() > 3 {
            walk_type_specs(&children[3..], SpecMode::Interleaved, ctx, scope, out);
        }
        scope.pop();
        return;
    }

    let binds_vector = matches!(
        kind,
        DefKind::Defn | DefKind::DefnPrivate | DefKind::Defmacro | DefKind::Defmethod
    );
    if !binds_vector {
        for child in children.iter().skip(2) {
            walk_occurrences(*child, ctx, scope, out);
        }
        return;
    }

    // (defmethod name dispatch-val [params] …): the name is a *reference*
    // to the multimethod (rename must update it), and the dispatch value is
    // an expression, even when it's a vector.
    let mut rest_start = 2;
    if kind == DefKind::Defmethod {
        if let Some(name) = children.get(1).filter(|n| n.kind() == "sym_lit") {
            record_occurrence(*name, ctx, scope, out);
        }
        if let Some(dispatch) = children.get(2) {
            // The `ig/init-key` dispatch keyword is the component's *definition*
            // (recorded as an IntegrantKey symbol in the symbol pass); skip it
            // here so references doesn't list the declaration twice. Every other
            // dispatch keyword falls through to the general keyword arm.
            let is_init_key_def = dispatch.kind() == "kwd_lit"
                && defmethod_multifn_fqn(children, ctx.ns_meta, ctx.source).as_deref()
                    == Some(INTEGRANT_INIT_KEY)
                && keyword_fqn(*dispatch, ctx.ns_meta, ctx.source).is_some();
            if !is_init_key_def {
                walk_occurrences(*dispatch, ctx, scope, out);
            }
        }
        rest_start = 3;
    }

    let mut frame_pushed = false;
    for child in children.iter().skip(rest_start) {
        match child.kind() {
            "vec_lit" if !frame_pushed => {
                // Single-arity params: bind for the remaining body
                let mut frame = HashSet::new();
                collect_binding_names(*child, ctx, scope, out, &mut frame);
                scope.push(frame);
                frame_pushed = true;
            }
            "list_lit" if arity_body(*child) => {
                // Multi-arity: ([params] body…) — bind per arity
                let inner = named_children(*child);
                let mut frame = HashSet::new();
                if let Some(params) = inner.first() {
                    collect_binding_names(*params, ctx, scope, out, &mut frame);
                }
                scope.push(frame);
                for body in inner.iter().skip(1) {
                    walk_occurrences(*body, ctx, scope, out);
                }
                scope.pop();
            }
            _ => walk_occurrences(*child, ctx, scope, out),
        }
    }
    if frame_pushed {
        scope.pop();
    }
}

fn arity_body(node: Node) -> bool {
    named_children(node)
        .first()
        .map(|n| n.kind() == "vec_lit")
        .unwrap_or(false)
}

/// How to interpret the leading symbols among a type form's specs.
enum SpecMode {
    /// Each symbol names a protocol/interface; methods belong to the most
    /// recent one (`defrecord`/`deftype`/`extend-type`/`reify`).
    Interleaved,
    /// One protocol is fixed for every method; symbols are types
    /// (`extend-protocol`). Carries the protocol's resolved namespace.
    Fixed(Option<String>),
}

/// Walks the protocol/method specs of a type form: a leading `sym_lit` is a
/// protocol or type (recorded as an occurrence), and a `list_lit` is a method
/// impl resolved against the current protocol's namespace.
fn walk_type_specs(
    specs: &[Node],
    mode: SpecMode,
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    let interleaved = matches!(mode, SpecMode::Interleaved);
    let mut current: Option<String> = match mode {
        SpecMode::Fixed(ns) => ns,
        SpecMode::Interleaved => None,
    };
    for spec in specs {
        match spec.kind() {
            "sym_lit" => {
                record_occurrence(*spec, ctx, scope, out);
                if interleaved {
                    current = protocol_ns(*spec, ctx);
                }
            }
            "list_lit" => walk_method_impl(*spec, current.as_deref(), ctx, scope, out),
            _ => walk_occurrences(*spec, ctx, scope, out),
        }
    }
}

/// A single method impl `(name [params] body…)`: records the head against the
/// protocol's namespace (skipped when unknown — e.g. `Object`/interfaces, so no
/// phantom occurrence is created), binds the params, and walks the body.
fn walk_method_impl(
    list: Node,
    proto_ns: Option<&str>,
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    let inner = named_children(list);
    let Some(name_node) = inner.first().filter(|n| n.kind() == "sym_lit") else {
        // Not a method-impl shape; walk its children generically.
        for child in &inner {
            walk_occurrences(*child, ctx, scope, out);
        }
        return;
    };

    if let Some(ns) = proto_ns {
        let nn = sym_name_node(*name_node);
        out.push(Occurrence {
            fqn: format!("{}/{}", ns, node_text(nn, ctx.source)),
            name_range: node_to_lsp_range(nn, ctx.source),
        });
    }

    let rest = &inner[1..];
    if rest.first().map(|n| n.kind() == "vec_lit").unwrap_or(false) {
        // Single arity: (name [params] body…).
        let mut frame = HashSet::new();
        collect_binding_names(rest[0], ctx, scope, out, &mut frame);
        scope.push(frame);
        for body in rest.iter().skip(1) {
            walk_occurrences(*body, ctx, scope, out);
        }
        scope.pop();
    } else {
        // Multi-arity: (name ([params] body…) ([params] body…) …) — bind each
        // arity's params for its own body, like `defn`.
        for arity in rest {
            if arity.kind() == "list_lit" && arity_body(*arity) {
                let parts = named_children(*arity);
                let mut frame = HashSet::new();
                if let Some(params) = parts.first() {
                    collect_binding_names(*params, ctx, scope, out, &mut frame);
                }
                scope.push(frame);
                for body in parts.iter().skip(1) {
                    walk_occurrences(*body, ctx, scope, out);
                }
                scope.pop();
            } else {
                walk_occurrences(*arity, ctx, scope, out);
            }
        }
    }
}

/// The namespace a protocol symbol's methods live in: a qualified `a/B`
/// resolves its alias; a bare `B` uses a `:refer`'s namespace or, if `B` is a
/// current-file def, the current namespace. `None` for interfaces/`Object` or
/// otherwise unresolved bare symbols.
fn protocol_ns(sym: Node, ctx: &OccurrenceCtx) -> Option<String> {
    if let Some(ns_node) = sym.child_by_field_name("namespace") {
        let alias = node_text(ns_node, ctx.source);
        return Some(
            ctx.ns_meta
                .aliases
                .get(alias)
                .cloned()
                .unwrap_or_else(|| alias.to_string()),
        );
    }
    let name = node_text(sym_name_node(sym), ctx.source);
    if let Some(fqn) = ctx.ns_meta.refers.get(name) {
        return fqn.rsplit_once('/').map(|(ns, _)| ns.to_string());
    }
    if ctx.def_names.contains(name) {
        return Some(ctx.ns_meta.name.clone());
    }
    None
}

/// `(let [pattern expr …] body…)`: RHS expressions are usages evaluated with
/// the bindings accumulated so far; LHS patterns bind.
fn walk_let_form(
    children: &[Node],
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    scope.push(HashSet::new());
    if let Some(bindings) = children.get(1).filter(|n| n.kind() == "vec_lit") {
        process_binding_pairs(*bindings, ctx, scope, out);
    }
    for body in children.iter().skip(2) {
        walk_occurrences(*body, ctx, scope, out);
    }
    scope.pop();
}

/// Processes a `[pattern expr …]` binding vector: RHS expressions are
/// usages, LHS patterns extend the current (innermost) scope frame.
/// Comprehension modifiers are handled: `:let [..]` recurses as a nested
/// binding vector; `:when`/`:while` expressions are plain usages.
fn process_binding_pairs(
    bindings: Node,
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    let items = named_children(bindings);
    for pair in items.chunks(2) {
        let [lhs, rhs] = pair else { continue };
        if lhs.kind() == "kwd_lit" {
            let kw = node_text(*lhs, ctx.source);
            if kw == ":let" && rhs.kind() == "vec_lit" {
                process_binding_pairs(*rhs, ctx, scope, out);
            } else {
                walk_occurrences(*rhs, ctx, scope, out);
            }
            continue;
        }
        walk_occurrences(*rhs, ctx, scope, out);
        let mut names = HashSet::new();
        collect_binding_names(*lhs, ctx, scope, out, &mut names);
        scope.last_mut().unwrap().extend(names);
    }
}

/// `(fn name? [params] body…)` — optional self-name and params bind.
fn walk_fn_form(
    children: &[Node],
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    let mut frame = HashSet::new();
    let mut rest_start = 1;
    if let Some(name) = children.get(1).filter(|n| n.kind() == "sym_lit") {
        frame.insert(sym_text(*name, ctx.source).to_string());
        rest_start = 2;
    }
    scope.push(frame);
    walk_fn_tail(&children[rest_start..], ctx, scope, out);
    scope.pop();
}

/// Params + bodies of a fn-like form (after the optional name): a leading
/// vector binds params; `([params] body…)` lists are per-arity scopes.
/// Assumes the caller pushed a scope frame.
fn walk_fn_tail(
    parts: &[Node],
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    let mut params_bound = false;
    for child in parts {
        match child.kind() {
            "vec_lit" if !params_bound => {
                let mut names = HashSet::new();
                collect_binding_names(*child, ctx, scope, out, &mut names);
                scope.last_mut().unwrap().extend(names);
                params_bound = true;
            }
            "list_lit" if arity_body(*child) => {
                let inner = named_children(*child);
                let mut arity_frame = HashSet::new();
                if let Some(params) = inner.first() {
                    collect_binding_names(*params, ctx, scope, out, &mut arity_frame);
                }
                scope.push(arity_frame);
                for body in inner.iter().skip(1) {
                    walk_occurrences(*body, ctx, scope, out);
                }
                scope.pop();
            }
            _ => walk_occurrences(*child, ctx, scope, out),
        }
    }
}

/// `(letfn [(name [params] body…) …] body…)`: the fn names are mutually
/// recursive locals visible in every fn body and the letfn body.
fn walk_letfn_form(
    children: &[Node],
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
) {
    let fn_specs: Vec<Node> = children
        .get(1)
        .filter(|n| n.kind() == "vec_lit")
        .map(|n| named_children(*n))
        .unwrap_or_default();

    let mut frame = HashSet::new();
    for spec in &fn_specs {
        if spec.kind() == "list_lit" {
            if let Some(name) = named_children(*spec)
                .first()
                .filter(|n| n.kind() == "sym_lit")
            {
                frame.insert(sym_text(*name, ctx.source).to_string());
            }
        }
    }
    scope.push(frame);

    for spec in &fn_specs {
        if spec.kind() != "list_lit" {
            continue;
        }
        let inner = named_children(*spec);
        scope.push(HashSet::new());
        walk_fn_tail(&inner[1..], ctx, scope, out);
        scope.pop();
    }
    for body in children.iter().skip(2) {
        walk_occurrences(*body, ctx, scope, out);
    }
    scope.pop();
}

/// Collects every symbol inside a binding pattern (plain names, vector and
/// map destructuring) except `&` and `_`. Map destructuring `:or` defaults
/// are *expressions*, recorded as occurrences rather than bindings.
fn collect_binding_names(
    pattern: Node,
    ctx: &OccurrenceCtx,
    scope: &mut Vec<HashSet<String>>,
    out: &mut Vec<Occurrence>,
    names: &mut HashSet<String>,
) {
    match pattern.kind() {
        "sym_lit" => {
            let name = sym_text(pattern, ctx.source);
            if name != "&" && name != "_" {
                names.insert(name.to_string());
            }
        }
        "map_lit" => {
            let items = named_children(pattern);
            for pair in items.chunks(2) {
                let [k, v] = pair else { continue };
                if k.kind() == "kwd_lit" {
                    if node_text(*k, ctx.source) == ":or" && v.kind() == "map_lit" {
                        // {:or {name default-expr}}: names bind, defaults
                        // are usages
                        for default in named_children(*v).chunks(2) {
                            let [dk, dv] = default else { continue };
                            if dk.kind() == "sym_lit" {
                                names.insert(sym_text(*dk, ctx.source).to_string());
                            }
                            walk_occurrences(*dv, ctx, scope, out);
                        }
                    } else {
                        // :keys/:strs/:syms vectors, :as name, …
                        collect_binding_names(*v, ctx, scope, out, names);
                    }
                } else {
                    // {pattern :key} — the pattern binds; the key is a keyword
                    // usage (a reference to that key), so record it.
                    collect_binding_names(*k, ctx, scope, out, names);
                    if v.kind() == "kwd_lit" {
                        record_keyword_occurrence(*v, ctx, out);
                    }
                }
            }
        }
        _ => {
            for child in named_children(pattern) {
                collect_binding_names(child, ctx, scope, out, names);
            }
        }
    }
}

/// `(:require [some.ns :refer [a b]])` — refer entries are occurrences of
/// `some.ns/a` etc., so rename can fix require clauses.
fn collect_refer_occurrences(children: &[Node], ctx: &OccurrenceCtx, out: &mut Vec<Occurrence>) {
    for child in children.iter().skip(2) {
        if child.kind() != "list_lit" {
            continue;
        }
        let inner = named_children(*child);
        let is_require = inner
            .first()
            .map(|kw| kw.kind() == "kwd_lit" && node_text(*kw, ctx.source) == ":require")
            .unwrap_or(false);
        if !is_require {
            continue;
        }
        for spec in inner.iter().skip(1) {
            if spec.kind() != "vec_lit" {
                continue;
            }
            let items = named_children(*spec);
            let Some(ns_name) = items.first().filter(|n| n.kind() == "sym_lit") else {
                continue;
            };
            let ns_name = sym_text(*ns_name, ctx.source).to_string();
            let mut i = 1;
            while i < items.len() {
                let is_refer =
                    items[i].kind() == "kwd_lit" && node_text(items[i], ctx.source) == ":refer";
                if is_refer {
                    if let Some(refer_vec) = items.get(i + 1).filter(|n| n.kind() == "vec_lit") {
                        for sym in named_children(*refer_vec) {
                            if sym.kind() == "sym_lit" {
                                out.push(Occurrence {
                                    fqn: format!("{}/{}", ns_name, sym_text(sym, ctx.source)),
                                    name_range: node_to_lsp_range(sym_name_node(sym), ctx.source),
                                });
                            }
                        }
                    }
                    i += 2;
                    continue;
                }
                i += 1;
            }
        }
    }
}

fn record_occurrence(
    node: Node,
    ctx: &OccurrenceCtx,
    scope: &[HashSet<String>],
    out: &mut Vec<Occurrence>,
) {
    // The grammar splits qualified symbols: `lib/process` is
    // (sym_lit namespace: (sym_ns) name: (sym_name)).
    let name_node = node.child_by_field_name("name").unwrap_or(node);
    let name = node_text(name_node, ctx.source);
    if name == "&" || name == "_" || name.starts_with('%') {
        return;
    }
    let name_range = node_to_lsp_range(name_node, ctx.source);

    if let Some(ns_node) = node.child_by_field_name("namespace") {
        // Qualified usage: resolve the alias; an unknown alias is treated
        // as a literal namespace name.
        let alias = node_text(ns_node, ctx.source);
        let ns = ctx
            .ns_meta
            .aliases
            .get(alias)
            .cloned()
            .unwrap_or_else(|| alias.to_string());
        out.push(Occurrence {
            fqn: format!("{}/{}", ns, name),
            name_range,
        });
        return;
    }

    if scope.iter().any(|frame| frame.contains(name)) {
        return; // locally bound
    }

    let current_ns = &ctx.ns_meta.name;
    let in_ns = |name: &str| {
        if current_ns.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", current_ns, name)
        }
    };

    let fqn = if let Some(refer_fqn) = ctx.ns_meta.refers.get(name) {
        refer_fqn.clone()
    } else if ctx.def_names.contains(name) {
        in_ns(name)
    } else if core_names().contains(name) {
        format!("clojure.core/{}", name)
    } else {
        in_ns(name)
    };

    out.push(Occurrence { fqn, name_range });
}

/// Records a qualified keyword usage. The range spans the whole keyword token
/// so navigation resolves from a click anywhere on `:ns/name` / `::name`
/// (keyword rename is unsupported in v1, so a name-only range buys nothing).
fn record_keyword_occurrence(node: Node, ctx: &OccurrenceCtx, out: &mut Vec<Occurrence>) {
    if let Some(fqn) = keyword_fqn(node, ctx.ns_meta, ctx.source) {
        out.push(Occurrence {
            fqn,
            name_range: node_to_lsp_range(node, ctx.source),
        });
    }
}

fn str_to_defkind(s: &str) -> Option<DefKind> {
    DefKind::from_def_symbol(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser.set_language(language()).unwrap();
        parser.parse(source, None).unwrap()
    }

    fn find_kwd(node: Node) -> Option<Node> {
        if node.kind() == "kwd_lit" {
            return Some(node);
        }
        for child in named_children(node) {
            if let Some(found) = find_kwd(child) {
                return Some(found);
            }
        }
        None
    }

    /// Resolves the first keyword in `source` against a namespace `ns` with the
    /// given `:as` aliases.
    fn resolve_kwd(source: &str, ns: &str, aliases: &[(&str, &str)]) -> Option<String> {
        let tree = parse(source);
        let kwd = find_kwd(tree.root_node()).expect("no kwd_lit in source");
        let meta = NsMeta {
            name: ns.to_string(),
            file: std::path::PathBuf::new(),
            aliases: aliases
                .iter()
                .map(|(a, f)| (a.to_string(), f.to_string()))
                .collect(),
            refers: HashMap::new(),
            requires: Vec::new(),
            imports: HashMap::new(),
        };
        keyword_fqn(kwd, &meta, source)
    }

    #[test]
    fn keyword_fqn_auto_resolves_bare_to_current_ns() {
        assert_eq!(
            resolve_kwd("::db", "readx.db", &[]),
            Some(":readx.db/db".to_string())
        );
    }

    #[test]
    fn keyword_fqn_auto_resolves_alias() {
        assert_eq!(
            resolve_kwd("::db2/x", "readx.db", &[("db2", "other.db")]),
            Some(":other.db/x".to_string())
        );
    }

    #[test]
    fn keyword_fqn_single_colon_namespace_is_literal() {
        // No alias resolution for `:lib/x`; the namespace is taken verbatim
        // even when an alias of the same name exists.
        assert_eq!(
            resolve_kwd(":lit.ns/x", "readx.db", &[("lit.ns", "should.not.win")]),
            Some(":lit.ns/x".to_string())
        );
    }

    #[test]
    fn keyword_fqn_unqualified_is_none() {
        assert_eq!(resolve_kwd(":plain", "readx.db", &[]), None);
    }

    #[test]
    fn keyword_fqn_auto_without_ns_or_current_ns_is_none() {
        assert_eq!(resolve_kwd("::x", "", &[]), None);
    }
}
