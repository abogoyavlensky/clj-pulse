use anyhow::Result;
use tower_lsp::lsp_types::*;

use super::builtins;
use crate::document::DocumentStore;
use crate::index::{CoreSymbol, DefKind, Index};

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

    let items = complete_symbols(index, &prefix, &current_ns);

    if items.is_empty() {
        return Ok(None);
    }

    Ok(Some(CompletionResponse::Array(items)))
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
                        if sym.name.starts_with(name_prefix) {
                            items.push(symbol_to_completion(&sym, Some(alias)));
                        }
                    }
                }
            }
        } else if let Some(class_fqn) = super::java::resolve_class(index, alias, current_ns) {
            // Java static members: `Class/prefix` (the alias resolves to a JDK
            // class, not a Clojure require alias).
            if let Some(info) = index.jdk().and_then(|j| j.class(&class_fqn)) {
                for m in &info.methods {
                    if m.is_static && m.name.starts_with(name_prefix) {
                        items.push(java_member_completion(alias, &m.name, &class_fqn, true));
                    }
                }
                for f in &info.fields {
                    if f.is_static && f.name.starts_with(name_prefix) {
                        items.push(java_member_completion(alias, &f.name, &class_fqn, false));
                    }
                }
            }
        }
    } else {
        // Pool A: current namespace symbols
        if let Some(fqns) = index.ns_symbols.get(current_ns) {
            for fqn in fqns.iter() {
                if let Some(sym) = index.symbols.get(fqn) {
                    if sym.name.starts_with(prefix) {
                        items.push(symbol_to_completion(&sym, None));
                    }
                }
            }
        }

        // Pool B: referred symbols
        if let Some(meta) = &ns_meta {
            for (refer_name, fqn) in &meta.refers {
                if refer_name.starts_with(prefix) {
                    if let Some(sym) = index.symbols.get(fqn) {
                        items.push(symbol_to_completion(&sym, None));
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
                if sf.name.starts_with(prefix) {
                    items.push(special_form_to_completion(sf));
                }
            }
            for &name in builtins::native_names() {
                if name.starts_with(prefix) {
                    if let Some(core) = index.core_symbols.iter().find(|c| c.name == name) {
                        items.push(letgo_native_to_completion(core));
                    }
                }
            }
            if let Some(fqns) = index.ns_symbols.get("core") {
                for fqn in fqns.iter() {
                    if let Some(sym) = index.symbols.get(fqn) {
                        if sym.name.starts_with(prefix) {
                            items.push(symbol_to_completion(&sym, None));
                        }
                    }
                }
            }
        } else {
            for core_sym in &index.core_symbols {
                if core_sym.name.starts_with(prefix) {
                    items.push(core_symbol_to_completion(core_sym));
                }
            }
            // Clojure special forms aren't clojure.core vars, so offer them too.
            for sf in builtins::special_forms(false) {
                if sf.name.starts_with(prefix) {
                    items.push(special_form_to_completion(sf));
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
                    if alias.starts_with(prefix) {
                        items.push(CompletionItem {
                            label: alias.clone(),
                            detail: Some(format!("alias for {}", full_ns)),
                            kind: Some(CompletionItemKind::MODULE),
                            ..Default::default()
                        });
                    }
                }
            }

            // Pool E: namespace names (project + libraries) — makes
            // completion inside (:require …) work. Library-internal
            // `.impl`/`.internal` namespaces are indexed for navigation but
            // omitted here to keep require completion clean.
            for entry in index.namespaces.iter() {
                let ns = entry.key();
                if ns.starts_with(prefix) && !is_internal_ns(ns) {
                    items.push(CompletionItem {
                        label: ns.clone(),
                        detail: Some("namespace".to_string()),
                        kind: Some(CompletionItemKind::MODULE),
                        ..Default::default()
                    });
                }
            }

            // Pool F: built-in Java class names. Gated on a PascalCase prefix so
            // ordinary (lowercase) completion isn't flooded with JDK classes, and
            // capped for short prefixes.
            if prefix.chars().next().is_some_and(|c| c.is_uppercase()) {
                if let Some(jdk) = index.jdk() {
                    for fqn in jdk
                        .class_names_with_prefix(prefix)
                        .into_iter()
                        .take(JAVA_CLASS_LIMIT)
                    {
                        items.push(java_class_completion(fqn));
                    }
                }
            }
        }
    }

    items
}

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

/// A let-go native core fn in completion — doc/arglists borrowed from the
/// clojure.core table, labelled native.
fn letgo_native_to_completion(sym: &CoreSymbol) -> CompletionItem {
    CompletionItem {
        label: sym.name.clone(),
        detail: Some(format!("let-go core (native) ({})", sym.params)),
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

fn defkind_to_completion_kind(kind: &DefKind) -> CompletionItemKind {
    match kind {
        DefKind::Defn | DefKind::DefnPrivate | DefKind::Defmacro => CompletionItemKind::FUNCTION,
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
