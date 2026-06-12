use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::{DefKind, Index};

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

        // Pool C: clojure.core builtins
        for core_sym in &index.core_symbols {
            if core_sym.name.starts_with(prefix) {
                items.push(core_symbol_to_completion(core_sym));
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
            // completion inside (:require …) work
            for entry in index.namespaces.iter() {
                if entry.key().starts_with(prefix) {
                    items.push(CompletionItem {
                        label: entry.key().clone(),
                        detail: Some("namespace".to_string()),
                        kind: Some(CompletionItemKind::MODULE),
                        ..Default::default()
                    });
                }
            }
        }
    }

    items
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
