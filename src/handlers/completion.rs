use std::collections::HashSet;

use anyhow::Result;
use tower_lsp::lsp_types::*;

use super::builtins;
use super::matching::match_score;
use crate::document::DocumentStore;
use crate::index::{extractor, CoreSymbol, DefKind, Index};

pub fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: CompletionParams,
) -> Result<Option<CompletionResponse>> {
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;

    let prefix = documents.word_at(&uri, pos).unwrap_or_default();

    tracing::info!("completion: prefix={}", prefix);

    let path = uri
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("invalid file URI"))?;
    let current_ns = index.file_ns(&path).unwrap_or_default();

    let mut items = complete_symbols(index, &prefix, &current_ns);

    // Locals (let/fn/loop/… bound names) in scope at the cursor. They shadow
    // globals, so offer them ahead of the index symbols. Qualified prefixes
    // (`alias/…`) can't name a local, so skip the walk there.
    if !prefix.contains('/') {
        let mut merged = local_completions(documents, &uri, pos, &prefix);
        merged.extend(items);
        items = merged;
    }

    if items.is_empty() {
        return Ok(None);
    }

    // Incomplete on purpose: the guardrails in `tier_allowed` and the namespace
    // cap mean a longer prefix can yield candidates this list does not hold
    // (`d` is prefix-only, `dd` substring-matches `add`). A complete list would
    // let the client filter its cache instead of asking again, and those
    // candidates would never appear.
    Ok(Some(CompletionResponse::List(CompletionList {
        is_incomplete: true,
        items,
    })))
}

/// In-scope local bindings at `pos` whose name matches `prefix`, innermost-first
/// and de-duplicated by name (an inner binding shadows an outer one). Locals are
/// pool 0, so within a match tier they rank above every var and core name.
fn local_completions(
    documents: &DocumentStore,
    uri: &Url,
    pos: Position,
    prefix: &str,
) -> Vec<CompletionItem> {
    let Some(text) = documents.text(uri) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for binding in extractor::locals_in_scope_at(&text, pos).into_iter().rev() {
        let Some(tier) = matched(&binding.name, prefix, Pool::Local) else {
            continue;
        };
        if seen.insert(binding.name.clone()) {
            push(
                &mut out,
                CompletionItem {
                    label: binding.name.clone(),
                    detail: Some("local".to_string()),
                    kind: Some(CompletionItemKind::VARIABLE),
                    ..Default::default()
                },
                tier,
                Pool::Local,
            );
        }
    }
    out
}

/// The candidate pools, ordered by how local their names are. The digit goes
/// into `sort_text` after the match tier, so an exact match anywhere beats a
/// prefix match anywhere, and within a tier the most local pool wins.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pool {
    Local,
    CurrentNs,
    Referred,
    Core,
    Alias,
    Namespace,
    Java,
}

impl Pool {
    fn digit(self) -> u8 {
        match self {
            Pool::Local => 0,
            Pool::CurrentNs => 1,
            Pool::Referred => 2,
            Pool::Core => 3,
            // Aliases and namespaces are the same rank; they differ only in the
            // guardrails `tier_allowed` applies to them.
            Pool::Alias | Pool::Namespace => 4,
            Pool::Java => 5,
        }
    }
}

/// The match tier `name` earns for `prefix` in `pool`, or `None` when it is no
/// candidate there.
fn matched(name: &str, prefix: &str, pool: Pool) -> Option<u8> {
    let tier = match_score(name, prefix)?;
    tier_allowed(tier, prefix, pool).then_some(tier)
}

/// Guardrails on the loose tiers. An empty prefix is exempt: `match_score`
/// scores everything tier 3 there, and the pools that would flood the list
/// (aliases, namespaces) skip an empty prefix entirely.
fn tier_allowed(tier: u8, prefix: &str, pool: Pool) -> bool {
    if prefix.is_empty() {
        return true;
    }
    // One character is too little to fuzzy-match on: `d` would substring-match
    // most of clojure.core. A single char stays a prefix search.
    if tier >= 2 && prefix.chars().count() < 2 {
        return false;
    }
    // `str` subsequence-matches hundreds of library namespaces; requiring a
    // substring keeps require completion readable.
    if pool == Pool::Namespace && tier >= 3 {
        return false;
    }
    true
}

