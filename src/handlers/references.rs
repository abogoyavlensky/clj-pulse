use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::{extractor, DefKind, Index, Occurrence, SymbolSource};

pub fn references(
    index: &Index,
    documents: &DocumentStore,
    params: ReferenceParams,
) -> Result<Option<Vec<Location>>> {
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;

    let Some(fqn) = resolve_fqn_at(index, documents, &uri, pos) else {
        return Ok(None);
    };

    let mut locations = Vec::new();
    if params.context.include_declaration {
        if let Some(sym) = index.lookup(&fqn) {
            // Declarations in any source are listed: project/dir files as
            // `file:` URIs, JAR entries as `jar:` URIs.
            if let Ok(decl_uri) = crate::uri::from_index_path(&sym.file) {
                locations.push(Location {
                    uri: decl_uri,
                    range: sym.name_range,
                });
            }
        }
    }

    for (file, occs) in occurrences_for(index, documents, &fqn) {
        let Ok(file_uri) = crate::uri::from_index_path(&file) else {
            continue;
        };
        for occ in occs {
            locations.push(Location {
                uri: file_uri.clone(),
                range: occ.name_range,
            });
        }
    }

    if locations.is_empty() {
        Ok(None)
    } else {
        Ok(Some(locations))
    }
}

pub fn rename(
    index: &Index,
    documents: &DocumentStore,
    params: RenameParams,
) -> Result<Option<WorkspaceEdit>> {
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    let new_name = params.new_name;

    if !is_valid_symbol_name(&new_name) {
        anyhow::bail!("cannot rename: '{}' is not a valid symbol name", new_name);
    }

    // Rename may only be initiated from an editable project file. Library
    // buffers (jar: entries, dir-dep file:s) are read-only — and since the
    // resolver is fqn-only, a rename started there could otherwise edit a
    // project symbol that shadows the library one.
    let origin = crate::uri::to_index_path(&uri)
        .ok_or_else(|| anyhow::anyhow!("cannot rename from this document"))?;
    if !index.is_project_path(&origin) {
        anyhow::bail!("cannot rename from a library file");
    }

    let fqn = resolve_fqn_at(index, documents, &uri, pos)
        .ok_or_else(|| anyhow::anyhow!("nothing to rename here"))?;
    // Keyword fqns are colon-prefixed. Keyword occurrences span the whole
    // token, so renaming through this path would rewrite the entire keyword;
    // keyword rename isn't supported yet, so reject it rather than corrupt.
    if fqn.starts_with(':') {
        anyhow::bail!("renaming keywords is not yet supported");
    }
    let sym = index
        .lookup(&fqn)
        .ok_or_else(|| anyhow::anyhow!("cannot rename: no definition found for {}", fqn))?;
    if sym.source != SymbolSource::Project {
        anyhow::bail!("cannot rename library or built-in symbol {}", fqn);
    }

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

    // Declaration edit — from live text when the defining file is open
    // (its indexed range may be stale against unsaved edits).
    let decl_uri = Url::from_file_path(&sym.file)
        .map_err(|_| anyhow::anyhow!("invalid path: {:?}", sym.file))?;
    let decl_range = documents
        .text(&decl_uri)
        .and_then(|text| {
            extractor::extract_full_with(&text, &sym.file, &index.extract_config())
                .ok()
                .and_then(|(_, syms, _)| {
                    syms.into_iter()
                        .find(|s| s.fqn == fqn)
                        .map(|s| s.name_range)
                })
        })
        .unwrap_or(sym.name_range);
    changes.entry(decl_uri).or_default().push(TextEdit {
        range: decl_range,
        new_text: new_name.clone(),
    });

    for (file, occs) in occurrences_for(index, documents, &fqn) {
        let Ok(file_uri) = Url::from_file_path(&file) else {
            continue;
        };
        let edits = changes.entry(file_uri).or_default();
        for occ in occs {
            edits.push(TextEdit {
                range: occ.name_range,
                new_text: new_name.clone(),
            });
        }
    }

    Ok(Some(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    }))
}

fn is_valid_symbol_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| crate::document::is_clj_ident_char(c) && c != '/')
}

