use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::{Index, SymbolSource};

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

    let path = uri
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("invalid file URI"))?;
    let current_ns = index.file_ns(&path).unwrap_or_default();

    match resolve_symbol(index, &word, &current_ns) {
        Some(ResolvedSymbol::Project(sym)) => match sym.source {
            SymbolSource::Project => {
                let target_uri = Url::from_file_path(&sym.file)
                    .map_err(|_| anyhow::anyhow!("invalid path: {:?}", sym.file))?;
                Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri: target_uri,
                    range: sym.name_range,
                })))
            }
            SymbolSource::Jar(_) => Ok(None),
        },
        Some(ResolvedSymbol::Core(_)) => Ok(None),
        None => Ok(None),
    }
}
