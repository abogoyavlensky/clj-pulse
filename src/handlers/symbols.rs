use anyhow::Result;
use tower_lsp::lsp_types::*;

use super::matching::match_score;
use crate::document::DocumentStore;
use crate::index::{extractor, DefKind, Index, Symbol, SymbolSource};

/// Outline for a single file. Prefers the live (possibly unsaved) document
/// text over the index so the outline tracks edits; extraction of one file
/// costs ~1ms.
pub fn document_symbols(
    index: &Index,
    documents: &DocumentStore,
    params: DocumentSymbolParams,
) -> Result<Option<DocumentSymbolResponse>> {
    let uri = params.text_document.uri;
    // Non-file documents (jar: virtual sources, untitled: buffers) have no
    // index entry but can still be outlined from their open text.
    let path = uri.to_file_path().ok();

    let symbols: Vec<Symbol> = match documents.text(&uri) {
        Some(text) => {
            let extract_path = path
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from(uri.path()));
            extractor::extract_full_with(&text, &extract_path, &index.extract_config())
                .map(|(_, syms, _)| syms)
                .unwrap_or_default()
        }
        None => path
            .map(|path| {
                index
                    .file_ns(&path)
                    .and_then(|ns| index.ns_symbols.get(&ns).map(|fqns| fqns.clone()))
                    .map(|fqns| {
                        fqns.iter()
                            .filter_map(|fqn| index.lookup(fqn))
                            .filter(|s| s.file == path)
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default(),
    };

    if symbols.is_empty() {
        return Ok(None);
    }

    #[allow(deprecated)]
    let doc_symbols: Vec<DocumentSymbol> = symbols
        .into_iter()
        .map(|s| DocumentSymbol {
            name: s.name,
            detail: if s.params.is_empty() {
                None
            } else {
                Some(s.params.join(" "))
            },
            kind: defkind_to_symbol_kind(&s.kind),
            tags: None,
            deprecated: None,
            range: s.range,
            selection_range: s.name_range,
            children: None,
        })
        .collect();

    Ok(Some(DocumentSymbolResponse::Nested(doc_symbols)))
}

/// Project-wide symbol search (Cmd+T). Project symbols only — library
/// symbols are reachable via completion/definition and would flood results.
pub fn workspace_symbols(index: &Index, query: &str) -> Vec<SymbolInformation> {
    const MAX_RESULTS: usize = 128;

    let query = query.to_lowercase();
    let mut matches: Vec<(u8, Symbol)> = index
        .symbols
        .iter()
        .filter(|entry| entry.value().source == SymbolSource::Project)
        .filter_map(|entry| {
            let sym = entry.value();
            let score = match_score(&sym.name.to_lowercase(), &query)?;
            Some((score, sym.clone()))
        })
        .collect();

    matches.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.name.len().cmp(&b.1.name.len()))
            .then_with(|| a.1.fqn.cmp(&b.1.fqn))
    });
    matches.truncate(MAX_RESULTS);

    matches
        .into_iter()
        .filter_map(|(_, sym)| {
            let uri = Url::from_file_path(&sym.file).ok()?;
            #[allow(deprecated)]
            Some(SymbolInformation {
                name: sym.name,
                kind: defkind_to_symbol_kind(&sym.kind),
                tags: None,
                deprecated: None,
                location: Location {
                    uri,
                    range: sym.name_range,
                },
                container_name: Some(sym.ns),
            })
        })
        .collect()
}

fn defkind_to_symbol_kind(kind: &DefKind) -> SymbolKind {
    match kind {
        DefKind::Defn | DefKind::DefnPrivate | DefKind::Defmacro | DefKind::Defmulti => {
            SymbolKind::FUNCTION
        }
        DefKind::Def | DefKind::Defonce | DefKind::Declare => SymbolKind::VARIABLE,
        DefKind::Defprotocol => SymbolKind::INTERFACE,
        DefKind::Defrecord | DefKind::Deftype => SymbolKind::CLASS,
        DefKind::Defmethod => SymbolKind::METHOD,
        // A deftest defines a fn var; the editor's outline shows it as one.
        DefKind::Deftest => SymbolKind::FUNCTION,
        DefKind::IntegrantKey => SymbolKind::KEY,
    }
}
