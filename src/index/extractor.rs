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
    match parse_tree(source) {
        Some(tree) => qualified_usages_tree(&tree, source),
        None => vec![],
    }
}

/// [`qualified_usages`] over an already-parsed `tree` of `source`.
pub fn qualified_usages_tree(tree: &tree_sitter::Tree, source: &str) -> Vec<QualifiedUsage> {
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
/// (`#ig/ref :ns/x`), maps, and vectors are all reached by the generic descent;
/// a namespaced map (`#:my.app{:db …}`) qualifies its keys with its prefix.
pub fn extract_edn(source: &str) -> Vec<Occurrence> {
    match parse_tree(source) {
        Some(tree) => extract_edn_tree(&tree, source),
        None => vec![],
    }
}

/// [`extract_edn`] over an already-parsed `tree` of `source`.
pub fn extract_edn_tree(tree: &tree_sitter::Tree, source: &str) -> Vec<Occurrence> {
    let empty = NsMeta {
        name: String::new(),
        file: std::path::PathBuf::new(),
        aliases: HashMap::new(),
        refers: HashMap::new(),
        requires: Vec::new(),
        imports: HashMap::new(),
        refer_all: Vec::new(),
        as_aliases: Vec::new(),
        core_excludes: Vec::new(),
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
    // `#:my.app{:db {…}}` is an ordinary Integrant config written short: the
    // reader qualifies each unqualified key with the map's prefix, so walking
    // the keys blindly would miss `:my.app/db` entirely — and a keyword rename
    // that cannot see a site silently leaves it reading the old key.
    if node.kind() == "ns_map_lit" {
        collect_edn_ns_map(node, source, ns_meta, out);
        return;
    }
    for child in named_children(node) {
        collect_edn_keywords(child, source, ns_meta, out);
    }
}

/// The keys of a namespaced map in an EDN config, under the fqns the reader
/// gives them. Mirrors [`walk_ns_map`] with no scope to track: an EDN file has
/// no `ns` form, so `#::{…}` and `#::alias{…}` resolve to nothing and their keys
/// stay unqualified (recorded under no fqn), while `#:my.app{…}` qualifies its
/// own. A splicing reader conditional makes keys and values indistinguishable,
/// so such a map falls back to the ordinary walk.
fn collect_edn_ns_map(node: Node, source: &str, ns_meta: &NsMeta, out: &mut Vec<Occurrence>) {
    static NO_LINT_AS: OnceLock<HashMap<String, DefKind>> = OnceLock::new();
    let ctx = OccurrenceCtx {
        source,
        ns_meta,
        def_names: HashSet::new(),
        lint_as: NO_LINT_AS.get_or_init(HashMap::new),
    };
    let map_ns = ns_map_prefix(node, &ctx);
    let mut cursor = node.walk();
    let entries: Vec<Node> = node.children_by_field_name("value", &mut cursor).collect();
    if entries.iter().any(|n| n.kind() == "splicing_read_cond_lit") {
        for entry in entries {
            collect_edn_keywords(entry, source, ns_meta, out);
        }
        return;
    }
    for pair in entries.chunks(2) {
        match pair.first() {
            Some(key) if key.kind() == "kwd_lit" => {
                record_ns_map_key(*key, map_ns.as_deref(), &ctx, out)
            }
            Some(key) => collect_edn_keywords(*key, source, ns_meta, out),
            None => {}
        }
        if let Some(value) = pair.get(1) {
            collect_edn_keywords(*value, source, ns_meta, out);
        }
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
    match parse_tree(source) {
        Some(tree) => file_occurrences_tree(&tree, source, path, cfg),
        None => Vec::new(),
    }
}

/// [`file_occurrences_with`] over an already-parsed `tree` of `source`.
pub fn file_occurrences_tree(
    tree: &tree_sitter::Tree,
    source: &str,
    path: &Path,
    cfg: &ExtractConfig,
) -> Vec<Occurrence> {
    if crate::config::is_edn(path) {
        if is_integrant_edn(path, source) {
            extract_edn_tree(tree, source)
        } else {
            Vec::new()
        }
    } else {
        extract_full_tree(tree, source, path, cfg)
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
    let analysis = extract_analysis_with(source, file, cfg)?;
    Ok((analysis.ns_meta, analysis.symbols, analysis.occurrences))
}

/// [`extract_full_with`] over an already-parsed `tree` of `source`.
pub fn extract_full_tree(
    tree: &tree_sitter::Tree,
    source: &str,
    file: &Path,
    cfg: &ExtractConfig,
) -> Result<(NsMeta, Vec<Symbol>, Vec<Occurrence>)> {
    let analysis = extract_analysis_tree(tree, source, file, cfg)?;
    Ok((analysis.ns_meta, analysis.symbols, analysis.occurrences))
}

/// Everything one parse of a Clojure file yields: its namespace metadata, the
/// definitions it introduces, every resolved usage, and the local bindings that
/// were never used (the `unused-binding` lint's input). [`extract_full_with`]
/// is the three-tuple view for callers that don't need the lint.
pub struct Analysis {
    pub ns_meta: NsMeta,
    pub symbols: Vec<Symbol>,
    pub occurrences: Vec<Occurrence>,
    pub unused_bindings: Vec<LocalBinding>,
}

/// Parses `source` once and runs both passes: definitions (with `cfg`'s
/// `:lint-as` macros) and, over the same tree, occurrences plus unused-binding
/// analysis.
pub fn extract_analysis_with(source: &str, file: &Path, cfg: &ExtractConfig) -> Result<Analysis> {
    let tree = parse_tree(source).ok_or_else(|| anyhow!("failed to parse"))?;
    extract_analysis_tree(&tree, source, file, cfg)
}

/// [`extract_analysis_with`] over an already-parsed `tree` of `source`: the
/// entry point for everything that holds the document store's cached tree.
/// Parses nothing.
pub fn extract_analysis_tree(
    tree: &tree_sitter::Tree,
    source: &str,
    file: &Path,
    cfg: &ExtractConfig,
) -> Result<Analysis> {
    let root = tree.root_node();
    let mut ns_meta = NsMeta {
        name: String::new(),
        file: file.to_path_buf(),
        aliases: HashMap::new(),
        refers: HashMap::new(),
        requires: Vec::new(),
        imports: HashMap::new(),
        refer_all: Vec::new(),
        as_aliases: Vec::new(),
        core_excludes: Vec::new(),
    };
    let mut symbols = Vec::new();

    for i in 0..root.named_child_count() {
        let child = root.named_child(i).unwrap();
        match child.kind() {
            "list_lit" => {
                process_top_level_list(child, source, file, &mut ns_meta, &mut symbols, cfg)
            }
            "read_cond_lit" => {
                process_reader_conditional(child, source, file, &mut ns_meta, &mut symbols, cfg);
            }
            _ => {}
        }
    }

    // A `declare`d name that the file goes on to define really is that
    // definition: drop the placeholder so the index and the outline show one
    // entry, pointing at the definition rather than the forward declaration.
    let defined: HashSet<String> = symbols
        .iter()
        .filter(|s| s.kind != DefKind::Declare)
        .map(|s| s.fqn.clone())
        .collect();
    symbols.retain(|s| s.kind != DefKind::Declare || !defined.contains(&s.fqn));

    // Second pass: occurrences, resolved through the completed ns metadata
    let def_names: HashSet<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    let ctx = OccurrenceCtx {
        source,
        ns_meta: &ns_meta,
        def_names,
        lint_as: &cfg.lint_as,
    };
    let mut occurrences = Vec::new();
    let mut scope = Scope::new();
    for i in 0..root.named_child_count() {
        let child = root.named_child(i).unwrap();
        walk_occurrences(child, &ctx, &mut scope, &mut occurrences);
    }
    // Every walk that pushes a frame pops it, so nothing is left holding
    // bindings back from `scope.unused` here.

    Ok(Analysis {
        ns_meta,
        symbols,
        occurrences,
        unused_bindings: scope.unused,
    })
}

fn process_reader_conditional(
    node: Node,
    source: &str,
    file: &Path,
    ns_meta: &mut NsMeta,
    symbols: &mut Vec<Symbol>,
    cfg: &ExtractConfig,
) {
    let children: Vec<Node> = named_children(node);
    // read_cond_lit contains alternating kwd_lit and form pairs
    let mut i = 0;
    while i + 1 < children.len() {
        let form = &children[i + 1];
        if form.kind() == "list_lit" {
            process_top_level_list(*form, source, file, ns_meta, symbols, cfg);
        }
        i += 2;
    }
}

/// Resolves a list-head `sym_lit` to the fully-qualified name clj-kondo would
/// use, for matching against `:lint-as` keys. A qualified head resolves its
/// `:as` alias (an unknown alias is kept literal); a bare head resolves a
/// `:refer`, else falls back to the current namespace. `None` for a nameless
/// head, or a bare name when there is no current namespace to qualify it.
fn resolve_head_fqn(head: Node, ns_meta: &NsMeta, source: &str) -> Option<String> {
    let name = node_text(sym_name_node(head), source);
    if name.is_empty() {
        return None;
    }
    if let Some(ns_node) = head.child_by_field_name("namespace") {
        let alias = node_text(ns_node, source);
        let ns = ns_meta
            .aliases
            .get(alias)
            .map(String::as_str)
            .unwrap_or(alias);
        return Some(format!("{}/{}", ns, name));
    }
    if let Some(fqn) = ns_meta.refers.get(name) {
        return Some(fqn.clone());
    }
    if ns_meta.name.is_empty() {
        None
    } else {
        Some(format!("{}/{}", ns_meta.name, name))
    }
}

/// The `DefKind` a macro-headed form introduces, with the fqn that matched: the
/// head's resolved fqn looked up in the user's `:lint-as` map, then in the
/// built-in table ([`DefKind::from_macro_fqn`]). A bare head that is not
/// `:refer`red is also tried against every `:refer :all` / `:use` namespace, so
/// `deftest` resolves however `clojure.test` was pulled in. `None` for core def
/// forms (handled by `str_to_defkind`) and for ordinary calls.
fn macro_def_kind(
    head: Node,
    ns_meta: &NsMeta,
    source: &str,
    lint_as: &HashMap<String, DefKind>,
) -> Option<(String, DefKind)> {
    let mut candidates: Vec<String> = Vec::new();
    // `resolve_head_fqn` falls back to the current namespace for a bare
    // unreferred head; that candidate is harmless (nothing maps it) and keeps
    // working for `:lint-as` keys written as the current ns.
    if let Some(fqn) = resolve_head_fqn(head, ns_meta, source) {
        candidates.push(fqn);
    }
    let name = node_text(sym_name_node(head), source);
    if !name.is_empty()
        && head.child_by_field_name("namespace").is_none()
        && !ns_meta.refers.contains_key(name)
    {
        for ns in &ns_meta.refer_all {
            candidates.push(format!("{}/{}", ns, name));
        }
    }

    candidates.into_iter().find_map(|fqn| {
        lint_as
            .get(&fqn)
            .cloned()
            .or_else(|| DefKind::from_macro_fqn(&fqn))
            .map(|kind| (fqn, kind))
    })
}

fn process_top_level_list(
    node: Node,
    source: &str,
    file: &Path,
    ns_meta: &mut NsMeta,
    symbols: &mut Vec<Symbol>,
    cfg: &ExtractConfig,
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
        return;
    }

    // `(declare a b c)` introduces one var per name, none of them defined yet.
    if first_text == "declare" {
        extract_declare(node, &children, source, file, &ns_meta.name, symbols);
        return;
    }

    // A built-in def form (`defn`, `def`, …), or a `:lint-as` / well-known macro
    // mapped to one (`defcomponent` → `def`, `clojure.test/deftest` →
    // `deftest`). The mapped kind reuses the normal def extraction, so the
    // macro's defined name becomes a real symbol.
    let kind = str_to_defkind(first_text)
        .or_else(|| macro_def_kind(first, ns_meta, source, &cfg.lint_as).map(|(_, kind)| kind));
    if let Some(kind) = kind {
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
                // `(:use ns)` refers every public var of `ns`, so it is both a
                // require and a refer-all. `:only` is not narrowed - the whole
                // namespace is offered, which over-offers rather than misses.
                // `(:refer-clojure :exclude [...] :rename {from to})` reshapes
                // what bare names mean: excluded names are no longer core's,
                // and a renamed one is referred under its new name. `:only` is
                // ignored — narrowing core would only cost resolutions.
                ":refer-clojure" => parse_refer_clojure(&inner[1..], source, ns_meta),
                ":use" => {
                    for use_spec in &inner[1..] {
                        process_require_spec(*use_spec, source, ns_meta);
                        let mut used = Vec::new();
                        collect_use_namespaces(*use_spec, source, &mut used);
                        for ns in used {
                            record_refer_all(ns_meta, &ns);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Records one `:require` spec into `ns_meta`. Handles plain libspecs
/// (`[a.b :as x]`), bare namespaces (`clojure.set`), legacy prefix lists
/// (`(clojure [set :as s] string)`), and reader conditionals
/// (`#?(:clj [a :as x])` / `#?@(:clj [[a :as x]])`) — every branch's aliases
/// are recorded so conditional requires aren't reported as unresolved.
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
        // Legacy prefix list `(clojure [set :as s] string)`: each entry is a
        // libspec whose namespace is the prefix joined by a dot. The prefix
        // itself binds nothing, so `set/union` stays unresolved — only
        // `clojure.set/union` and the alias `s` resolve, as in Clojure.
        "list_lit" => {
            let items = named_children(spec);
            let Some(prefix_node) = items.first() else {
                return;
            };
            let is_prefix_list = prefix_node.kind() == "sym_lit"
                && items.len() > 1
                && items[1..]
                    .iter()
                    .all(|n| matches!(n.kind(), "sym_lit" | "vec_lit"));
            if !is_prefix_list {
                return;
            }
            let prefix = sym_text(*prefix_node, source).to_string();
            for item in &items[1..] {
                match item.kind() {
                    "sym_lit" => {
                        ns_meta
                            .requires
                            .push(format!("{}.{}", prefix, sym_text(*item, source)))
                    }
                    "vec_lit" => {
                        let sub = named_children(*item);
                        if sub.first().map(|n| n.kind()) == Some("sym_lit") {
                            let ns_name = format!("{}.{}", prefix, sym_text(sub[0], source));
                            parse_libspec_items(&sub, ns_name, source, ns_meta);
                        }
                    }
                    _ => {}
                }
            }
        }
        // Reader conditional: descend into each branch's form (skip platform
        // keywords). Reader conditionals nested inside a prefix list are out
        // of scope and record nothing.
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

/// Every namespace a `(:use …)` spec names, pushed onto `out`: a bare symbol
/// (`clojure.set`), the head of a libspec vector (`[clojure.set :only [union]]`),
/// a vector of specs, or each branch of a reader conditional — the same shapes
/// [`process_require_spec`] accepts, so a conditional `:use` refers in full on
/// every platform. `:only` is not narrowed: the whole namespace is referred,
/// which over-offers rather than misses. Legacy prefix lists expand exactly as
/// they do in `:require`, so `(:use (clojure set))` refers `clojure.set`.
fn collect_use_namespaces(spec: Node, source: &str, out: &mut Vec<String>) {
    match spec.kind() {
        "sym_lit" => out.push(sym_text(spec, source).to_string()),
        "vec_lit" => {
            let items = named_children(spec);
            match items.first().map(|n| n.kind()) {
                Some("sym_lit") => out.push(sym_text(items[0], source).to_string()),
                Some("vec_lit") => {
                    for item in items {
                        collect_use_namespaces(item, source, out);
                    }
                }
                _ => {}
            }
        }
        "list_lit" => {
            let items = named_children(spec);
            let Some(prefix_node) = items.first() else {
                return;
            };
            if prefix_node.kind() != "sym_lit" || items.len() < 2 {
                return;
            }
            let prefix = sym_text(*prefix_node, source);
            for item in &items[1..] {
                let name = match item.kind() {
                    "sym_lit" => Some(sym_text(*item, source)),
                    "vec_lit" => named_children(*item)
                        .first()
                        .filter(|n| n.kind() == "sym_lit")
                        .map(|n| sym_text(*n, source)),
                    _ => None,
                };
                if let Some(name) = name {
                    out.push(format!("{}.{}", prefix, name));
                }
            }
        }
        "read_cond_lit" | "splicing_read_cond_lit" => {
            for child in named_children(spec) {
                if child.kind() != "kwd_lit" {
                    collect_use_namespaces(child, source, out);
                }
            }
        }
        _ => {}
    }
}

/// Records `ns` as referred in full. De-duplicated: a reader conditional can
/// name the same namespace in several branches, and a repeat would offer its
/// vars twice in completion.
fn record_refer_all(ns_meta: &mut NsMeta, ns: &str) {
    if !ns_meta.refer_all.iter().any(|n| n == ns) {
        ns_meta.refer_all.push(ns.to_string());
    }
}

/// Reads a `(:refer-clojure …)` clause: `:exclude [names]` into `core_excludes`
/// and `:rename {from to}` into `refers`, so the new name resolves to the core
/// var it renames.
fn parse_refer_clojure(items: &[Node], source: &str, ns_meta: &mut NsMeta) {
    let mut i = 0;
    while i + 1 < items.len() {
        if items[i].kind() != "kwd_lit" {
            i += 1;
            continue;
        }
        match node_text(items[i], source) {
            ":exclude" if items[i + 1].kind() == "vec_lit" => {
                for name in named_children(items[i + 1]) {
                    if name.kind() == "sym_lit" {
                        let name = sym_text(name, source).to_string();
                        if !ns_meta.core_excludes.contains(&name) {
                            ns_meta.core_excludes.push(name);
                        }
                    }
                }
            }
            ":rename" if items[i + 1].kind() == "map_lit" => {
                for (from, to) in rename_pairs(items[i + 1], source) {
                    ns_meta.refers.insert(to, format!("clojure.core/{}", from));
                    // Renaming a core name unmaps the original: after
                    // `:rename {map cmap}`, bare `map` is not core's `map`.
                    if !ns_meta.core_excludes.contains(&from) {
                        ns_meta.core_excludes.push(from);
                    }
                }
            }
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
}

/// The `{from to}` pairs of a `:rename` map, as source text. Non-symbol entries
/// are skipped, so a malformed map records nothing rather than a bogus binding.
fn rename_pairs(map_node: Node, source: &str) -> Vec<(String, String)> {
    let items = named_children(map_node);
    items
        .chunks_exact(2)
        .filter(|pair| pair[0].kind() == "sym_lit" && pair[1].kind() == "sym_lit")
        .map(|pair| {
            (
                sym_text(pair[0], source).to_string(),
                sym_text(pair[1], source).to_string(),
            )
        })
        .collect()
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
    let Some(first) = items.first() else {
        return;
    };
    if first.kind() != "sym_lit" {
        return;
    }
    let ns_name = sym_text(*first, source).to_string();
    parse_libspec_items(&items, ns_name, source, ns_meta);
}

/// The options of a libspec whose namespace is already resolved. Split out so a
/// prefix-list entry (`[set :as s]` under `(clojure …)`) is read with its joined
/// name (`clojure.set`) without rewriting source text. `items` is the whole
/// libspec, `items[0]` being the namespace symbol.
fn parse_libspec_items(items: &[Node], ns_name: String, source: &str, ns_meta: &mut NsMeta) {
    // `:as-alias` binds an alias without loading the namespace, so the require
    // is only recorded once the options rule that out.
    let mut as_alias_only = false;
    // Applied after the whole spec is read, so a `:rename` written before its
    // `:refer` still finds the entry it renames.
    let mut renames: Vec<(String, String)> = Vec::new();

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
                // `[a.b :as-alias x]` binds `x` for keyword and symbol
                // resolution without loading `a.b`.
                ":as-alias" if i + 1 < items.len() && items[i + 1].kind() == "sym_lit" => {
                    let alias = node_text(items[i + 1], source).to_string();
                    ns_meta.aliases.insert(alias, ns_name.clone());
                    if !ns_meta.as_aliases.contains(&ns_name) {
                        ns_meta.as_aliases.push(ns_name.clone());
                    }
                    as_alias_only = true;
                    i += 2;
                    continue;
                }
                // `:refer :all` names no individual vars, so it lands in
                // `refer_all` instead of `refers`.
                ":refer"
                    if i + 1 < items.len()
                        && items[i + 1].kind() == "kwd_lit"
                        && node_text(items[i + 1], source) == ":all" =>
                {
                    record_refer_all(ns_meta, &ns_name);
                    i += 2;
                    continue;
                }
                // `[a.b :refer [x] :rename {x y}]` binds `y`, not `x`.
                ":rename" if i + 1 < items.len() && items[i + 1].kind() == "map_lit" => {
                    renames.extend(rename_pairs(items[i + 1], source));
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

    for (from, to) in renames {
        if let Some(fqn) = ns_meta.refers.remove(&from) {
            ns_meta.refers.insert(to, fqn);
        }
    }

    if !as_alias_only {
        ns_meta.requires.push(ns_name);
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

    let private = kind == DefKind::DefnPrivate
        || has_private_meta(name_node, source)
        || has_private_attr_map(kind.clone(), &children[rest_start..], source);

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
        private,
    });

    // A protocol's method signatures are namespace-level vars too; index each
    // so go-to-definition / hover / completion / references reach them.
    if kind == DefKind::Defprotocol {
        extract_protocol_methods(&children[2..], source, file, ns_name, symbols);
    }
}

/// Records one `Symbol` per name in `(declare a ^:private b)`. A declaration
/// carries no params and no docstring; its `range` is the whole form, so the
/// outline and hover point at the `declare` that introduced it.
fn extract_declare(
    form_node: Node,
    children: &[Node],
    source: &str,
    file: &Path,
    ns_name: &str,
    symbols: &mut Vec<Symbol>,
) {
    for name_node in &children[1..] {
        if name_node.kind() != "sym_lit" {
            continue;
        }
        let name = sym_text(*name_node, source).to_string();
        let fqn = if ns_name.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", ns_name, name)
        };
        symbols.push(Symbol {
            name,
            fqn,
            ns: ns_name.to_string(),
            kind: DefKind::Declare,
            params: Vec::new(),
            doc: None,
            file: file.to_path_buf(),
            source: super::SymbolSource::Project,
            range: node_to_lsp_range(form_node, source),
            name_range: node_to_lsp_range(sym_name_node(*name_node), source),
            private: has_private_meta(*name_node, source),
        });
    }
}

/// Whether a def'd name carries `^:private` or `^{:private true}`. Metadata
/// attaches to the *name* symbol (`(def ^:private x 1)`), so this reads the
/// `sym_lit`'s own `meta_lit`/`old_meta_lit` children. `^{:private false}` is
/// explicitly public.
fn has_private_meta(name_node: Node, source: &str) -> bool {
    named_children(name_node)
        .into_iter()
        .filter(|n| matches!(n.kind(), "meta_lit" | "old_meta_lit"))
        .filter_map(|meta| named_children(meta).into_iter().next())
        .any(|value| match value.kind() {
            "kwd_lit" => node_text(value, source) == ":private",
            "map_lit" => map_declares_private(value, source),
            _ => false,
        })
}

/// Whether a map literal has `:private true` among its pairs.
fn map_declares_private(map: Node, source: &str) -> bool {
    named_children(map).chunks(2).any(|pair| {
        matches!(pair, [k, v]
            if node_text(*k, source) == ":private" && node_text(*v, source) == "true")
    })
}

/// Whether a `defn`-family form declares `:private true` in its *attr-map*:
/// `(defn f {:private true} [] …)`, or the trailing map of a multi-arity
/// `(defn f ([] …) {:private true})`. `rest` is the form's children after the
/// name and any docstring.
///
/// Only the two positions the reader treats as an attr-map count, so
/// `(defn f [] {:private true})` — a function *returning* that map — stays
/// public. `defmulti` has its own leading-attr-map shape. Plain `def`/`defonce`
/// are excluded entirely: their map is the value.
fn has_private_attr_map(kind: DefKind, rest: &[Node], source: &str) -> bool {
    // `(defmulti name attr-map? dispatch-fn & options)`: the attr-map is the
    // leading map, and something (the dispatch fn) must follow it — a lone map
    // is the dispatch fn itself, since maps are functions.
    if kind == DefKind::Defmulti {
        return rest.len() > 1
            && rest[0].kind() == "map_lit"
            && map_declares_private(rest[0], source);
    }
    if !matches!(
        kind,
        DefKind::Defn | DefKind::DefnPrivate | DefKind::Defmacro
    ) {
        return false;
    }
    // Leading: the map sits directly before the params vector / first arity.
    let leading = rest
        .first()
        .filter(|n| n.kind() == "map_lit")
        .is_some_and(|m| {
            let follows_params = rest
                .get(1)
                .map(|n| n.kind() == "vec_lit" || (n.kind() == "list_lit" && arity_body(*n)))
                .unwrap_or(false);
            follows_params && map_declares_private(*m, source)
        });
    // Trailing: after at least one `([params] body…)` arity.
    let trailing = rest.len() > 1
        && rest
            .last()
            .filter(|n| n.kind() == "map_lit")
            .is_some_and(|m| map_declares_private(*m, source))
        && rest[..rest.len() - 1]
            .iter()
            .any(|n| n.kind() == "list_lit" && arity_body(*n));
    leading || trailing
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
        private: false,
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
            // A protocol method signature is public by definition.
            private: false,
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

pub(crate) fn node_to_lsp_range(node: Node, source: &str) -> Range {
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

/// The tree-sitter point (row, *byte* column) for an LSP position, the inverse
/// of [`point_to_position`]. Walks the line's chars counting UTF-16 units;
/// a column past the end of the line clamps to the line end. `None` when the
/// line does not exist.
pub fn position_to_point(source: &str, pos: Position) -> Option<tree_sitter::Point> {
    let line = source.split('\n').nth(pos.line as usize)?;
    let mut utf16 = 0usize;
    let mut column = line.len();
    for (byte, ch) in line.char_indices() {
        if utf16 >= pos.character as usize {
            column = byte;
            break;
        }
        utf16 += ch.len_utf16();
    }
    Some(tree_sitter::Point {
        row: pos.line as usize,
        column,
    })
}

/// Named nodes containing `pos`, innermost first, stopping before the root
/// `source` node.
///
/// Token-internal nodes are normalized away: the grammar gives `sym_lit` and
/// `kwd_lit` named children for their namespace and name parts
/// (`sym_ns`/`sym_name`, `kwd_ns`/`kwd_name`), and the descendant lookup can
/// land on one of them, so the path starts at the enclosing literal instead.
/// Callers that want the name part ask for it explicitly
/// (`child_by_field_name("name")`).
pub fn node_path_at<'a>(root: Node<'a>, source: &str, pos: Position) -> Vec<Node<'a>> {
    let Some(pt) = position_to_point(source, pos) else {
        return Vec::new();
    };
    let Some(leaf) = root.named_descendant_for_point_range(pt, pt) else {
        return Vec::new();
    };

    let mut path = Vec::new();
    let mut node = Some(leaf);
    while let Some(current) = node {
        if current.id() == root.id() {
            break;
        }
        // Skip the parts of a token: the enclosing literal is the first step.
        if !matches!(
            current.kind(),
            "sym_ns" | "sym_name" | "kwd_ns" | "kwd_name"
        ) {
            path.push(current);
        }
        node = current.parent();
    }
    path
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

/// The fqn a keyword *usage* is recorded under: [`keyword_fqn`] for qualified
/// and auto-resolved keywords, plus the unqualified case those reject —
/// `:name` is recorded as `:name`, since completion ranks unqualified keywords
/// by how often the project uses them and references list every usage.
///
/// Only usages take this path. Definition sites (`ig/init-key` dispatch) and
/// EDN configs keep to `keyword_fqn`: an unqualified keyword is too ambiguous
/// to define a component, and a `::name` in a file with no `ns` form resolves
/// to no namespace at all, so it is recorded under no fqn either.
fn keyword_occurrence_fqn(node: Node, ns_meta: &NsMeta, source: &str) -> Option<String> {
    if let Some(fqn) = keyword_fqn(node, ns_meta, source) {
        return Some(fqn);
    }
    if node.child_by_field_name("namespace").is_some() {
        return None;
    }
    let auto_resolved = node
        .child_by_field_name("marker")
        .map(|m| node_text(m, source) == "::")
        .unwrap_or(false);
    if auto_resolved {
        return None;
    }
    Some(format!(
        ":{}",
        node_text(node.child_by_field_name("name")?, source)
    ))
}

// --- occurrence collection -------------------------------------------------

/// One local binding in the occurrence walker's scope stack: enough to
/// suppress var resolution (`name`) *and* to report it unused (`name_range`,
/// `used`, `lintable`).
struct LocalSlot {
    name: String,
    name_range: Range,
    used: bool,
    /// Whether an unused slot is worth reporting. Names the user cannot drop —
    /// `fn`/`letfn` self-names, record/type fields, protocol-method params —
    /// bind but never report.
    lintable: bool,
}

/// The occurrence walker's lexical scope: a stack of frames, plus the bindings
/// that turned out unused. A frame's leftovers are harvested when it pops, so
/// the unused-binding lint reuses the walker's scope rules rather than
/// re-deriving them.
struct Scope {
    frames: Vec<Vec<LocalSlot>>,
    unused: Vec<LocalBinding>,
}

impl Scope {
    fn new() -> Self {
        Scope {
            frames: Vec::new(),
            unused: Vec::new(),
        }
    }

    fn push(&mut self) {
        self.frames.push(Vec::new());
    }

    /// Pops the innermost frame, reporting every lintable slot that was never
    /// used. A leading `_` is the conventional opt-out and is never reported.
    fn pop(&mut self) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        for slot in frame {
            if slot.lintable && !slot.used && !slot.name.starts_with('_') {
                self.unused.push(LocalBinding {
                    name: slot.name,
                    name_range: slot.name_range,
                });
            }
        }
    }

    /// Adds `bindings` to the innermost frame. Binding sites are collected
    /// before the frame they belong to is pushed (`:or` defaults must be walked
    /// in the enclosing scope), so this is a separate step from `push`.
    fn bind_all(&mut self, bindings: Vec<LocalBinding>, lintable: bool) {
        let Some(frame) = self.frames.last_mut() else {
            return;
        };
        frame.extend(bindings.into_iter().map(|b| LocalSlot {
            name: b.name,
            name_range: b.name_range,
            used: false,
            lintable,
        }));
    }

    /// Marks the innermost binding of `name` used, returning whether one
    /// existed (i.e. whether the symbol is a local rather than a var). Slots are
    /// searched newest-first so `(let [x 1 x (inc x)] x)` marks both.
    fn mark_used(&mut self, name: &str) -> bool {
        for frame in self.frames.iter_mut().rev() {
            if let Some(slot) = frame.iter_mut().rev().find(|s| s.name == name) {
                slot.used = true;
                return true;
            }
        }
        false
    }
}

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

fn walk_occurrences(node: Node, ctx: &OccurrenceCtx, scope: &mut Scope, out: &mut Vec<Occurrence>) {
    match node.kind() {
        "sym_lit" => record_occurrence(node, ctx, scope, out),
        // Every keyword literal is a usage: qualified ones (`:lib/x`, `::x`,
        // `::alias/x`) under their resolved namespace, unqualified ones under
        // `:name`. This powers keyword references and completion, and feeds
        // Integrant component navigation.
        "kwd_lit" => record_keyword_occurrence(node, ctx, out),
        // `#:user{:id 1}` — the reader qualifies the keys, so the keyword the
        // user wrote is not the keyword the program sees.
        "ns_map_lit" => walk_ns_map(node, ctx, scope, out),
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

fn walk_list(node: Node, ctx: &OccurrenceCtx, scope: &mut Scope, out: &mut Vec<Occurrence>) {
    let children = named_children(node);
    let Some(head) = children.first() else { return };

    // A `:lint-as` or built-in defining-macro head (`defcomponent` → `def`,
    // `deftest` → `deftest`) introduces a definition. Record the head itself as
    // a usage of the fqn it matched — not via `record_occurrence`, which would
    // resolve a refer-all bare head to the current namespace — then walk the
    // form as the def-family kind it maps to: its name binds as a def, its body
    // args are usages.
    if head.kind() == "sym_lit" {
        if let Some((fqn, kind)) = macro_def_kind(*head, ctx.ns_meta, ctx.source, ctx.lint_as) {
            out.push(Occurrence {
                fqn,
                name_range: node_to_lsp_range(sym_name_node(*head), ctx.source),
            });
            walk_def_form(kind, &children, ctx, scope, out);
            return;
        }
    }

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
        // (catch Class name body…) / (as-> expr name body…): the second child
        // is an ordinary usage, the third binds a local for the body.
        Some("catch") | Some("as->") => {
            record_occurrence(*head, ctx, scope, out);
            walk_binding_tail(&children, ctx, scope, out);
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
    scope: &mut Scope,
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
        // Record/type fields are fixed by the type's shape, so they bind but
        // are never reported unused.
        let mut fields_bound = Vec::new();
        if let Some(fields) = children.get(2).filter(|n| n.kind() == "vec_lit") {
            collect_binding_names(*fields, ctx, scope, out, &mut fields_bound);
        }
        scope.push();
        scope.bind_all(fields_bound, false);
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
                let mut bound = Vec::new();
                collect_binding_names(*child, ctx, scope, out, &mut bound);
                scope.push();
                scope.bind_all(bound, true);
                frame_pushed = true;
            }
            "list_lit" if arity_body(*child) => {
                // Multi-arity: ([params] body…) — bind per arity
                let inner = named_children(*child);
                let mut bound = Vec::new();
                if let Some(params) = inner.first() {
                    collect_binding_names(*params, ctx, scope, out, &mut bound);
                }
                scope.push();
                scope.bind_all(bound, true);
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
    scope: &mut Scope,
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
    scope: &mut Scope,
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
        // Single arity: (name [params] body…). The signature fixes the arity,
        // so an unused param is not the user's to remove — bind, never report.
        let mut bound = Vec::new();
        collect_binding_names(rest[0], ctx, scope, out, &mut bound);
        scope.push();
        scope.bind_all(bound, false);
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
                let mut bound = Vec::new();
                if let Some(params) = parts.first() {
                    collect_binding_names(*params, ctx, scope, out, &mut bound);
                }
                scope.push();
                scope.bind_all(bound, false);
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
    scope: &mut Scope,
    out: &mut Vec<Occurrence>,
) {
    scope.push();
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
    scope: &mut Scope,
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
        let mut bound = Vec::new();
        collect_binding_names(*lhs, ctx, scope, out, &mut bound);
        scope.bind_all(bound, true);
    }
}

/// `(catch Class name body…)` / `(as-> expr name body…)`: `children[1]` is an
/// expression (the class or the seed value), `children[2]` binds a local
/// visible only in `children[3..]`.
fn walk_binding_tail(
    children: &[Node],
    ctx: &OccurrenceCtx,
    scope: &mut Scope,
    out: &mut Vec<Occurrence>,
) {
    if let Some(expr) = children.get(1) {
        walk_occurrences(*expr, ctx, scope, out);
    }
    let mut bound = Vec::new();
    if let Some(name) = children.get(2).filter(|n| n.kind() == "sym_lit") {
        collect_binding_names(*name, ctx, scope, out, &mut bound);
    }
    scope.push();
    scope.bind_all(bound, true);
    for body in children.iter().skip(3) {
        walk_occurrences(*body, ctx, scope, out);
    }
    scope.pop();
}

/// `(fn name? [params] body…)` — optional self-name and params bind.
fn walk_fn_form(
    children: &[Node],
    ctx: &OccurrenceCtx,
    scope: &mut Scope,
    out: &mut Vec<Occurrence>,
) {
    // The self-name exists for recursion; an unused one is idiomatic, so it
    // binds without being reported.
    let mut self_name = Vec::new();
    let mut rest_start = 1;
    if let Some(name) = children.get(1).filter(|n| n.kind() == "sym_lit") {
        collect_binding_names(*name, ctx, scope, out, &mut self_name);
        rest_start = 2;
    }
    scope.push();
    scope.bind_all(self_name, false);
    walk_fn_tail(&children[rest_start..], ctx, scope, out);
    scope.pop();
}

/// Params + bodies of a fn-like form (after the optional name): a leading
/// vector binds params; `([params] body…)` lists are per-arity scopes.
/// Assumes the caller pushed a scope frame.
fn walk_fn_tail(parts: &[Node], ctx: &OccurrenceCtx, scope: &mut Scope, out: &mut Vec<Occurrence>) {
    let mut params_bound = false;
    for child in parts {
        match child.kind() {
            "vec_lit" if !params_bound => {
                let mut bound = Vec::new();
                collect_binding_names(*child, ctx, scope, out, &mut bound);
                scope.bind_all(bound, true);
                params_bound = true;
            }
            "list_lit" if arity_body(*child) => {
                let inner = named_children(*child);
                let mut bound = Vec::new();
                if let Some(params) = inner.first() {
                    collect_binding_names(*params, ctx, scope, out, &mut bound);
                }
                scope.push();
                scope.bind_all(bound, true);
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
    scope: &mut Scope,
    out: &mut Vec<Occurrence>,
) {
    let fn_specs: Vec<Node> = children
        .get(1)
        .filter(|n| n.kind() == "vec_lit")
        .map(|n| named_children(*n))
        .unwrap_or_default();

    // The fn names are mutually recursive: one used only by a sibling is not
    // dead, and the walk order can't tell. Bind them without reporting.
    let mut fn_names = Vec::new();
    for spec in &fn_specs {
        if spec.kind() == "list_lit" {
            if let Some(name) = named_children(*spec)
                .first()
                .filter(|n| n.kind() == "sym_lit")
            {
                collect_binding_names(*name, ctx, scope, out, &mut fn_names);
            }
        }
    }
    scope.push();
    scope.bind_all(fn_names, false);

    for spec in &fn_specs {
        if spec.kind() != "list_lit" {
            continue;
        }
        let inner = named_children(*spec);
        scope.push();
        walk_fn_tail(&inner[1..], ctx, scope, out);
        scope.pop();
    }
    for body in children.iter().skip(2) {
        walk_occurrences(*body, ctx, scope, out);
    }
    scope.pop();
}

/// Collects every symbol inside a binding pattern (plain names, vector and
/// map destructuring) except `&` and `_`, each with its name range. Map
/// destructuring `:or` defaults are *expressions*, recorded as occurrences
/// rather than bindings.
fn collect_binding_names(
    pattern: Node,
    ctx: &OccurrenceCtx,
    scope: &mut Scope,
    out: &mut Vec<Occurrence>,
    names: &mut Vec<LocalBinding>,
) {
    match pattern.kind() {
        "sym_lit" => {
            let nn = sym_name_node(pattern);
            let name = node_text(nn, ctx.source);
            if name != "&" && name != "_" {
                names.push(LocalBinding {
                    name: name.to_string(),
                    name_range: node_to_lsp_range(nn, ctx.source),
                });
            }
        }
        "map_lit" => {
            let items = named_children(pattern);
            for pair in items.chunks(2) {
                let [k, v] = pair else { continue };
                if k.kind() == "kwd_lit" {
                    if node_text(*k, ctx.source) == ":or" && v.kind() == "map_lit" {
                        // {:or {name default-expr}}: the defaults are usages.
                        // The key is *not* a binding site — the real binding is
                        // the `:keys`/`:as`/map-key entry elsewhere in the same
                        // pattern, and a second slot here would shadow it and
                        // report it unused. (It still resolves to that binding
                        // through the position-directed scope walk, so
                        // references and rename cover it.)
                        for default in named_children(*v).chunks(2) {
                            let [_dk, dv] = default else { continue };
                            walk_occurrences(*dv, ctx, scope, out);
                        }
                    } else {
                        // :keys/:strs/:syms vectors, :as name, …
                        record_destructuring_keys(*k, *v, ctx, out);
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

/// Records the key each entry of a namespaced `:keys` destructuring vector
/// reads. `{::keys [a]}`, `{:my.ns/keys [a]}`, `{::alias/keys [a]}` and the
/// qualified entry of a plain `{:keys [my.ns/a]}` all read `:my.ns/a`, so the
/// entry symbol is a usage of that keyword as well as a binding site: find
/// references lists it, and a keyword rename sees it instead of silently
/// leaving it reading the old key.
///
/// Only `:keys` reads keywords — `:syms` reads quoted symbols and `:strs`
/// strings — and an unqualified entry of a plain `{:keys [a]}` reads `:a`,
/// which no rename can target. None of those contribute an occurrence.
fn record_destructuring_keys(
    directive: Node,
    entries: Node,
    ctx: &OccurrenceCtx,
    out: &mut Vec<Occurrence>,
) {
    let reads_keywords = directive
        .child_by_field_name("name")
        .map(|n| node_text(n, ctx.source) == "keys")
        .unwrap_or(false);
    if !reads_keywords || entries.kind() != "vec_lit" {
        return;
    }
    let directive_ns = destructuring_key_ns(directive, ctx);
    for entry in named_children(entries) {
        if entry.kind() != "sym_lit" {
            continue;
        }
        // `clojure.core/destructure` builds the key as
        // `(keyword (or directive-ns (namespace entry)) (name entry))`, so a
        // qualified directive wins over a qualified entry: `{:foo/keys [bar/a]}`
        // reads `:foo/a`. An entry's own namespace is taken verbatim — only
        // `::` auto-resolves, and only on the directive.
        let ns = match (&directive_ns, entry.child_by_field_name("namespace")) {
            (Some(ns), _) => ns.clone(),
            (None, Some(node)) => node_text(node, ctx.source).to_string(),
            (None, None) => continue,
        };
        out.push(Occurrence {
            fqn: format!(":{}/{}", ns, node_text(sym_name_node(entry), ctx.source)),
            name_range: node_to_lsp_range(entry, ctx.source),
        });
    }
}

/// The namespace a `:keys` directive qualifies its entries with: the current
/// namespace for `::keys`, the alias-resolved one for `::alias/keys`, the
/// literal prefix for `:my.ns/keys`. `None` for a plain `:keys`, whose entries
/// carry their own namespace or none at all.
fn destructuring_key_ns(directive: Node, ctx: &OccurrenceCtx) -> Option<String> {
    let auto_resolved = directive
        .child_by_field_name("marker")
        .map(|m| node_text(m, ctx.source) == "::")
        .unwrap_or(false);
    match directive.child_by_field_name("namespace") {
        Some(ns_node) => {
            let ns = node_text(ns_node, ctx.source);
            Some(if auto_resolved {
                ctx.ns_meta
                    .aliases
                    .get(ns)
                    .cloned()
                    .unwrap_or_else(|| ns.to_string())
            } else {
                ns.to_string()
            })
        }
        None if auto_resolved => (!ctx.ns_meta.name.is_empty()).then(|| ctx.ns_meta.name.clone()),
        None => None,
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
    scope: &mut Scope,
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

    if scope.mark_used(name) {
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
    } else if core_names().contains(name) && !ctx.ns_meta.core_excludes.iter().any(|e| e == name) {
        format!("clojure.core/{}", name)
    } else {
        in_ns(name)
    };

    out.push(Occurrence { fqn, name_range });
}

/// Records a keyword usage. The range spans the whole keyword token so
/// navigation resolves from a click anywhere on `:ns/name` / `::name`
/// (keyword rename is unsupported in v1, so a name-only range buys nothing).
fn record_keyword_occurrence(node: Node, ctx: &OccurrenceCtx, out: &mut Vec<Occurrence>) {
    if let Some(fqn) = keyword_occurrence_fqn(node, ctx.ns_meta, ctx.source) {
        out.push(Occurrence {
            fqn,
            name_range: node_to_lsp_range(node, ctx.source),
        });
    }
}

/// Walks a namespaced map literal (`#:user{…}`, `#::{…}`, `#::alias{…}`). The
/// reader qualifies every *unqualified* key with the map's prefix, so
/// `#:user{:id 1}` reads as `{:user/id 1}` and recording a bare `:id` would
/// answer find-references for an unrelated `:id`. Values are ordinary
/// expressions, and the prefix itself is a reader marker, not a keyword usage.
///
/// A splicing reader conditional (`#:user{#?@(:clj [:a 1]) :b 2}`) contributes
/// an unknown number of entries, so nothing after it can be told apart as key
/// or value. Such a map falls back to the ordinary walk: its unqualified
/// keywords stay unqualified, which under-reports the keys but never invents a
/// namespace for a value.
fn walk_ns_map(node: Node, ctx: &OccurrenceCtx, scope: &mut Scope, out: &mut Vec<Occurrence>) {
    let map_ns = ns_map_prefix(node, ctx);
    let mut cursor = node.walk();
    let entries: Vec<Node> = node.children_by_field_name("value", &mut cursor).collect();
    if entries.iter().any(|n| n.kind() == "splicing_read_cond_lit") {
        for entry in entries {
            walk_occurrences(entry, ctx, scope, out);
        }
        return;
    }
    for pair in entries.chunks(2) {
        match pair.first() {
            Some(key) if key.kind() == "kwd_lit" => {
                record_ns_map_key(*key, map_ns.as_deref(), ctx, out)
            }
            Some(key) => walk_occurrences(*key, ctx, scope, out),
            None => {}
        }
        if let Some(value) = pair.get(1) {
            walk_occurrences(*value, ctx, scope, out);
        }
    }
}

/// The namespace a namespaced map literal qualifies its keys with: the current
/// namespace for `#::{…}`, an alias-resolved one for `#::alias{…}`, the literal
/// prefix for `#:user{…}`. `None` when there is nothing to resolve it to (an
/// `#::{…}` in a file with no `ns` form).
fn ns_map_prefix(node: Node, ctx: &OccurrenceCtx) -> Option<String> {
    let prefix = node.child_by_field_name("prefix")?;
    match prefix.kind() {
        "auto_res_mark" => (!ctx.ns_meta.name.is_empty()).then(|| ctx.ns_meta.name.clone()),
        "kwd_lit" => {
            let ns = node_text(prefix.child_by_field_name("name")?, ctx.source);
            let auto_resolved = prefix
                .child_by_field_name("marker")
                .map(|m| node_text(m, ctx.source) == "::")
                .unwrap_or(false);
            if auto_resolved {
                Some(
                    ctx.ns_meta
                        .aliases
                        .get(ns)
                        .cloned()
                        .unwrap_or_else(|| ns.to_string()),
                )
            } else {
                Some(ns.to_string())
            }
        }
        _ => None,
    }
}

/// Records one key of a namespaced map under the fqn the reader gives it. A key
/// that carries its own namespace (`:other/x`, `::x`) keeps it; `:_/x` is the
/// reader's escape from the prefix and reads as plain `:x`; anything else takes
/// the map's namespace.
fn record_ns_map_key(
    key: Node,
    map_ns: Option<&str>,
    ctx: &OccurrenceCtx,
    out: &mut Vec<Occurrence>,
) {
    let auto_resolved = key
        .child_by_field_name("marker")
        .map(|m| node_text(m, ctx.source) == "::")
        .unwrap_or(false);
    if let Some(ns_node) = key.child_by_field_name("namespace") {
        if !auto_resolved && node_text(ns_node, ctx.source) == "_" {
            if let Some(name) = key.child_by_field_name("name") {
                out.push(Occurrence {
                    fqn: format!(":{}", node_text(name, ctx.source)),
                    name_range: node_to_lsp_range(key, ctx.source),
                });
            }
            return;
        }
        record_keyword_occurrence(key, ctx, out);
        return;
    }
    match map_ns {
        Some(ns) if !auto_resolved => {
            if let Some(name) = key.child_by_field_name("name") {
                out.push(Occurrence {
                    fqn: format!(":{}/{}", ns, node_text(name, ctx.source)),
                    name_range: node_to_lsp_range(key, ctx.source),
                });
            }
        }
        _ => record_keyword_occurrence(key, ctx, out),
    }
}

fn str_to_defkind(s: &str) -> Option<DefKind> {
    DefKind::from_def_symbol(s)
}

// --- local binding resolution (goto-def / completion) ----------------------

/// A local binding (`let`/`fn`/`defn` params, `loop`, `for`/`doseq`,
/// destructuring, `letfn`, …) with the source range of its binding-site symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalBinding {
    pub name: String,
    pub name_range: Range,
}

/// The local bindings visible at `pos`, in outermost→innermost order so the
/// last match shadows earlier ones. Powers go-to-definition and completion for
/// locally-bound names (which are never recorded as occurrences). A cursor on a
/// binding site itself yields that binding (a harmless self-jump).
///
/// Implemented as a position-directed spine walk: at each binding form on the
/// path to `pos` it collects the names lexically visible there, then descends
/// only into the child subtree containing `pos`. It mirrors the scope rules of
/// the occurrence walker (`walk_let_form`/`walk_fn_form`/`walk_letfn_form`/
/// `collect_binding_names`); `collect_binding_targets` additionally records each
/// name's range. Exotic type-spec method params (`reify`/`extend-*`) are left to
/// generic descent — enclosing scopes stay correct, those method params don't
/// bind (outside the `let`/`fn` cases this targets).
///
/// Limitation: only literal binding heads are recognized. A `:lint-as` macro
/// mapped to `defn`/`defmacro` is not treated as a binding form here (that would
/// need the merged `ExtractConfig` + ns aliases, which this pure `source`-only
/// primitive intentionally omits), so its params are not surfaced as locals —
/// the same fall-through as before local support existed, not a regression.
pub fn locals_in_scope_at(source: &str, pos: Position) -> Vec<LocalBinding> {
    let Some(tree) = parse_tree(source) else {
        return vec![];
    };
    locals_in_scope_at_tree(&tree, source, pos)
}

/// [`locals_in_scope_at`] over an already-parsed `tree` of `source`.
pub fn locals_in_scope_at_tree(
    tree: &tree_sitter::Tree,
    source: &str,
    pos: Position,
) -> Vec<LocalBinding> {
    locals_at_node(tree.root_node(), source, pos)
}

/// Parses `source` with the Clojure grammar, or `None` on setup/parse failure.
/// The one-off parse for callers without a cached tree; open documents carry
/// theirs in the document store.
pub fn parse_tree(source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(language()).ok()?;
    parser.parse(source, None)
}

/// [`locals_in_scope_at`] against an already-parsed `root`, so callers that
/// resolve many positions in one buffer (references) parse only once.
fn locals_at_node(root: Node, source: &str, pos: Position) -> Vec<LocalBinding> {
    let mut out = Vec::new();
    descend_into(root, source, pos, &mut out);
    out
}

/// Whether an LSP `range` contains `pos` (inclusive on both ends). Mirrors the
/// rule in `handlers::references::range_contains`, kept local to the extractor.
fn lsp_range_contains(range: Range, pos: Position) -> bool {
    let after_start = range.start.line < pos.line
        || (range.start.line == pos.line && range.start.character <= pos.character);
    let before_end = pos.line < range.end.line
        || (pos.line == range.end.line && pos.character <= range.end.character);
    after_start && before_end
}

/// Descends into whichever named child of `node` contains `pos`, continuing the
/// scope walk there. The generic step for any form that introduces no bindings.
fn descend_into(node: Node, source: &str, pos: Position, out: &mut Vec<LocalBinding>) {
    for child in named_children(node) {
        if lsp_range_contains(node_to_lsp_range(child, source), pos) {
            walk_scope(child, source, pos, out);
            return;
        }
    }
}

/// Dispatches on a node's form: binding forms collect their in-scope names and
/// steer descent; everything else descends generically.
fn walk_scope(node: Node, source: &str, pos: Position, out: &mut Vec<LocalBinding>) {
    if node.kind() == "list_lit" {
        let children = named_children(node);
        if let Some(head) = children.first() {
            // A head is a binding form when it is unqualified or explicitly
            // qualified to `clojure.core` (`(clojure.core/let …)`), matching the
            // occurrence walker's `head_is_core_form`. A qualified `s/def` etc.
            // falls through to generic descent. (An `:as` alias of clojure.core
            // is not resolved here — the primitive has no ns metadata — so
            // `cc/let` is not treated as a binding form; that form is rare.)
            let core_form = head.kind() == "sym_lit"
                && match head.child_by_field_name("namespace") {
                    None => true,
                    Some(ns) => node_text(ns, source) == "clojure.core",
                };
            if core_form {
                let head_text = sym_text(*head, source);
                if let Some(kind) = str_to_defkind(head_text) {
                    walk_scope_def(kind, &children, source, pos, out);
                    return;
                }
                if is_let_like(head_text) {
                    walk_scope_let(&children, source, pos, out);
                    return;
                }
                if head_text == "fn" {
                    walk_scope_fn(&children, source, pos, out);
                    return;
                }
                if head_text == "letfn" {
                    walk_scope_letfn(&children, source, pos, out);
                    return;
                }
                if head_text == "catch" || head_text == "as->" {
                    walk_scope_binding_tail(&children, source, pos, out);
                    return;
                }
            }
        }
    }
    descend_into(node, source, pos, out);
}

/// `(let [pat expr …] body…)` and every `is_let_like` form. Bindings accumulate
/// left-to-right, then the body sees them all.
fn walk_scope_let(children: &[Node], source: &str, pos: Position, out: &mut Vec<LocalBinding>) {
    if let Some(bindings) = children.get(1).filter(|n| n.kind() == "vec_lit") {
        if walk_scope_binding_vec(*bindings, source, pos, out) {
            return; // cursor was inside the binding vector; don't scan the body
        }
    }
    for body in children.iter().skip(2) {
        if lsp_range_contains(node_to_lsp_range(*body, source), pos) {
            walk_scope(*body, source, pos, out);
            return;
        }
    }
}

/// Processes a `[pat expr …]` binding vector, accumulating each LHS pattern's
/// names in order. A pair's LHS is visible to later pairs' RHS and the body, but
/// not its own RHS. Comprehension `:let [..]` recurses; `:when`/`:while` are
/// plain expressions. Returns true when `pos` fell inside the vector (an RHS,
/// nested `:let`, or an LHS), so the caller stops before the body.
fn walk_scope_binding_vec(
    bindings: Node,
    source: &str,
    pos: Position,
    out: &mut Vec<LocalBinding>,
) -> bool {
    let items = named_children(bindings);
    let mut i = 0;
    while i < items.len() {
        let lhs = items[i];
        let rhs = items.get(i + 1).copied();
        if lhs.kind() == "kwd_lit" {
            // `for`/`doseq` modifier: `:let [..]` is a nested binding vector;
            // any other (`:when`/`:while`) has a plain expression RHS.
            if node_text(lhs, source) == ":let" {
                if let Some(v) = rhs.filter(|n| n.kind() == "vec_lit") {
                    if walk_scope_binding_vec(v, source, pos, out) {
                        return true;
                    }
                }
            } else if let Some(r) = rhs {
                if lsp_range_contains(node_to_lsp_range(r, source), pos) {
                    walk_scope(r, source, pos, out);
                    return true;
                }
            }
            i += 2;
            continue;
        }
        if let Some(r) = rhs {
            if lsp_range_contains(node_to_lsp_range(r, source), pos) {
                walk_scope(r, source, pos, out); // cursor in this RHS: LHS not yet bound
                return true;
            }
        }
        collect_binding_targets(lhs, source, out);
        if lsp_range_contains(node_to_lsp_range(lhs, source), pos) {
            return true; // cursor on this LHS; later bindings aren't in scope here
        }
        i += 2;
    }
    lsp_range_contains(node_to_lsp_range(bindings, source), pos)
}

/// `(catch Class name body…)` / `(as-> expr name body…)`: the name binds only
/// for `children[3..]`. The occurrence-walker twin is `walk_binding_tail`.
fn walk_scope_binding_tail(
    children: &[Node],
    source: &str,
    pos: Position,
    out: &mut Vec<LocalBinding>,
) {
    // A cursor in the class/seed expression sees no new binding.
    if let Some(expr) = children.get(1) {
        if lsp_range_contains(node_to_lsp_range(*expr, source), pos) {
            walk_scope(*expr, source, pos, out);
            return;
        }
    }
    let mut bound = Vec::new();
    if let Some(name) = children.get(2).filter(|n| n.kind() == "sym_lit") {
        collect_binding_targets(*name, source, &mut bound);
        // A cursor on the binding site itself yields that binding, like every
        // other binding form (`walk_scope_binding_vec`'s LHS case).
        if lsp_range_contains(node_to_lsp_range(*name, source), pos) {
            out.extend(bound);
            return;
        }
    }
    for body in children.iter().skip(3) {
        if lsp_range_contains(node_to_lsp_range(*body, source), pos) {
            out.extend(bound);
            walk_scope(*body, source, pos, out);
            return;
        }
    }
}

/// `(fn name? [params] body…)` or multi-arity `(fn name? ([params] body…) …)`.
fn walk_scope_fn(children: &[Node], source: &str, pos: Position, out: &mut Vec<LocalBinding>) {
    let mut rest_start = 1;
    if let Some(name) = children.get(1).filter(|n| n.kind() == "sym_lit") {
        collect_binding_targets(*name, source, out); // optional self-reference name
        rest_start = 2;
    }
    walk_scope_fn_tail(&children[rest_start..], source, pos, out);
}

/// Params + bodies of a fn-like tail: a leading vector binds params; each
/// `([params] body…)` list is a per-arity scope entered only when it holds `pos`.
fn walk_scope_fn_tail(parts: &[Node], source: &str, pos: Position, out: &mut Vec<LocalBinding>) {
    let mut params_bound = false;
    for child in parts {
        match child.kind() {
            "vec_lit" if !params_bound => {
                params_bound = true;
                let in_vec = lsp_range_contains(node_to_lsp_range(*child, source), pos);
                // An `:or` default is an expression evaluated in the *enclosing*
                // scope, so this vector's params are not in scope inside it —
                // bind nothing and stop. Anywhere else (a body, or a param
                // binding site) the params do bind.
                if in_vec && pos_in_or_default(*child, source, pos) {
                    return;
                }
                collect_binding_targets(*child, source, out);
                if in_vec {
                    return; // cursor on a param binding site: params self-resolve
                }
            }
            "list_lit" if arity_body(*child) => {
                if lsp_range_contains(node_to_lsp_range(*child, source), pos) {
                    let inner = named_children(*child);
                    let params = inner.first();
                    // Same rule as the single-arity case: bind this arity's
                    // params unless the cursor is inside one of their `:or`
                    // defaults (an enclosing-scope expression).
                    let in_or_default = params
                        .map(|p| {
                            lsp_range_contains(node_to_lsp_range(*p, source), pos)
                                && pos_in_or_default(*p, source, pos)
                        })
                        .unwrap_or(false);
                    if !in_or_default {
                        if let Some(params) = params {
                            collect_binding_targets(*params, source, out);
                        }
                    }
                    for body in inner.iter().skip(1) {
                        if lsp_range_contains(node_to_lsp_range(*body, source), pos) {
                            walk_scope(*body, source, pos, out);
                            return;
                        }
                    }
                    return;
                }
            }
            _ => {
                if lsp_range_contains(node_to_lsp_range(*child, source), pos) {
                    walk_scope(*child, source, pos, out);
                    return;
                }
            }
        }
    }
}

/// `def`-family forms. Function-like ones (`defn`/`defn-`/`defmacro`/
/// `defmethod`) bind params; `defrecord`/`deftype` bind fields; the rest
/// (`def`/`defonce`/`defmulti`/`defprotocol`) introduce no locals.
fn walk_scope_def(
    kind: DefKind,
    children: &[Node],
    source: &str,
    pos: Position,
    out: &mut Vec<LocalBinding>,
) {
    match kind {
        DefKind::Defn | DefKind::DefnPrivate | DefKind::Defmacro => {
            // Skip the name and an optional docstring / attr-map before params.
            let mut rest = 2;
            if children.get(rest).map(|n| n.kind()) == Some("str_lit") {
                rest += 1;
            }
            if children.get(rest).map(|n| n.kind()) == Some("map_lit") {
                rest += 1;
            }
            if rest <= children.len() {
                walk_scope_fn_tail(&children[rest.min(children.len())..], source, pos, out);
            }
        }
        DefKind::Defmethod => {
            // (defmethod name dispatch-val [params] body…): dispatch may itself
            // be a vector, so params start at index 3, not "first vec_lit".
            if let Some(dispatch) = children.get(2) {
                if lsp_range_contains(node_to_lsp_range(*dispatch, source), pos) {
                    walk_scope(*dispatch, source, pos, out);
                    return;
                }
            }
            if children.len() > 3 {
                walk_scope_fn_tail(&children[3..], source, pos, out);
            }
        }
        DefKind::Defrecord | DefKind::Deftype => {
            if let Some(fields) = children.get(2).filter(|n| n.kind() == "vec_lit") {
                collect_binding_targets(*fields, source, out);
            }
            for child in children.iter().skip(3) {
                if lsp_range_contains(node_to_lsp_range(*child, source), pos) {
                    walk_scope(*child, source, pos, out);
                    return;
                }
            }
        }
        _ => {
            for child in children.iter().skip(2) {
                if lsp_range_contains(node_to_lsp_range(*child, source), pos) {
                    walk_scope(*child, source, pos, out);
                    return;
                }
            }
        }
    }
}

/// `(letfn [(name [params] body…) …] body…)`: the fn names are mutually
/// recursive locals visible in every fn body and the letfn body.
fn walk_scope_letfn(children: &[Node], source: &str, pos: Position, out: &mut Vec<LocalBinding>) {
    let specs: Vec<Node> = children
        .get(1)
        .filter(|n| n.kind() == "vec_lit")
        .map(|n| named_children(*n))
        .unwrap_or_default();

    for spec in &specs {
        if spec.kind() == "list_lit" {
            if let Some(name) = named_children(*spec)
                .first()
                .filter(|n| n.kind() == "sym_lit")
            {
                collect_binding_targets(*name, source, out);
            }
        }
    }
    // Cursor inside one of the fn specs → bind that fn's params and descend.
    for spec in &specs {
        if spec.kind() == "list_lit" && lsp_range_contains(node_to_lsp_range(*spec, source), pos) {
            let inner = named_children(*spec);
            walk_scope_fn_tail(&inner[1.min(inner.len())..], source, pos, out);
            return;
        }
    }
    for body in children.iter().skip(2) {
        if lsp_range_contains(node_to_lsp_range(*body, source), pos) {
            walk_scope(*body, source, pos, out);
            return;
        }
    }
}

/// Collects every symbol bound by a binding pattern (plain name, vector and map
/// destructuring), each with its name range. The binding-site twin of
/// [`collect_binding_names`] (which collects only names, for occurrence
/// suppression) — keep the destructuring rules here in sync with it. `&` and `_`
/// are not bindings; `:or` defaults are expressions but their keys still bind.
fn collect_binding_targets(pattern: Node, source: &str, out: &mut Vec<LocalBinding>) {
    match pattern.kind() {
        "sym_lit" => {
            let nn = sym_name_node(pattern);
            let name = node_text(nn, source);
            if name != "&" && name != "_" {
                out.push(LocalBinding {
                    name: name.to_string(),
                    name_range: node_to_lsp_range(nn, source),
                });
            }
        }
        "map_lit" => {
            let items = named_children(pattern);
            for pair in items.chunks(2) {
                let [k, v] = pair else { continue };
                if k.kind() == "kwd_lit" {
                    // `:or {name default}` supplies defaults for names already
                    // bound elsewhere in the destructuring (via :keys/:as/…), so
                    // it introduces no new binding site. Skip it: emitting `name`
                    // here would duplicate the real binding with the wrong (`:or`
                    // key) range and, since the last match shadows, hijack
                    // goto-definition. (This is where the binding-site collection
                    // deliberately diverges from `collect_binding_names`, which
                    // adds `:or` keys to a name-set where a duplicate is inert.)
                    if node_text(*k, source) != ":or" {
                        // :keys/:strs/:syms vectors, :as name, …
                        collect_binding_targets(*v, source, out);
                    }
                } else {
                    // {pattern :key} — the pattern binds; the key does not.
                    collect_binding_targets(*k, source, out);
                }
            }
        }
        _ => {
            for child in named_children(pattern) {
                collect_binding_targets(child, source, out);
            }
        }
    }
}

/// Whether `pos` sits inside an `:or {name default}` *default expression* within
/// a binding pattern. Defaults are evaluated in the enclosing scope, so a cursor
/// there must not see the pattern's own destructured names.
fn pos_in_or_default(node: Node, source: &str, pos: Position) -> bool {
    if node.kind() == "map_lit" {
        let items = named_children(node);
        for pair in items.chunks(2) {
            let [k, v] = pair else { continue };
            if k.kind() == "kwd_lit" && node_text(*k, source) == ":or" && v.kind() == "map_lit" {
                for default in named_children(*v).chunks(2) {
                    if let [_name, expr] = default {
                        if lsp_range_contains(node_to_lsp_range(*expr, source), pos) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    named_children(node)
        .iter()
        .any(|child| pos_in_or_default(*child, source, pos))
}

/// The binding-site range of a local, plus the ranges of every usage that
/// resolves to it — all within the one source file (locals never cross files).
#[derive(Debug, Clone, PartialEq)]
pub struct LocalRefs {
    pub declaration: Range,
    pub usages: Vec<Range>,
    /// Whether the declaration sits in a `:keys`/`:strs`/`:syms` vector, where
    /// the binding name doubles as the key looked up. Renaming such a binding
    /// would silently change which key is read, so rename refuses it.
    pub destructured_key: bool,
}

/// Resolves the local named `name` under `pos` to its declaration and all
/// in-scope usages, for find-references. `None` when `pos` is not on such a
/// local (the caller then falls back to fqn-based references).
///
/// Each candidate occurrence of `name` is re-resolved with [`locals_at_node`]:
/// only those whose innermost binding is *this* declaration are kept, so a
/// nested rebinding of the same name and everything in its scope are excluded,
/// and a same-named global outside the local's scope is not matched.
pub fn local_references_at(source: &str, pos: Position, name: &str) -> Option<LocalRefs> {
    let tree = parse_tree(source)?;
    local_references_at_tree(&tree, source, pos, name)
}

/// [`local_references_at`] over an already-parsed `tree` of `source`.
pub fn local_references_at_tree(
    tree: &tree_sitter::Tree,
    source: &str,
    pos: Position,
    name: &str,
) -> Option<LocalRefs> {
    let root = tree.root_node();

    let mut occurrences = Vec::new();
    collect_name_occurrences(root, source, name, &mut occurrences);

    // The cursor must sit on a real (non-quoted) occurrence of `name`. Quoted
    // data (`'x`) is skipped by `collect_name_occurrences`, so a cursor there
    // references no local even though the name is lexically in scope.
    if !occurrences.iter().any(|r| lsp_range_contains(*r, pos)) {
        return None;
    }

    let declaration = locals_at_node(root, source, pos)
        .into_iter()
        .rev()
        .find(|b| b.name == name)?
        .name_range;

    let mut usages = Vec::new();
    for occ in occurrences {
        if occ == declaration {
            continue; // the binding site itself, reported as the declaration
        }
        let resolved = locals_at_node(root, source, occ.start)
            .into_iter()
            .rev()
            .find(|b| b.name == name)
            .map(|b| b.name_range);
        if resolved == Some(declaration) {
            usages.push(occ);
        }
    }
    Some(LocalRefs {
        declaration,
        usages,
        destructured_key: is_destructured_key(root, source, declaration),
    })
}

/// Whether the binding site at `declaration` is a name inside a
/// `{:keys [...]}` / `:strs` / `:syms` vector — where the symbol is both the
/// local's name and (modulo the key type) the key read from the map.
/// Namespaced entries (`{:keys [foo/bar]}`) live in the same vector, so the
/// same structural check covers them.
fn is_destructured_key(root: Node, source: &str, declaration: Range) -> bool {
    let Some(sym) = find_binding_sym(root, source, declaration) else {
        return false;
    };
    let Some(vec) = sym.parent().filter(|p| p.kind() == "vec_lit") else {
        return false;
    };
    if vec.parent().map(|g| g.kind()) != Some("map_lit") {
        return false;
    }
    vec.prev_named_sibling()
        .map(|kw| {
            // The directive may be namespaced (`{:user/keys [name]}`,
            // `{::keys [name]}`), which binds the same way — match on the
            // keyword's name part, not its literal text.
            kw.kind() == "kwd_lit"
                && kw
                    .child_by_field_name("name")
                    .map(|n| matches!(node_text(n, source), "keys" | "strs" | "syms"))
                    .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// The `sym_lit` whose name range is exactly `range`. Skips quoted data, like
/// `collect_name_occurrences`.
fn find_binding_sym<'a>(node: Node<'a>, source: &str, range: Range) -> Option<Node<'a>> {
    match node.kind() {
        "quoting_lit" => None,
        "sym_lit" if node_to_lsp_range(sym_name_node(node), source) == range => Some(node),
        _ => named_children(node)
            .into_iter()
            .find_map(|child| find_binding_sym(child, source, range)),
    }
}

/// Ranges of every unqualified `sym_lit` named `name`. Skips `'name` quoted
/// data, which is not a usage. (A `(quote name)` list is a rare edge that isn't
/// specially excluded; at worst it lists one extra location.)
fn collect_name_occurrences(node: Node, source: &str, name: &str, out: &mut Vec<Range>) {
    match node.kind() {
        "quoting_lit" => {}
        "sym_lit" => {
            if node.child_by_field_name("namespace").is_none()
                && node_text(sym_name_node(node), source) == name
            {
                out.push(node_to_lsp_range(sym_name_node(node), source));
            }
        }
        _ => {
            for child in named_children(node) {
                collect_name_occurrences(child, source, name, out);
            }
        }
    }
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
            refer_all: vec![],
            as_aliases: vec![],
            core_excludes: vec![],
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

    #[test]
    fn lint_as_def_extracts_defined_name_and_keeps_head_usage() {
        let src = "(ns x (:require [my :refer [defthing]]))\n(defthing foo 1)\n(inc foo)";
        let cfg = ExtractConfig {
            lint_as: HashMap::from([("my/defthing".to_string(), DefKind::Def)]),
        };
        let (_, symbols, occs) =
            extract_full_with(src, std::path::Path::new("x.clj"), &cfg).unwrap();

        // The macro's name argument is now a real definition.
        let foo = symbols
            .iter()
            .find(|s| s.name == "foo")
            .expect("foo should be defined");
        assert_eq!(foo.fqn, "x/foo");
        assert!(matches!(foo.kind, DefKind::Def));

        // The macro head still resolves to the macro (navigable when indexed).
        assert!(occs.iter().any(|o| o.fqn == "my/defthing"));
        // The later `(inc foo)` use points at the new def, and the def-site name
        // is not itself recorded as an occurrence (exactly one `x/foo`).
        assert_eq!(occs.iter().filter(|o| o.fqn == "x/foo").count(), 1);
    }

    #[test]
    fn without_lint_as_macro_defines_nothing() {
        let src = "(ns x (:require [my :refer [defthing]]))\n(defthing foo 1)\n(inc foo)";
        let (_, symbols, _) = extract_full(src, std::path::Path::new("x.clj")).unwrap();
        assert!(
            symbols.iter().all(|s| s.name != "foo"),
            "with no :lint-as, defthing must not define foo"
        );
    }

    // --- locals_in_scope_at -------------------------------------------------

    /// Position at char offset `within` into the `nth` (0-based) occurrence of
    /// `needle` in `source`. Columns are UTF-16 (ASCII in these tests).
    fn pos_of(source: &str, needle: &str, nth: usize, within: usize) -> Position {
        let mut search = 0;
        let mut seen = 0;
        let byte = loop {
            let rel = source[search..].find(needle).expect("needle not found");
            let at = search + rel;
            if seen == nth {
                break at + within;
            }
            seen += 1;
            search = at + needle.len();
        };
        let mut line = 0u32;
        let mut col = 0u32;
        for (i, c) in source.char_indices() {
            if i >= byte {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += c.len_utf16() as u32;
            }
        }
        Position::new(line, col)
    }

    fn local_names(source: &str, pos: Position) -> Vec<String> {
        locals_in_scope_at(source, pos)
            .into_iter()
            .map(|b| b.name)
            .collect()
    }

    #[test]
    fn locals_let_sequential_visibility() {
        // The reported bug: an earlier `let` binding must be visible in a later
        // binding's RHS and in the body, but not in its own RHS.
        let src = "(ns x)\n(defn f []\n  (let [a 1\n        b (+ a 1)]\n    (+ a b)))";

        // In the body `(+ a b)`, both `a` and `b` are in scope.
        let body = local_names(src, pos_of(src, "(+ a b)", 0, 3));
        assert!(body.contains(&"a".to_string()), "body sees a: {:?}", body);
        assert!(body.contains(&"b".to_string()), "body sees b: {:?}", body);

        // In `b`'s RHS `(+ a 1)`, `a` is visible but `b` is not yet.
        let rhs = local_names(src, pos_of(src, "(+ a 1)", 0, 3));
        assert!(rhs.contains(&"a".to_string()), "rhs sees a: {:?}", rhs);
        assert!(
            !rhs.contains(&"b".to_string()),
            "rhs must not see b: {:?}",
            rhs
        );
    }

    #[test]
    fn locals_let_binding_range_points_at_binding_site() {
        let src = "(ns x)\n(defn f []\n  (let [a 1\n        b (+ a 1)]\n    (+ a b)))";
        let binding = locals_in_scope_at(src, pos_of(src, "(+ a b)", 0, 3))
            .into_iter()
            .find(|b| b.name == "a")
            .expect("a in scope");
        assert_eq!(binding.name_range.start, pos_of(src, "[a 1", 0, 1));
    }

    #[test]
    fn locals_innermost_shadows() {
        // A nested `let` re-binding `a` shadows the param; the innermost binding
        // is last and its range is the inner binding site.
        let src = "(ns x)\n(defn f [a]\n  (let [a 2]\n    a))";
        let binds = locals_in_scope_at(src, pos_of(src, "    a))", 0, 4));
        let last_a = binds
            .iter()
            .rev()
            .find(|b| b.name == "a")
            .expect("a in scope");
        assert_eq!(last_a.name_range.start, pos_of(src, "[a 2", 0, 1));
    }

    #[test]
    fn locals_defn_params_visible_in_body() {
        let src = "(ns x)\n(defn g [p q]\n  (+ p q))";
        let body = local_names(src, pos_of(src, "(+ p q)", 0, 3));
        assert!(body.contains(&"p".to_string()));
        assert!(body.contains(&"q".to_string()));
    }

    #[test]
    fn locals_destructuring_binds_all_targets() {
        let src = "(ns x)\n(defn h [{:keys [p] :as m} [q & qs]]\n  (+ p q))";
        let body = local_names(src, pos_of(src, "(+ p q)", 0, 3));
        for name in ["p", "m", "q", "qs"] {
            assert!(
                body.contains(&name.to_string()),
                "{} bound: {:?}",
                name,
                body
            );
        }
        assert!(!body.contains(&"&".to_string()), "& is not a binding");
    }

    #[test]
    fn locals_for_let_modifier() {
        let src = "(ns x)\n(defn i [xs]\n  (for [n xs :let [m (inc n)]]\n    (+ n m)))";
        let body = local_names(src, pos_of(src, "(+ n m)", 0, 3));
        assert!(body.contains(&"n".to_string()), "n bound: {:?}", body);
        assert!(body.contains(&"m".to_string()), "m bound: {:?}", body);
    }

    #[test]
    fn catch_binding_visible_in_its_body() {
        let src = "(ns x)\n(defn f []\n  (try (g)\n       (catch Exception e\n         (log e))))";
        assert!(
            local_names(src, pos_of(src, "(log e)", 0, 2)).contains(&"e".to_string()),
            "catch binding must be visible in its body"
        );
        assert!(
            !local_names(src, pos_of(src, "(g)", 0, 1)).contains(&"e".to_string()),
            "catch binding must not leak into the try body"
        );
    }

    #[test]
    fn catch_and_as_arrow_binding_sites_resolve_to_themselves() {
        // A cursor on the binding symbol itself must yield that binding, so
        // goto-definition/references/rename work from the declaration too.
        let src = "(ns x)\n(defn f []\n  (try (g)\n       (catch Exception e\n         (log e))))";
        assert!(local_names(src, pos_of(src, "Exception e", 0, 10)).contains(&"e".to_string()));

        let src = "(ns x)\n(defn f [y]\n  (as-> y v\n    (inc v)))";
        assert!(local_names(src, pos_of(src, "as-> y v", 0, 7)).contains(&"v".to_string()));
    }

    #[test]
    fn as_arrow_name_visible_in_body() {
        let src = "(ns x)\n(defn f [y]\n  (as-> y v\n    (inc v)))";
        assert!(
            local_names(src, pos_of(src, "(inc v)", 0, 2)).contains(&"v".to_string()),
            "as-> name must be visible in its body"
        );
        assert!(
            !local_names(src, pos_of(src, "as-> y", 0, 5)).contains(&"v".to_string()),
            "as-> name must not be visible in the seed expression"
        );
    }

    #[test]
    fn locals_letfn_names_visible() {
        let src =
            "(ns x)\n(defn j []\n  (letfn [(foo [] (bar))\n          (bar [] 1)]\n    (foo)))";
        let body = local_names(src, pos_of(src, "(foo)))", 0, 1));
        assert!(body.contains(&"foo".to_string()), "foo bound: {:?}", body);
        assert!(body.contains(&"bar".to_string()), "bar bound: {:?}", body);
    }

    #[test]
    fn locals_or_default_does_not_duplicate_binding() {
        // `{:keys [p] :or {p 0}}`: `p` binds exactly once, at the `:keys` site —
        // the `:or` default must not add a second binding (which, shadowing,
        // would hijack goto-definition to the default's position).
        let src = "(ns x)\n(defn f [{:keys [p] :or {p 0}}]\n  p)";
        let binds = locals_in_scope_at(src, pos_of(src, "\n  p)", 0, 3));
        let ps: Vec<_> = binds.iter().filter(|b| b.name == "p").collect();
        assert_eq!(ps.len(), 1, "p bound once: {:?}", binds);
        assert_eq!(ps[0].name_range.start, pos_of(src, "[p]", 0, 1));
    }

    #[test]
    fn locals_or_default_value_evaluated_in_outer_scope() {
        // `{:keys [a] :or {a a}}`: the default value `a` is evaluated in the
        // enclosing scope, so the sibling destructured `a` is NOT in scope there
        // (matching how the occurrence walker treats defaults as outer usages).
        let src = "(ns x)\n(defn f [{:keys [a] :or {a a}}]\n  a)";
        // Cursor on the default value (the second `a` in `{a a}`).
        let at_default = local_names(src, pos_of(src, "{a a}", 0, 3));
        assert!(
            !at_default.contains(&"a".to_string()),
            "default value uses outer scope: {:?}",
            at_default
        );
        // But in the body, the destructured `a` IS in scope.
        let at_body = local_names(src, pos_of(src, "\n  a)", 0, 3));
        assert!(
            at_body.contains(&"a".to_string()),
            "body sees a: {:?}",
            at_body
        );
    }

    #[test]
    fn locals_qualified_clojure_core_let() {
        // A head explicitly qualified to clojure.core is still a binding form.
        let src = "(ns x)\n(defn f []\n  (clojure.core/let [a 1]\n    a))";
        let body = local_names(src, pos_of(src, "    a))", 0, 4));
        assert!(
            body.contains(&"a".to_string()),
            "qualified-core let: {:?}",
            body
        );
    }

    // --- unused bindings ----------------------------------------------------

    /// `(name, start line)` of every binding the analysis reports unused.
    fn unused(src: &str) -> Vec<(String, u32)> {
        extract_analysis_with(src, Path::new("t.clj"), &ExtractConfig::default())
            .unwrap()
            .unused_bindings
            .into_iter()
            .map(|b| (b.name, b.name_range.start.line))
            .collect()
    }

    fn unused_names(src: &str) -> Vec<String> {
        unused(src).into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn unused_let_binding_reported() {
        assert_eq!(unused("(let [a 1 b 2] a)"), vec![("b".to_string(), 0)]);
    }

    #[test]
    fn unused_defn_param_reported() {
        assert_eq!(unused_names("(defn f [x y] x)"), vec!["y"]);
    }

    #[test]
    fn unused_underscore_prefixed_binding_is_opt_out() {
        assert!(unused_names("(defn f [_y] 1)").is_empty());
        assert!(unused_names("(defn f [_] 1)").is_empty());
    }

    #[test]
    fn unused_rebinding_of_same_name_is_not_reported() {
        // The RHS marks the first `x`, the body the second.
        assert!(unused_names("(let [x 1 x (inc x)] x)").is_empty());
    }

    #[test]
    fn unused_destructured_names_reported() {
        let names = unused_names("(defn f [{:keys [a b] :as m}] a)");
        assert_eq!(names, vec!["b", "m"], "got {:?}", names);
    }

    #[test]
    fn unused_or_key_never_reports_on_its_own() {
        // The `:or` key is not a binding site; only the `{a :a}` pattern is.
        let found = unused("(let [{a :a :or {a 1}} m] 1)");
        assert_eq!(found.len(), 1, "one report for `a`: {:?}", found);
        let col = "(let [{".len() as u32;
        let reported = extract_analysis_with(
            "(let [{a :a :or {a 1}} m] 1)",
            Path::new("t.clj"),
            &ExtractConfig::default(),
        )
        .unwrap()
        .unused_bindings;
        assert_eq!(reported[0].name_range.start.character, col);
    }

    #[test]
    fn unused_fn_self_name_is_exempt() {
        assert!(unused_names("(fn me [x] x)").is_empty());
        assert_eq!(unused_names("(fn me [x] 1)"), vec!["x"]);
    }

    #[test]
    fn unused_letfn_name_is_exempt_but_params_are_not() {
        assert_eq!(unused_names("(letfn [(g [p] 1)] (g 1))"), vec!["p"]);
        assert!(unused_names("(letfn [(g [p] p)] 1)").is_empty());
    }

    #[test]
    fn unused_record_fields_and_method_params_are_exempt() {
        assert!(unused_names("(defrecord R [a b] P (m [this q] a))").is_empty());
    }

    #[test]
    fn unused_catch_binding_reported() {
        assert_eq!(unused_names("(try (f) (catch Exception e nil))"), vec!["e"]);
    }

    #[test]
    fn unused_loop_binding_used_by_recur() {
        assert!(unused_names("(loop [i 0] (when (< i 3) (recur (inc i))))").is_empty());
    }

    #[test]
    fn unused_for_let_modifier_reported() {
        assert_eq!(unused_names("(for [x xs :let [y (inc x)]] x)"), vec!["y"]);
    }

    #[test]
    fn unused_defmethod_param_reported() {
        assert_eq!(unused_names("(defmethod m :k [_ arg] 1)"), vec!["arg"]);
    }

    #[test]
    fn unused_syntax_quote_gensym_is_matched_by_name() {
        assert!(unused_names("(defmacro w [x] `(let [v# ~x] v#))").is_empty());
    }

    #[test]
    fn unused_as_arrow_name_reported() {
        assert_eq!(unused_names("(as-> 1 v)"), vec!["v"]);
        assert!(unused_names("(as-> 1 v (inc v))").is_empty());
    }

    #[test]
    fn unused_reported_per_arity() {
        let names = unused_names("(defn f ([a] a) ([a b] a))");
        assert_eq!(names, vec!["b"], "got {:?}", names);
    }

    // --- local_references_at ------------------------------------------------

    #[test]
    fn local_refs_let_declaration_and_usages() {
        let src = "(ns x)\n(defn f []\n  (let [a 1]\n    (+ a a)))";
        let refs = local_references_at(src, pos_of(src, "[a 1", 0, 1), "a").expect("local");
        assert_eq!(refs.declaration.start, pos_of(src, "[a 1", 0, 1));
        assert_eq!(refs.usages.len(), 2, "two body usages: {:?}", refs.usages);
        // Resolving from a usage gives the same declaration.
        let from_use = local_references_at(src, pos_of(src, "(+ a a)", 0, 3), "a").expect("local");
        assert_eq!(from_use.declaration, refs.declaration);
        assert_eq!(from_use.usages.len(), 2);
    }

    #[test]
    fn local_refs_flags_keys_destructured() {
        // `a` comes from `{:keys [a]}`: renaming it would change the key
        // looked up, so the caller must be able to refuse.
        let src = "(ns x)\n(defn f [{:keys [a]}] (inc a))";
        let refs = local_references_at(src, pos_of(src, "(inc a)", 0, 5), "a").expect("local");
        assert!(refs.destructured_key, "{{:keys [a]}} binding: {:?}", refs);
        assert_eq!(refs.usages.len(), 1, "one body usage: {:?}", refs.usages);
    }

    #[test]
    fn local_refs_flags_strs_and_syms() {
        for kw in [":strs", ":syms"] {
            let src = format!("(ns x)\n(defn f [{{{} [a]}}] (inc a))", kw);
            let refs =
                local_references_at(&src, pos_of(&src, "(inc a)", 0, 5), "a").expect("local");
            assert!(refs.destructured_key, "{} binding: {:?}", kw, refs);
        }
    }

    #[test]
    fn local_refs_flags_namespaced_keys_directive() {
        // `{:user/keys [a]}` and `{::keys [a]}` bind from `:user/a` / `::a`,
        // so the name is still the key.
        for directive in [":user/keys", "::keys", ":user/syms"] {
            let src = format!("(ns x)\n(defn f [{{{} [a]}}] (inc a))", directive);
            let refs =
                local_references_at(&src, pos_of(&src, "(inc a)", 0, 5), "a").expect("local");
            assert!(refs.destructured_key, "{} binding: {:?}", directive, refs);
        }
    }

    #[test]
    fn local_refs_plain_map_key_is_not_destructured_key() {
        // `{a :a}` names the binding explicitly, so renaming `a` is safe.
        let src = "(ns x)\n(let [{a :a} m] a)";
        let refs = local_references_at(src, pos_of(src, "{a :a}", 0, 1), "a").expect("local");
        assert!(!refs.destructured_key, "{{a :a}} binding: {:?}", refs);
    }

    #[test]
    fn local_refs_or_key_is_a_usage() {
        // The `:or` key resolves to the same declaration as the body usage.
        let src = "(ns x)\n(let [{a :a :or {a 1}} m] a)";
        let refs = local_references_at(src, pos_of(src, "] a)", 0, 2), "a").expect("local");
        assert!(!refs.destructured_key, "{:?}", refs);
        assert_eq!(refs.usages.len(), 2, ":or key + body: {:?}", refs.usages);
    }

    #[test]
    fn local_refs_vector_binding_is_not_destructured_key() {
        let src = "(ns x)\n(let [[a b] v] (+ a b))";
        let refs = local_references_at(src, pos_of(src, "[[a b]", 0, 2), "a").expect("local");
        assert!(!refs.destructured_key, "vector binding: {:?}", refs);
    }

    #[test]
    fn local_refs_exclude_shadowed_rebinding() {
        // Outer `a` has one usage (the trailing body `a`); the inner `let`'s
        // rebinding and its body must not be counted.
        let src = "(ns x)\n(defn f []\n  (let [a 1]\n    (let [a 2]\n      a)\n    a))";
        let refs = local_references_at(src, pos_of(src, "[a 1", 0, 1), "a").expect("local");
        assert_eq!(
            refs.usages.len(),
            1,
            "only the outer body use: {:?}",
            refs.usages
        );
        assert_eq!(refs.usages[0].start.line, 5);
    }

    #[test]
    fn local_refs_from_fn_param() {
        let src = "(ns x)\n(defn f [a]\n  (+ a a))";
        let refs = local_references_at(src, pos_of(src, "[a]", 0, 1), "a").expect("local param");
        assert_eq!(refs.declaration.start, pos_of(src, "[a]", 0, 1));
        assert_eq!(refs.usages.len(), 2);
    }

    #[test]
    fn local_refs_sequential_binding_rhs_counts() {
        // `a` is used in a later binding's RHS (`b a`) and in the body.
        let src = "(ns x)\n(defn f []\n  (let [a 1\n        b a]\n    (+ a b)))";
        let refs = local_references_at(src, pos_of(src, "[a 1", 0, 1), "a").expect("local");
        assert_eq!(refs.usages.len(), 2, "rhs + body: {:?}", refs.usages);
    }

    #[test]
    fn local_refs_none_for_non_local() {
        // `undefined` is not bound anywhere → not a local.
        let src = "(ns x)\n(defn f []\n  (+ undefined 1))";
        assert!(local_references_at(src, pos_of(src, "undefined", 0, 2), "undefined").is_none());
    }

    #[test]
    fn local_refs_none_on_quoted_symbol() {
        // A cursor on quoted data `'x` is not a real usage of the local `x`,
        // even though `x` is lexically in scope.
        let src = "(ns y)\n(defn f []\n  (let [x 1]\n    'x))";
        assert!(local_references_at(src, pos_of(src, "'x", 0, 1), "x").is_none());
    }

    #[test]
    fn position_to_point_counts_utf16() {
        // The emoji is one UTF-16 surrogate pair (2 units) and 4 bytes, so the
        // LSP column right after "😀 " is 3 while the byte column is 5.
        let src = "😀 (foo bar)";
        let pt = position_to_point(
            src,
            Position {
                line: 0,
                character: 3,
            },
        )
        .expect("line 0 exists");
        assert_eq!(pt, tree_sitter::Point { row: 0, column: 5 });
    }

    #[test]
    fn position_to_point_clamps_past_end() {
        let src = "(foo)\n(bar)";
        let pt = position_to_point(
            src,
            Position {
                line: 1,
                character: 99,
            },
        )
        .expect("line 1 exists");
        assert_eq!(pt, tree_sitter::Point { row: 1, column: 5 });
        assert!(position_to_point(
            src,
            Position {
                line: 7,
                character: 0
            }
        )
        .is_none());
    }

    #[test]
    fn node_path_at_lists_innermost_first() {
        let src = "(defn f [x] (let [y 1] y))";
        let tree = parse(src);
        let kinds: Vec<&str> = node_path_at(tree.root_node(), src, pos_of(src, "] y))", 0, 2))
            .iter()
            .map(|n| n.kind())
            .collect();
        assert_eq!(kinds, vec!["sym_lit", "list_lit", "list_lit"]);
    }

    #[test]
    fn node_path_at_normalizes_token_parts() {
        // The descendant lookup lands on `sym_ns`; the path must start at the
        // enclosing `sym_lit` instead.
        let src = "(str/join x)";
        let tree = parse(src);
        let kinds: Vec<&str> = node_path_at(tree.root_node(), src, pos_of(src, "str/join", 0, 1))
            .iter()
            .map(|n| n.kind())
            .collect();
        assert_eq!(kinds, vec!["sym_lit", "list_lit"]);
    }
}