/// Adds `item` with the `tier-pool-label` `sort_text` that ranks it.
fn push(items: &mut Vec<CompletionItem>, item: CompletionItem, tier: u8, pool: Pool) {
    items.push(CompletionItem {
        sort_text: Some(format!("{}-{}-{}", tier, pool.digit(), item.label)),
        ..item
    });
}

pub fn complete_symbols(index: &Index, prefix: &str, current_ns: &str) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let ns_meta = index.ns_meta(current_ns);

    if let Some((alias, name_prefix)) = prefix.split_once('/') {
        // Qualified completion: alias/prefix
        let full_ns = ns_meta.as_ref().and_then(|m| m.aliases.get(alias)).cloned();

        if let Some(full_ns) = full_ns {
            if let Some(fqns) = index.ns_symbols.get(&full_ns) {
                for fqn in fqns.iter() {
                    if let Some(sym) = index.symbols.get(fqn) {
                        if let Some(tier) = matched(&sym.name, name_prefix, Pool::CurrentNs) {
                            push(
                                &mut items,
                                symbol_to_completion(&sym, Some(alias)),
                                tier,
                                Pool::CurrentNs,
                            );
                        }
                    }
                }
            }
        } else if let Some(class_fqn) = super::java::resolve_class(index, alias, current_ns) {
            // Java static members: `Class/prefix` (the alias resolves to a JDK
            // class, not a Clojure require alias).
            if let Some(info) = index.jdk().and_then(|j| j.class(&class_fqn)) {
                for m in &info.methods {
                    if !m.is_static {
                        continue;
                    }
                    if let Some(tier) = matched(&m.name, name_prefix, Pool::Java) {
                        push(
                            &mut items,
                            java_member_completion(alias, &m.name, &class_fqn, true),
                            tier,
                            Pool::Java,
                        );
                    }
                }
                for f in &info.fields {
                    if !f.is_static {
                        continue;
                    }
                    if let Some(tier) = matched(&f.name, name_prefix, Pool::Java) {
                        push(
                            &mut items,
                            java_member_completion(alias, &f.name, &class_fqn, false),
                            tier,
                            Pool::Java,
                        );
                    }
                }
            }
        }
    } else {
        // Pool A: current namespace symbols
        if let Some(fqns) = index.ns_symbols.get(current_ns) {
            for fqn in fqns.iter() {
                if let Some(sym) = index.symbols.get(fqn) {
                    if let Some(tier) = matched(&sym.name, prefix, Pool::CurrentNs) {
                        push(
                            &mut items,
                            symbol_to_completion(&sym, None),
                            tier,
                            Pool::CurrentNs,
                        );
                    }
                }
            }
        }

        if let Some(meta) = &ns_meta {
            // Pool B: referred symbols. An explicitly referred name is valid
            // whether or not its namespace is indexed yet — the clojure JAR may
            // still be loading — so offer it bare when the fqn misses.
            for (refer_name, fqn) in &meta.refers {
                let Some(tier) = matched(refer_name, prefix, Pool::Referred) else {
                    continue;
                };
                let item = match index.symbols.get(fqn) {
                    Some(sym) => symbol_to_completion(&sym, None),
                    None => referred_completion(refer_name, fqn),
                };
                push(&mut items, item, tier, Pool::Referred);
            }

            // Pool B2: `:refer :all` / `(:use ns)` namespaces — every public var
            // of those is a bare name here. Needs the namespace indexed (unlike
            // Pool B, only the names of explicit refers are known up front).
            // Explicit refers are already offered above, and private vars are
            // indexed for jar navigation but are not referable.
            for ns in &meta.refer_all {
                let Some(fqns) = index.ns_symbols.get(ns) else {
                    continue;
                };
                for fqn in fqns.iter() {
                    if let Some(sym) = index.symbols.get(fqn) {
                        if sym.kind == DefKind::DefnPrivate || meta.refers.contains_key(&sym.name) {
                            continue;
                        }
                        if let Some(tier) = matched(&sym.name, prefix, Pool::Referred) {
                            push(
                                &mut items,
                                symbol_to_completion(&sym, None),
                                tier,
                                Pool::Referred,
                            );
                        }
                    }
                }
            }
        }

        // Pool C: builtins. A let-go project gets its own core — special forms,
        // native fns, and the live `.lg` `core` namespace — instead of the
        // static clojure.core list, which would offer names let-go lacks and
        // mislabel the ones it has. Clojure projects keep the clojure.core list.
        if index.letgo_core() {
            for sf in builtins::special_forms(true) {
                if let Some(tier) = matched(sf.name, prefix, Pool::Core) {
                    push(&mut items, special_form_to_completion(sf), tier, Pool::Core);
                }
            }
            // Native (Go `ns.Def`) names: the set harvested from this let-go
            // version's `lang.go` when available, else the static fallback.
            // Names the live `.lg` `core` index already provides are skipped so
            // they aren't offered twice (the `.lg` entry below has real source).
            let harvested = index.letgo_native_names();
            let native_names: Vec<&str> = match &harvested {
                Some(names) => names.iter().map(String::as_str).collect(),
                None => builtins::native_names().to_vec(),
            };
            for &name in &native_names {
                let Some(tier) = matched(name, prefix, Pool::Core) else {
                    continue;
                };
                if index.lookup_in_ns("core", name).is_none() {
                    let core = index.core_symbols.iter().find(|c| c.name == name);
                    push(
                        &mut items,
                        letgo_native_to_completion(name, core),
                        tier,
                        Pool::Core,
                    );
                }
            }
            if let Some(fqns) = index.ns_symbols.get("core") {
                for fqn in fqns.iter() {
                    if let Some(sym) = index.symbols.get(fqn) {
                        if let Some(tier) = matched(&sym.name, prefix, Pool::Core) {
                            push(
                                &mut items,
                                symbol_to_completion(&sym, None),
                                tier,
                                Pool::Core,
                            );
                        }
                    }
                }
            }
        } else {
            // A name the file excludes from `clojure.core` is not core's here.
            let excluded = |name: &str| {
                ns_meta
                    .as_ref()
                    .is_some_and(|m| m.core_excludes.iter().any(|e| e == name))
            };
            for core_sym in &index.core_symbols {
                if excluded(&core_sym.name) {
                    continue;
                }
                if let Some(tier) = matched(&core_sym.name, prefix, Pool::Core) {
                    push(
                        &mut items,
                        core_symbol_to_completion(core_sym),
                        tier,
                        Pool::Core,
                    );
                }
            }
            // Clojure special forms aren't clojure.core vars, so offer them too.
            for sf in builtins::special_forms(false) {
                if let Some(tier) = matched(sf.name, prefix, Pool::Core) {
                    push(&mut items, special_form_to_completion(sf), tier, Pool::Core);
                }
            }
        }

        // Pools D and E only fire for non-empty prefixes: on an empty prefix
        // they would dump every indexed namespace into the list.
        if !prefix.is_empty() {
            // Pool D: aliases of the current namespace — completing "metr"
            // to "metrics" lets the user then complete "metrics/…"
            if let Some(meta) = &ns_meta {
                for (alias, full_ns) in &meta.aliases {
                    if let Some(tier) = matched(alias, prefix, Pool::Alias) {
                        push(
                            &mut items,
                            CompletionItem {
                                label: alias.clone(),
                                detail: Some(format!("alias for {}", full_ns)),
                                kind: Some(CompletionItemKind::MODULE),
                                ..Default::default()
                            },
                            tier,
                            Pool::Alias,
                        );
                    }
                }
            }

            // Pool E: namespace names (project + libraries) — makes
            // completion inside (:require …) work. Library-internal
            // `.impl`/`.internal` namespaces are indexed for navigation but
            // omitted here to keep require completion clean. Capped, since a
            // substring match reaches far more namespaces than a prefix did.
            let mut ns_hits: Vec<(u8, String)> = Vec::new();
            for entry in index.namespaces.iter() {
                let ns = entry.key();
                if is_internal_ns(ns) {
                    continue;
                }
                if let Some(tier) = matched(ns, prefix, Pool::Namespace) {
                    ns_hits.push((tier, ns.clone()));
                }
            }
            ns_hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            ns_hits.truncate(NAMESPACE_LIMIT);
            for (tier, ns) in ns_hits {
                push(
                    &mut items,
                    CompletionItem {
                        label: ns,
                        detail: Some("namespace".to_string()),
                        kind: Some(CompletionItemKind::MODULE),
                        ..Default::default()
                    },
                    tier,
                    Pool::Namespace,
                );
            }

            // Pool F: built-in Java class names. Gated on a PascalCase prefix so
            // ordinary (lowercase) completion isn't flooded with JDK classes, and
            // capped for short prefixes. Prefix-only: fuzzy matching over the
            // whole JDK would cost more than it is worth.
            if prefix.chars().next().is_some_and(|c| c.is_uppercase()) {
                if let Some(jdk) = index.jdk() {
                    for fqn in jdk
                        .class_names_with_prefix(prefix)
                        .into_iter()
                        .take(JAVA_CLASS_LIMIT)
                    {
                        let item = java_class_completion(fqn);
                        let tier = match_score(&item.label, prefix).unwrap_or(1);
                        push(&mut items, item, tier, Pool::Java);
                    }
                }
            }
        }
    }

    items
}

