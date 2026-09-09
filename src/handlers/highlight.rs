//! `textDocument/documentHighlight`: every occurrence of the symbol under the
//! cursor *in this buffer*, so the editor can underline them as the cursor
//! moves. Unlike find-references it never leaves the document, and it answers
//! from the live buffer alone — a keystroke must not wait for a reindex.

use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::handlers::references;
use crate::index::{extractor, DefKind, Index};

pub fn document_highlight(
    index: &Index,
    documents: &DocumentStore,
    params: DocumentHighlightParams,
) -> Result<Option<Vec<DocumentHighlight>>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    // Locals shadow vars and are never recorded as occurrences, so resolve
    // them structurally first. As in `references`, this branch is
    // authoritative: a param shadowing a global highlights only itself.
    if let Some(highlights) = local_highlights(documents, &uri, pos) {
        return Ok((!highlights.is_empty()).then_some(highlights));
    }

    let Some(fqn) = references::resolve_fqn_at(index, documents, &uri, pos) else {
        return Ok(None);
    };
    let Some(path) = crate::uri::to_index_path(&uri) else {
        return Ok(None);
    };
    let Some(snapshot) = documents.snapshot(&uri) else {
        return Ok(None);
    };
    let cfg = index.extract_config();

    // A keyword has no read/write distinction — `:id` is the same token
    // wherever it appears — so its whole chain is TEXT.
    let usage_kind = if fqn.starts_with(':') {
        DocumentHighlightKind::TEXT
    } else {
        DocumentHighlightKind::READ
    };

    let mut highlights = Vec::new();
    if crate::config::is_edn(&path) {
        for occ in extractor::file_occurrences_tree(&snapshot.tree, &snapshot.text, &path, &cfg) {
            if occ.fqn == fqn {
                highlights.push(highlight(occ.name_range, usage_kind));
            }
        }
    } else if let Ok((_, syms, occs)) =
        extractor::extract_full_tree(&snapshot.tree, &snapshot.text, &path, &cfg)
    {
        for occ in &occs {
            if occ.fqn == fqn {
                highlights.push(highlight(occ.name_range, usage_kind));
            }
        }
        for sym in &syms {
            // A `defmethod` head names the multimethod it extends rather than
            // defining anything, exactly as `resolve_fqn_at` treats it.
            if sym.kind != DefKind::Defmethod && sym.fqn == fqn {
                let kind = if usage_kind == DocumentHighlightKind::TEXT {
                    DocumentHighlightKind::TEXT
                } else {
                    DocumentHighlightKind::WRITE
                };
                highlights.push(highlight(sym.name_range, kind));
            }
        }
    }

    // Document order, with a definition ahead of any usage that happens to
    // share its span: the Integrant `ig/init-key` dispatch keyword is both a
    // symbol and an occurrence, so the same range can arrive twice and the
    // WRITE is the one worth keeping.
    highlights.sort_by_key(|h| {
        let write = h.kind == Some(DocumentHighlightKind::WRITE);
        (
            h.range.start.line,
            h.range.start.character,
            u8::from(!write),
        )
    });
    highlights.dedup_by_key(|h| h.range);

    Ok((!highlights.is_empty()).then_some(highlights))
}

/// The local under the cursor as highlights: its binding site is a WRITE, every
/// usage a READ. `None` when the cursor is not on a local, so the caller falls
/// through to the fqn path.
fn local_highlights(
    documents: &DocumentStore,
    uri: &Url,
    pos: Position,
) -> Option<Vec<DocumentHighlight>> {
    let (_, refs) = references::local_refs_at(documents, uri, pos)?;
    let mut highlights = vec![highlight(refs.declaration, DocumentHighlightKind::WRITE)];
    for range in refs.usages {
        highlights.push(highlight(range, DocumentHighlightKind::READ));
    }
    Some(highlights)
}

fn highlight(range: Range, kind: DocumentHighlightKind) -> DocumentHighlight {
    DocumentHighlight {
        range,
        kind: Some(kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store holding one open `file:///test.clj`, plus an empty index: every
    /// resolution these tests exercise runs off the buffer's own parse.
    fn setup(text: &str) -> (Index, DocumentStore, Url) {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.clj").unwrap();
        store.open(uri.clone(), text.to_string());
        (Index::new(), store, uri)
    }

    fn highlights_at(text: &str, line: u32, character: u32) -> Vec<(String, Range)> {
        let (index, store, uri) = setup(text);
        let params = DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        document_highlight(&index, &store, params)
            .unwrap()
            .unwrap_or_default()
            .into_iter()
            .map(|h| {
                let kind = if h.kind == Some(DocumentHighlightKind::WRITE) {
                    "write"
                } else if h.kind == Some(DocumentHighlightKind::READ) {
                    "read"
                } else if h.kind == Some(DocumentHighlightKind::TEXT) {
                    "text"
                } else {
                    "none"
                };
                (kind.to_string(), h.range)
            })
            .collect()
    }

    /// The text `range` covers, so a test can assert the exact span.
    fn slice(text: &str, range: Range) -> String {
        let line = text.lines().nth(range.start.line as usize).unwrap();
        line[range.start.character as usize..range.end.character as usize].to_string()
    }

    #[test]
    fn local_binding_is_write_and_usages_are_read() {
        let text = "(ns app)\n(defn f [x]\n  (+ x x))";
        // Cursor on the second `x` usage.
        let got = highlights_at(text, 2, 7);
        let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, vec!["write", "read", "read"], "{:?}", got);
        assert_eq!(
            got[0].1.start,
            Position::new(1, 9),
            "binding site: {:?}",
            got
        );
    }

    #[test]
    fn var_defined_in_this_file_is_write_plus_reads() {
        let text = "(ns app)\n(defn add [a b] (+ a b))\n(defn g [] (add 1 2))";
        let got = highlights_at(text, 2, 12);
        let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, vec!["write", "read"], "{:?}", got);
        assert_eq!(slice(text, got[0].1), "add");
        assert_eq!(slice(text, got[1].1), "add");
    }

    #[test]
    fn var_defined_elsewhere_is_reads_only() {
        let text = "(ns app (:require [other :as o]))\n(defn g [] (o/add 1) (o/add 2))";
        let got = highlights_at(text, 1, 14);
        let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, vec!["read", "read"], "{:?}", got);
        // Qualified usages highlight the name half, the same span rename edits.
        assert_eq!(slice(text, got[0].1), "add");
    }

    #[test]
    fn keyword_occurrences_are_text_over_the_whole_token() {
        let text = "(ns app)\n(def m {::k 1})\n(defn g [] (::k m))";
        let got = highlights_at(text, 1, 10);
        let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, vec!["text", "text"], "{:?}", got);
        assert_eq!(slice(text, got[0].1), "::k");
        assert_eq!(slice(text, got[1].1), "::k");
    }

    #[test]
    fn nothing_under_the_cursor_returns_none() {
        let (index, store, uri) = setup("(ns app)\n\n(defn f [] 1)");
        let params = DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position::new(1, 0),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        assert!(document_highlight(&index, &store, params)
            .unwrap()
            .is_none());
    }
}
