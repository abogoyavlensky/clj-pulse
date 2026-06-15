use anyhow::Result;
use std::path::Path;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::Index;
use crate::uri;

use super::{resolve_symbol, ResolvedSymbol};

pub fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: GotoDefinitionParams,
) -> Result<Option<GotoDefinitionResponse>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let word = match documents.word_at(&uri, pos) {
        Some(w) => w,
        None => return Ok(None),
    };

    tracing::info!("goto_definition: word={}", word);

    // Works whether the open document is a project file, a directory-library
    // file (`file:` URIs), or a JAR entry (`jar:` URIs → virtual index path) —
    // the latter is what lets navigation continue into transitive deps.
    let path = match uri::to_index_path(&uri) {
        Some(p) => p,
        None => {
            tracing::debug!(
                "goto_definition: unresolvable document URI {}",
                uri.as_str()
            );
            return Ok(None);
        }
    };
    let current_ns = index.file_ns(&path).unwrap_or_default();
    tracing::debug!(
        "goto_definition: uri={} index_path={} current_ns={:?}",
        uri.as_str(),
        path.display(),
        current_ns
    );

    // Prefer the definition/occurrence resolved at this exact position when it
    // points at a known symbol. This is context-aware — a protocol method impl
    // resolves to the protocol's declaration even when the bare name also names
    // a core/current-ns var — and is computed from the live buffer (like
    // references/rename), so unsaved edits resolve correctly. When it doesn't
    // resolve to a known symbol, fall through to the bare-word resolver, which
    // also handles aliases, namespaces, and the static core list.
    let resolved = super::references::resolve_fqn_at(index, documents, &uri, pos);
    tracing::debug!("goto_definition: resolved={:?}", resolved);
    if let Some(fqn) = resolved {
        if let Some(sym) = index.lookup(&fqn) {
            let location = location_for(&sym.file, sym.name_range)?;
            return Ok(Some(GotoDefinitionResponse::Scalar(location)));
        }
    }

    match resolve_symbol(index, &word, &current_ns) {
        Some(ResolvedSymbol::Project(sym)) => {
            let location = location_for(&sym.file, sym.name_range)?;
            Ok(Some(GotoDefinitionResponse::Scalar(location)))
        }
        Some(ResolvedSymbol::Core(core)) => {
            // Aliases can shadow core names (`[clojure.string :as str]`);
            // navigate to the namespace only when the cursor is on the alias
            // declaration itself, not on a core-symbol usage in a body.
            if on_alias_declaration(documents, &uri, pos.line, &word) {
                return namespace_location(index, &current_ns, &word);
            }
            // Built-ins live in the clojure JAR like any other library
            // symbol; the static core list is only a doc shortcut.
            if let Some(sym) = index.lookup_in_ns("clojure.core", &core.name) {
                let location = location_for(&sym.file, sym.name_range)?;
                return Ok(Some(GotoDefinitionResponse::Scalar(location)));
            }
            Ok(None)
        }
        None => {
            // The word may be a require alias (`[ring.util.response :as
            // response]` with the cursor on `response`) or a namespace name
            // itself — navigate to the top of that namespace's file.
            namespace_location(index, &current_ns, &word)
        }
    }
}

/// True when `word` on this line is the alias being declared in a require
/// clause, i.e. the line contains `:as word`.
fn on_alias_declaration(documents: &DocumentStore, uri: &Url, line: u32, word: &str) -> bool {
    let Some(text) = documents.line_text(uri, line) else {
        return false;
    };
    text.split(":as")
        .skip(1)
        .any(|after| after.trim_start().starts_with(word))
}

/// Location at the top of the file defining `word`, where `word` is either a
/// require alias of `current_ns` or a namespace name itself.
fn namespace_location(
    index: &Index,
    current_ns: &str,
    word: &str,
) -> Result<Option<GotoDefinitionResponse>> {
    let target_ns = index
        .ns_meta(current_ns)
        .and_then(|m| m.aliases.get(word).cloned())
        .or_else(|| index.ns_meta(word).map(|_| word.to_string()));

    if let Some(ns) = target_ns {
        if let Some(meta) = index.ns_meta(&ns) {
            let location = location_for(&meta.file, Range::default())?;
            return Ok(Some(GotoDefinitionResponse::Scalar(location)));
        }
    }
    Ok(None)
}

/// Builds an LSP Location for a symbol file. The URI scheme follows the path
/// shape: plain `file:` URIs for real paths (project sources and directory
/// libs), `jar:` URIs for virtual `jar_path!/entry` paths inside JARs.
fn location_for(file: &Path, range: Range) -> Result<Location> {
    let uri = uri::from_index_path(file)?;
    Ok(Location { uri, range })
}