/// Cap on namespace completions, so a short substring doesn't offer every
/// library namespace that happens to contain it.
const NAMESPACE_LIMIT: usize = 50;

/// Cap on Java class-name completions, so a short PascalCase prefix doesn't dump
/// hundreds of JDK classes into the list.
const JAVA_CLASS_LIMIT: usize = 50;

fn java_member_completion(
    alias: &str,
    name: &str,
    class_fqn: &str,
    is_method: bool,
) -> CompletionItem {
    // Label as `Class/member` (e.g. `Thread/sleep`), mirroring Clojure
    // alias-qualified completion — the editor filters the list against the typed
    // `Class/...` word, so a bare `member` label would be filtered out.
    CompletionItem {
        label: format!("{}/{}", alias, name),
        detail: Some(format!(
            "{} (static {})",
            class_fqn,
            if is_method { "method" } else { "field" }
        )),
        kind: Some(if is_method {
            CompletionItemKind::METHOD
        } else {
            CompletionItemKind::FIELD
        }),
        ..Default::default()
    }
}

fn java_class_completion(fqn: &str) -> CompletionItem {
    let simple = fqn.rsplit('.').next().unwrap_or(fqn);
    CompletionItem {
        label: simple.to_string(),
        detail: Some(fqn.to_string()),
        kind: Some(CompletionItemKind::CLASS),
        ..Default::default()
    }
}

