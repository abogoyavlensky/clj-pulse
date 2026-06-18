use anyhow::Result;
use tower_lsp::lsp_types::*;

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
            extractor::extract(&text, &extract_path)
                .map(|(_, syms)| syms)
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

/// Match tiers: exact (0) > prefix (1) > substring (2) > subsequence (3).
/// `None` means no match. An empty query matches everything.
fn match_score(name: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(3);
    }
    if name == query {
        Some(0)
    } else if name.starts_with(query) {
        Some(1)
    } else if name.contains(query) {
        Some(2)
    } else if is_subsequence(query, name) {
        Some(3)
    } else {
        None
    }
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

fn defkind_to_symbol_kind(kind: &DefKind) -> SymbolKind {
    match kind {
        DefKind::Defn | DefKind::DefnPrivate | DefKind::Defmacro | DefKind::Defmulti => {
            SymbolKind::FUNCTION
        }
        DefKind::Def | DefKind::Defonce => SymbolKind::VARIABLE,
        DefKind::Defprotocol => SymbolKind::INTERFACE,
        DefKind::Defrecord | DefKind::Deftype => SymbolKind::CLASS,
        DefKind::Defmethod => SymbolKind::METHOD,
        DefKind::IntegrantKey => SymbolKind::KEY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_score_tiers() {
        assert_eq!(match_score("add", "add"), Some(0));
        assert_eq!(match_score("add-and-double", "add"), Some(1));
        assert_eq!(match_score("re-add", "add"), Some(2));
        assert_eq!(match_score("a-d-d", "add"), Some(3));
        assert_eq!(match_score("multiply", "add"), None);
    }

    #[test]
    fn test_match_score_empty_query_matches_all() {
        assert_eq!(match_score("anything", ""), Some(3));
    }

    #[test]
    fn test_is_subsequence() {
        assert!(is_subsequence("aad", "add-and-double"));
        assert!(!is_subsequence("xyz", "add"));
    }
}