/// Resolves the symbol under the cursor to an fqn:
///
/// 1. Cursor on a definition name in this file → that definition's fqn.
/// 2. Cursor on a recorded occurrence → that occurrence's fqn. Locals are
///    never occurrences, so a `(defn f [add] add)` param cannot leak the
///    global `add` into references/rename.
/// 3. Qualified words only (locals are never qualified): resolve through
///    the alias — covers a cursor on the alias half of `lib/name`.
pub fn resolve_fqn_at(
    index: &Index,
    documents: &DocumentStore,
    uri: &Url,
    pos: Position,
) -> Option<String> {
    let path = crate::uri::to_index_path(uri)?;
    let current_ns = index.file_ns(&path).unwrap_or_default();

    // Resolve against live text so unsaved edits use current ranges. Position
    // matching (below) runs without a word token, so a cursor on a keyword's
    // `:`/`::` marker still resolves — the occurrence/definition range spans it.
    let text = documents.text(uri)?;

    // EDN config files (Integrant systems) have no symbols or aliases; match
    // the cursor against keyword occurrences only. `file_occurrences` applies
    // the `#ig/ref` gate, so a cursor in a non-Integrant manifest resolves to
    // nothing.
    if crate::config::is_edn(&path) {
        return extractor::file_occurrences_with(&text, &path, &index.extract_config())
            .into_iter()
            .find(|occ| range_contains(&occ.name_range, pos))
            .map(|occ| occ.fqn);
    }

    if let Ok((_, syms, occs)) = extractor::extract_full_with(&text, &path, &index.extract_config())
    {
        for sym in &syms {
            // A `defmethod` head names the multimethod it extends, not a new
            // definition — its symbol points at itself. Skip it so the
            // multimethod occurrence below (resolved to the `defmulti`) wins,
            // letting goto-def/references/rename target the multimethod.
            if sym.kind == DefKind::Defmethod {
                continue;
            }
            if range_contains(&sym.name_range, pos) {
                return Some(sym.fqn.clone());
            }
        }
        for occ in &occs {
            if range_contains(&occ.name_range, pos) {
                return Some(occ.fqn.clone());
            }
        }
    }

    // Not a definition or occurrence — the only legitimate remaining case
    // is the cursor on the alias half of a qualified usage. Bare words here
    // are locals or noise; resolving them would risk corrupting renames.
    let word = documents.word_at(uri, pos)?;
    let (alias, name) = word.split_once('/')?;
    if alias.is_empty() || name.is_empty() {
        return None;
    }
    let ns = index
        .ns_meta(&current_ns)
        .and_then(|m| m.aliases.get(alias).cloned())
        .unwrap_or_else(|| alias.to_string());
    Some(format!("{}/{}", ns, name))
}

fn range_contains(range: &Range, pos: Position) -> bool {
    (range.start.line < pos.line
        || (range.start.line == pos.line && range.start.character <= pos.character))
        && (pos.line < range.end.line
            || (pos.line == range.end.line && pos.character <= range.end.character))
}

/// All occurrences of `fqn`, per file. Files currently open in the editor
/// are re-extracted from live text so unsaved edits produce correct ranges;
/// everything else comes from the index.
pub fn occurrences_for(
    index: &Index,
    documents: &DocumentStore,
    fqn: &str,
) -> Vec<(PathBuf, Vec<Occurrence>)> {
    let mut live: HashMap<PathBuf, Vec<Occurrence>> = HashMap::new();
    for uri in documents.open_uris() {
        // Open JAR docs (`jar:` URIs) convert to their virtual index path, so a
        // library file the user is viewing contributes its live occurrences.
        let Some(path) = crate::uri::to_index_path(&uri) else {
            continue;
        };
        let Some(text) = documents.text(&uri) else {
            continue;
        };
        let occs = extractor::file_occurrences_with(&text, &path, &index.extract_config());
        live.insert(path, occs);
    }

    let mut result = Vec::new();
    for entry in index.occurrences.iter() {
        if live.contains_key(entry.key()) {
            continue;
        }
        let matching: Vec<Occurrence> = entry
            .value()
            .iter()
            .filter(|o| o.fqn == fqn)
            .cloned()
            .collect();
        if !matching.is_empty() {
            result.push((entry.key().clone(), matching));
        }
    }
    for (path, occs) in live {
        let matching: Vec<Occurrence> = occs.into_iter().filter(|o| o.fqn == fqn).collect();
        if !matching.is_empty() {
            result.push((path, matching));
        }
    }
    result
}