/// Library-internal namespaces (`*.impl` / `*.internal`) are indexed for
/// navigation/hover/references but kept out of require completion.
fn is_internal_ns(ns: &str) -> bool {
    ns.ends_with(".impl") || ns.ends_with(".internal")
}

fn symbol_to_completion(sym: &crate::index::Symbol, alias: Option<&str>) -> CompletionItem {
    let label = match alias {
        Some(a) => format!("{}/{}", a, sym.name),
        None => sym.name.clone(),
    };

    CompletionItem {
        label,
        detail: Some(format!("{} ({})", sym.ns, params_display(&sym.params))),
        documentation: sym.doc.as_ref().map(|d| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: d.clone(),
            })
        }),
        kind: Some(defkind_to_completion_kind(&sym.kind)),
        ..Default::default()
    }
}

/// A `:refer`red name whose namespace is not indexed yet: the user named it in
/// the ns form, so it is offered on the strength of that alone — no arglists or
/// doc to show, and `FUNCTION` as the safest guess at its kind.
fn referred_completion(name: &str, fqn: &str) -> CompletionItem {
    let ns = fqn.rsplit_once('/').map(|(ns, _)| ns).unwrap_or(fqn);
    CompletionItem {
        label: name.to_string(),
        detail: Some(format!("{} (referred)", ns)),
        kind: Some(CompletionItemKind::FUNCTION),
        ..Default::default()
    }
}

fn core_symbol_to_completion(sym: &crate::index::CoreSymbol) -> CompletionItem {
    CompletionItem {
        label: sym.name.clone(),
        detail: Some(format!("clojure.core ({})", sym.params)),
        documentation: if sym.doc.is_empty() {
            None
        } else {
            Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: sym.doc.clone(),
            }))
        },
        kind: Some(CompletionItemKind::FUNCTION),
        ..Default::default()
    }
}

fn special_form_to_completion(sf: &builtins::SpecialForm) -> CompletionItem {
    CompletionItem {
        label: sf.name.to_string(),
        detail: Some(format!("special form {}", sf.usage)),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: sf.doc.to_string(),
        })),
        kind: Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    }
}

/// A let-go native core fn/var in completion, labelled native. Doc/arglists are
/// borrowed from the clojure.core table (`core`) when present; names let-go has
/// but clojure.core lacks are offered bare (name only).
fn letgo_native_to_completion(name: &str, core: Option<&CoreSymbol>) -> CompletionItem {
    let params = core.map(|c| c.params.as_str()).unwrap_or("");
    let detail = if params.is_empty() {
        "let-go core (native)".to_string()
    } else {
        format!("let-go core (native) ({})", params)
    };
    CompletionItem {
        label: name.to_string(),
        detail: Some(detail),
        documentation: core.filter(|c| !c.doc.is_empty()).map(|c| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: c.doc.clone(),
            })
        }),
        kind: Some(CompletionItemKind::FUNCTION),
        ..Default::default()
    }
}

fn defkind_to_completion_kind(kind: &DefKind) -> CompletionItemKind {
    match kind {
        DefKind::Defn | DefKind::DefnPrivate | DefKind::Defmacro | DefKind::Deftest => {
            CompletionItemKind::FUNCTION
        }
        DefKind::Def | DefKind::Defonce => CompletionItemKind::VARIABLE,
        DefKind::Defprotocol => CompletionItemKind::INTERFACE,
        DefKind::Defrecord | DefKind::Deftype => CompletionItemKind::CLASS,
        _ => CompletionItemKind::VALUE,
    }
}

fn params_display(params: &[String]) -> String {
    if params.is_empty() {
        String::new()
    } else {
        params.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{NsMeta, Symbol, SymbolSource};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tower_lsp::lsp_types::Range;

    fn core_sym(name: &str, params: &str) -> CoreSymbol {
        CoreSymbol {
            name: name.to_string(),
            params: params.to_string(),
            doc: String::new(),
        }
    }

    fn lib_sym(name: &str, ns: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            fqn: format!("{}/{}", ns, name),
            ns: ns.to_string(),
            kind: DefKind::Defn,
            params: vec![],
            doc: None,
            file: PathBuf::from("core.lg"),
            source: SymbolSource::Dir(PathBuf::from("core")),
            range: Range::default(),
            name_range: Range::default(),
            private: false,
        }
    }

    fn meta(name: &str, file: &str) -> NsMeta {
        NsMeta {
            name: name.to_string(),
            file: PathBuf::from(file),
            aliases: HashMap::new(),
            refers: HashMap::new(),
            requires: vec![],
            imports: HashMap::new(),
            refer_all: vec![],
            as_aliases: vec![],
            core_excludes: vec![],
        }
    }

    fn labels(index: &Index, prefix: &str) -> Vec<String> {
        complete_symbols(index, prefix, "app")
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    #[test]
    fn completes_java_static_members_and_class_names() {
        let (index, _zip) = crate::handlers::java::test_fixture();
        let java_labels = |prefix: &str| -> Vec<String> {
            complete_symbols(&index, prefix, "app.core")
                .into_iter()
                .map(|i| i.label)
                .collect()
        };
        // `Class/prefix` → static members, labelled `Class/member` so the editor
        // (which filters against the typed `Class/...`) keeps them.
        assert!(
            java_labels("Greeter/gr").contains(&"Greeter/greet".to_string()),
            "imported class statics: {:?}",
            java_labels("Greeter/gr")
        );
        // Same for an auto-`java.lang` class with no `:import`.
        assert!(
            java_labels("Sample/o").contains(&"Sample/of".to_string()),
            "auto-java.lang statics: {:?}",
            java_labels("Sample/o")
        );
        // PascalCase prefix → class names (imported, then auto-`java.lang`).
        assert!(java_labels("Gr").contains(&"Greeter".to_string()));
        assert!(java_labels("Sam").contains(&"Sample".to_string()));
    }

    fn letgo_index() -> Index {
        let mut index = Index::new();
        // Current (project) ns.
        index.insert_file(meta("app", "app.lg"), vec![], vec![]);
        // Live `.lg` core ns with `map`.
        index.insert_lib_file(meta("core", "core.lg"), vec![lib_sym("map", "core")]);
        index.core_symbols = vec![
            core_sym("count", "([coll])"),    // a real let-go native
            core_sym("zzz-clojure-only", ""), // a clojure.core name let-go lacks
        ];
        index.mark_letgo_core();
        index
    }

    #[test]
    fn letgo_completion_offers_builtins_not_static_core() {
        let index = letgo_index();
        assert!(
            labels(&index, "i").contains(&"if".to_string()),
            "special form if"
        );
        assert!(
            labels(&index, "cou").contains(&"count".to_string()),
            "native count"
        );
        assert!(
            labels(&index, "ma").contains(&"map".to_string()),
            "live .lg core map"
        );
        // The full clojure.core static pool is NOT dumped for let-go: a
        // clojure.core name that is not a let-go native must not be offered.
        assert!(!labels(&index, "zzz").contains(&"zzz-clojure-only".to_string()));
    }

    #[test]
    fn letgo_native_completion_is_labelled() {
        let index = letgo_index();
        let item = complete_symbols(&index, "count", "app")
            .into_iter()
            .find(|i| i.label == "count")
            .expect("count offered");
        assert!(item.detail.unwrap().contains("native"));
    }

    #[test]
    fn letgo_harvested_natives_offer_version_vars() {
        // A pinned let-go version whose lang.go was harvested: completion offers
        // the harvested names (e.g. the new `*command-line-args*` var) directly,
        // not just the static NATIVE_NAMES list.
        let mut index = Index::new();
        index.insert_file(meta("app", "app.lg"), vec![], vec![]);
        index.insert_lib_file(meta("core", "core.lg"), vec![lib_sym("map", "core")]);
        index.core_symbols = vec![core_sym("*command-line-args*", "")];
        index.mark_letgo_core();
        index.set_letgo_native(vec![
            "*command-line-args*".to_string(),
            "count".to_string(), // harvested native with no clojure.core entry here
            "map".to_string(),   // also a live `.lg` core fn → must not double up
        ]);

        assert!(
            labels(&index, "*com").contains(&"*command-line-args*".to_string()),
            "harvested var offered: {:?}",
            labels(&index, "*com")
        );
        // A harvested native with no clojure.core doc entry is still offered.
        assert!(labels(&index, "cou").contains(&"count".to_string()));
        // `map` is served by the live `.lg` core; the harvested duplicate is
        // skipped so it appears exactly once.
        let maps = labels(&index, "map");
        assert_eq!(
            maps.iter().filter(|s| *s == "map").count(),
            1,
            "map offered once: {:?}",
            maps
        );
    }

    #[test]
    fn clojure_project_uses_static_core_pool() {
        // Marker unset → the clojure.core static pool is used as before.
        let mut index = Index::new();
        index.insert_file(meta("app", "app.clj"), vec![], vec![]);
        index.core_symbols = vec![core_sym("zzz-clojure-only", "")];
        assert!(labels(&index, "zzz").contains(&"zzz-clojure-only".to_string()));
    }

    #[test]
    fn clojure_completion_offers_special_forms_and_core() {
        // Marker unset → Clojure path offers clojure.core fns AND special forms.
        let mut index = Index::new();
        index.insert_file(meta("app", "app.clj"), vec![], vec![]);
        index.core_symbols = vec![core_sym("inc", "([x])")];
        let l = labels(&index, "i");
        assert!(l.contains(&"if".to_string()), "special form if");
        assert!(l.contains(&"inc".to_string()), "clojure.core inc");
    }
}
