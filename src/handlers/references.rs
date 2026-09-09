use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::{DocumentStore, Snapshot};
use crate::index::{extractor, DefKind, Index, Occurrence, Symbol, SymbolSource};

pub fn references(
    index: &Index,
    documents: &DocumentStore,
    params: ReferenceParams,
) -> Result<Option<Vec<Location>>> {
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;

    // Local bindings (let/fn/loop/…) shadow vars and are never recorded as
    // occurrences, so resolve their usages structurally, before the fqn path.
    // When the cursor is on a local, this is authoritative — don't fall through.
    if let Some(locations) =
        local_references(documents, &uri, pos, params.context.include_declaration)
    {
        return Ok((!locations.is_empty()).then_some(locations));
    }

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

/// Find-references for a local binding: the declaration (when requested) plus
/// every in-scope usage, all in the same document. Returns `None` when the
/// cursor is not on a local (caller falls back to fqn-based references) and
/// `Some` — possibly empty — when it is, so a local never leaks into the fqn
/// path. Mirrors `local_definition`'s keyword/qualified guards.
fn local_references(
    documents: &DocumentStore,
    uri: &Url,
    pos: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let (_, refs) = local_refs_at(documents, uri, pos)?;

    let mut locations = Vec::new();
    if include_declaration {
        locations.push(Location {
            uri: uri.clone(),
            range: refs.declaration,
        });
    }
    for range in refs.usages {
        locations.push(Location {
            uri: uri.clone(),
            range,
        });
    }
    Some(locations)
}

/// The local under the cursor, as `(word, refs)`. `None` when the cursor is on
/// a keyword, a qualified word (locals are never qualified), or a word that
/// resolves to no local — the callers then fall back to the fqn path. Shared by
/// find-references, rename and document highlight so they all agree on what
/// counts as a local.
pub(crate) fn local_refs_at(
    documents: &DocumentStore,
    uri: &Url,
    pos: Position,
) -> Option<(String, extractor::LocalRefs)> {
    if documents.is_keyword_at(uri, pos) {
        return None;
    }
    let word = documents.word_at(uri, pos)?;
    if word.contains('/') {
        return None;
    }
    let snapshot = documents.snapshot(uri)?;
    let refs = extractor::local_references_at_tree(&snapshot.tree, &snapshot.text, pos, &word)?;
    Some((word, refs))
}

/// Refuses a local rename whose new name is already bound at the declaration
/// or at any of its usages. Renaming `x` to `y` in `(let [x 1 y 2] (+ x y))`
/// would otherwise produce `(let [y 1 y 2] (+ y y))`: valid code that quietly
/// means something else. A new name that merely shadows a *var* is allowed —
/// that is ordinary Clojure, and the local wins by design.
fn reject_local_capture(
    snapshot: &Snapshot,
    refs: &extractor::LocalRefs,
    word: &str,
    new_name: &str,
) -> Result<()> {
    let sites = std::iter::once(refs.declaration).chain(refs.usages.iter().copied());
    for site in sites {
        let taken = extractor::locals_in_scope_at_tree(&snapshot.tree, &snapshot.text, site.start)
            .into_iter()
            .any(|b| b.name == new_name);
        if taken {
            anyhow::bail!(
                "cannot rename '{}' to '{}': '{}' is already bound in that scope",
                word,
                new_name,
                new_name
            );
        }
    }
    Ok(())
}

/// What a rename at a position would target, once every rejection that does not
/// depend on the new name has been made.
pub enum RenameTarget {
    /// A local binding: its declaration and usages, all in the same document.
    Local {
        word: String,
        refs: extractor::LocalRefs,
    },
    /// A project-wide var, record, keyword-free symbol — renamed by fqn.
    Global { fqn: String, sym: Symbol },
    /// A qualified project keyword: every site that reads it, already checked
    /// down to the range of the name its notation ends with.
    Keyword { sites: Vec<KeywordSite> },
}

/// One site a keyword rename rewrites: the file, the whole token (which the
/// cursor may sit anywhere in, `::` marker included) and the sub-range of it
/// the edit replaces.
pub struct KeywordSite {
    pub uri: Url,
    pub token: Range,
    pub name: Range,
}

/// Everything [`rename`] checks before building edits, minus the checks that
/// need the new name. [`prepare_rename`] runs exactly this, so the editor's
/// rename box appears only where a rename would actually succeed.
pub fn rename_target(
    index: &Index,
    documents: &DocumentStore,
    uri: &Url,
    pos: Position,
) -> Result<RenameTarget> {
    // Rename may only be initiated from an editable project file. Library
    // buffers (jar: entries, dir-dep file:s) are read-only — and since the
    // resolver is fqn-only, a rename started there could otherwise edit a
    // project symbol that shadows the library one.
    let origin = crate::uri::to_index_path(uri)
        .ok_or_else(|| anyhow::anyhow!("cannot rename from this document"))?;
    if !index.is_project_path(&origin) {
        anyhow::bail!("cannot rename from a library file");
    }

    // Locals (let/fn/defn params, destructuring, …) are resolved structurally
    // and never reach the fqn path, so renaming a param that shadows a global
    // edits only the local's own binding and usages, all in this document.
    if let Some((word, refs)) = local_refs_at(documents, uri, pos) {
        if refs.destructured_key {
            anyhow::bail!(
                "cannot rename a :keys/:strs/:syms destructured binding '{}': \
                 rewrite it as {{new-name :{}}} first",
                word,
                word
            );
        }
        return Ok(RenameTarget::Local { word, refs });
    }

    let fqn = resolve_fqn_at(index, documents, uri, pos)
        .ok_or_else(|| anyhow::anyhow!("nothing to rename here"))?;
    // Keyword fqns are colon-prefixed. A keyword occurrence spans the whole
    // token, so it takes its own path: the edit replaces the name the notation
    // ends with, never the token.
    if fqn.starts_with(':') {
        return keyword_target(index, documents, fqn);
    }
    let sym = index
        .lookup(&fqn)
        .ok_or_else(|| anyhow::anyhow!("cannot rename: no definition found for {}", fqn))?;
    if sym.source != SymbolSource::Project {
        anyhow::bail!("cannot rename library or built-in symbol {}", fqn);
    }
    Ok(RenameTarget::Global { fqn, sym })
}

/// Everything a keyword rename can be refused for before the new name is
/// known, plus the sites it would rewrite. Kept in [`rename_target`] so
/// `prepareRename` refuses exactly what `rename` would.
fn keyword_target(index: &Index, documents: &DocumentStore, fqn: String) -> Result<RenameTarget> {
    let Some((ns, name)) = fqn.trim_start_matches(':').split_once('/') else {
        anyhow::bail!(
            "cannot rename the unqualified keyword {}: the same name in unrelated maps is not one thing",
            fqn
        );
    };
    let (ns, name) = (ns.to_string(), name.to_string());
    if index.is_library_namespace(&ns) {
        anyhow::bail!("cannot rename a keyword of library namespace {}", ns);
    }

    // A definition site (an `ig/init-key` dispatch keyword, an `s/def` name) is
    // a symbol rather than an occurrence, so it needs a pass of its own: live
    // from every open project buffer, since one just typed has no indexed
    // symbol and unsaved edits move the indexed one, plus the index for the
    // file that holds it when it is not open. Missing it would edit every other
    // site and, when the cursor sits on it, make `prepareRename` refuse what
    // `rename` would accept.
    let mut candidates: Vec<(PathBuf, Range)> = Vec::new();
    let mut open_files: Vec<PathBuf> = Vec::new();
    for uri in documents.open_uris() {
        let Some(path) = crate::uri::to_index_path(&uri) else {
            continue;
        };
        open_files.push(path.clone());
        // EDN configs hold occurrences only; extracting them as Clojure would
        // read a system map as code.
        if crate::config::is_edn(&path) {
            continue;
        }
        let Some(snapshot) = documents.snapshot(&uri) else {
            continue;
        };
        let Ok((_, syms, _)) = extractor::extract_full_tree(
            &snapshot.tree,
            &snapshot.text,
            &path,
            &index.extract_config(),
        ) else {
            continue;
        };
        candidates.extend(
            syms.into_iter()
                .filter(|sym| sym.fqn == fqn)
                .map(|sym| (path.clone(), sym.name_range)),
        );
    }
    if let Some(sym) = index.lookup(&fqn) {
        if !open_files.contains(&sym.file) {
            candidates.push((sym.file, sym.name_range));
        }
    }
    for (file, occs) in occurrences_for(index, documents, &fqn) {
        candidates.extend(occs.into_iter().map(|occ| (file.clone(), occ.name_range)));
    }

    let mut sites: Vec<KeywordSite> = Vec::new();
    for (path, token) in candidates {
        // Library files are read-only even when the user has one open, and an
        // open `jar:` buffer contributes occurrences like any other file.
        if !index.is_project_path(&path) {
            continue;
        }
        let Ok(uri) = Url::from_file_path(&path) else {
            continue;
        };
        // Unsaved edits move the ranges `occurrences_for` reports, so the token
        // is read from the same text those ranges came from.
        let text = documents
            .text(&uri)
            .or_else(|| std::fs::read_to_string(&path).ok())
            .ok_or_else(|| {
                anyhow::anyhow!("cannot rename {}: {} is unreadable", fqn, path.display())
            })?;
        let text = token_at(&text, token).unwrap_or_default();
        let Some(name_range) = name_suffix_range(token, &text, &name) else {
            // All-or-nothing: rewriting the sites that do conform would leave
            // this one reading the old key, which is worse than refusing.
            if !text.starts_with(':') {
                anyhow::bail!(
                    "cannot rename {}: the {{:keys [{}]}} destructuring at {}:{} would keep \
                     reading the old key; rewrite it as {{{} {}}} first",
                    fqn,
                    text,
                    path.display(),
                    token.start.line + 1,
                    name,
                    fqn
                );
            }
            anyhow::bail!(
                "cannot rename {}: '{}' at {}:{} is not a keyword ending in '{}'",
                fqn,
                text,
                path.display(),
                token.start.line + 1,
                name
            );
        };
        if sites
            .iter()
            .any(|site| site.uri == uri && site.name == name_range)
        {
            continue;
        }
        sites.push(KeywordSite {
            uri,
            token,
            name: name_range,
        });
    }
    if sites.is_empty() {
        anyhow::bail!("cannot rename {}: no project occurrence to rewrite", fqn);
    }
    Ok(RenameTarget::Keyword { sites })
}

/// A definition's name range, taken from the live buffer when its file is open
/// — the indexed range may be stale against unsaved edits.
fn live_definition_range(
    index: &Index,
    documents: &DocumentStore,
    sym: &Symbol,
    fqn: &str,
) -> Range {
    let Ok(uri) = Url::from_file_path(&sym.file) else {
        return sym.name_range;
    };
    documents
        .snapshot(&uri)
        .and_then(|snapshot| {
            extractor::extract_full_tree(
                &snapshot.tree,
                &snapshot.text,
                &sym.file,
                &index.extract_config(),
            )
            .ok()
        })
        .and_then(|(_, syms, _)| syms.into_iter().find(|s| s.fqn == fqn))
        .map(|s| s.name_range)
        .unwrap_or(sym.name_range)
}

/// The text `range` covers. Keyword tokens never span lines, so a multi-line
/// range is not one; columns are UTF-16 units, what LSP ranges count in.
fn token_at(text: &str, range: Range) -> Option<String> {
    if range.start.line != range.end.line {
        return None;
    }
    let line = text.lines().nth(range.start.line as usize)?;
    let units: Vec<u16> = line.encode_utf16().collect();
    let (from, to) = (range.start.character as usize, range.end.character as usize);
    if from > to || to > units.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&units[from..to]))
}

/// The sub-range of a keyword token covering just the trailing `name`. Every
/// notation ends with the name — `::name`, `::alias/name`, `:ns/name` — so
/// rewriting that suffix renames the keyword whatever notation the site uses.
/// `None` when the token does not end with `name` behind its separator: a
/// `{::keys [name]}` entry is a bare symbol, and any other shape this cannot
/// rewrite is refused rather than corrupted.
fn name_suffix_range(range: Range, token: &str, name: &str) -> Option<Range> {
    let head = token.strip_suffix(name)?;
    // A keyword token always opens with its colon. A destructuring entry is a
    // bare symbol (`id`, `app/id`) that *reads* the keyword while binding a
    // local of the same name, so rewriting its suffix would rename the binding
    // and leave every usage of it behind.
    if !head.starts_with(':') || (!head.ends_with('/') && !head.ends_with(':')) {
        return None;
    }
    let len = name.encode_utf16().count() as u32;
    Some(Range {
        start: Position {
            line: range.end.line,
            character: range.end.character.checked_sub(len)?,
        },
        end: range.end,
    })
}

/// The range of the token under the cursor, for `textDocument/prepareRename` —
/// the very range a rename would rewrite, so the editor pre-selects exactly
/// what changes. Rejections carry the same messages as [`rename`].
pub fn prepare_rename(
    index: &Index,
    documents: &DocumentStore,
    uri: &Url,
    pos: Position,
) -> Result<PrepareRenameResponse> {
    let range = match rename_target(index, documents, uri, pos)? {
        RenameTarget::Local { refs, .. } => std::iter::once(refs.declaration)
            .chain(refs.usages.iter().copied())
            .find(|r| range_contains(r, pos))
            .unwrap_or(refs.declaration),
        RenameTarget::Global { fqn, .. } => {
            occurrence_range_at(index, documents, uri, pos, &fqn)
                .ok_or_else(|| anyhow::anyhow!("nothing to rename here"))?
        }
        // The cursor may sit anywhere in the token, `::` marker included, but
        // the range the editor pre-selects is the name the edit replaces.
        RenameTarget::Keyword { sites } => sites
            .iter()
            .find(|site| site.uri == *uri && range_contains(&site.token, pos))
            .map(|site| site.name)
            .ok_or_else(|| anyhow::anyhow!("nothing to rename here"))?,
    };
    Ok(PrepareRenameResponse::Range(range))
}

/// The `name_range` of the definition or occurrence of `fqn` under `pos` in
/// *this* document — the exact span a rename edit would replace. A range from
/// the defining file would be meaningless to the editor, so a cursor that
/// resolves to nothing here yields `None` rather than a foreign range.
fn occurrence_range_at(
    index: &Index,
    documents: &DocumentStore,
    uri: &Url,
    pos: Position,
    fqn: &str,
) -> Option<Range> {
    let path = crate::uri::to_index_path(uri)?;
    let snapshot = documents.snapshot(uri)?;
    let (_, syms, occs) = extractor::extract_full_tree(
        &snapshot.tree,
        &snapshot.text,
        &path,
        &index.extract_config(),
    )
    .ok()?;
    let line = snapshot
        .text
        .lines()
        .nth(pos.line as usize)
        .unwrap_or_default();
    syms.iter()
        .filter(|s| s.fqn == fqn)
        .map(|s| s.name_range)
        .chain(occs.iter().filter(|o| o.fqn == fqn).map(|o| o.name_range))
        .find(|r| range_contains(r, pos) || on_qualifier_of(line, pos, *r))
}

/// Whether `pos` sits on the alias half of a qualified usage whose name half is
/// `name`: everything from the cursor up to the name must be identifier text
/// closed by the `/` separator. A cursor on the `h` of `h/greet` renames
/// `greet`, so prepareRename must report `greet`'s range.
fn on_qualifier_of(line: &str, pos: Position, name: Range) -> bool {
    if pos.line != name.start.line || pos.character >= name.start.character {
        return false;
    }
    let units: Vec<u16> = line.encode_utf16().collect();
    let (from, to) = (pos.character as usize, name.start.character as usize);
    if to == 0 || to > units.len() || units[to - 1] != u16::from(b'/') {
        return false;
    }
    String::from_utf16_lossy(&units[from..to - 1])
        .chars()
        .all(crate::document::is_clj_ident_char)
}

pub fn rename(
    index: &Index,
    documents: &DocumentStore,
    params: RenameParams,
) -> Result<Option<WorkspaceEdit>> {
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    let new_name = params.new_name;

    let target = rename_target(index, documents, &uri, pos)?;

    // A keyword rename takes the bare name — the notation at each site supplies
    // the colons — so say that rather than let the general rule below report
    // `:store` as an invalid symbol name.
    if matches!(target, RenameTarget::Keyword { .. }) {
        if let Some(bare) = new_name.strip_prefix(':') {
            anyhow::bail!(
                "cannot rename to '{}': type the new name without the colon ('{}')",
                new_name,
                bare.trim_start_matches(':')
            );
        }
    }
    if !is_valid_symbol_name(&new_name) {
        anyhow::bail!("cannot rename: '{}' is not a valid symbol name", new_name);
    }

    let (fqn, sym) = match target {
        RenameTarget::Local { word, refs } => {
            if let Some(snapshot) = documents.snapshot(&uri) {
                reject_local_capture(&snapshot, &refs, &word, &new_name)?;
            }
            let mut edits = vec![TextEdit {
                range: refs.declaration,
                new_text: new_name.clone(),
            }];
            edits.extend(refs.usages.into_iter().map(|range| TextEdit {
                range,
                new_text: new_name.clone(),
            }));
            return Ok(Some(WorkspaceEdit {
                changes: Some(HashMap::from([(uri, edits)])),
                ..Default::default()
            }));
        }
        // Every site was validated by `rename_target`; each edit replaces the
        // name its notation ends with, so `::db`, `::alias/db` and
        // `:readx.db/db` all keep the notation they were written in.
        RenameTarget::Keyword { sites } => {
            let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
            for site in sites {
                changes.entry(site.uri).or_default().push(TextEdit {
                    range: site.name,
                    new_text: new_name.clone(),
                });
            }
            return Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }));
        }
        RenameTarget::Global { fqn, sym } => (fqn, sym),
    };

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

    // Declaration edit — from live text when the defining file is open
    // (its indexed range may be stale against unsaved edits).
    let decl_uri = Url::from_file_path(&sym.file)
        .map_err(|_| anyhow::anyhow!("invalid path: {:?}", sym.file))?;
    let decl_range = live_definition_range(index, documents, &sym, &fqn);
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
///    global `add` in here (references and rename also resolve locals
///    structurally, before ever calling this).
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

    // Resolve against the live buffer so unsaved edits use current ranges.
    // Position matching (below) runs without a word token, so a cursor on a
    // keyword's `:`/`::` marker still resolves — the occurrence/definition
    // range spans it.
    let snapshot = documents.snapshot(uri)?;
    let cfg = index.extract_config();

    // EDN config files (Integrant systems) have no symbols or aliases; match
    // the cursor against keyword occurrences only. `file_occurrences` applies
    // the `#ig/ref` gate, so a cursor in a non-Integrant manifest resolves to
    // nothing.
    if crate::config::is_edn(&path) {
        return extractor::file_occurrences_tree(&snapshot.tree, &snapshot.text, &path, &cfg)
            .into_iter()
            .find(|occ| range_contains(&occ.name_range, pos))
            .map(|occ| occ.fqn);
    }

    if let Ok((_, syms, occs)) =
        extractor::extract_full_tree(&snapshot.tree, &snapshot.text, &path, &cfg)
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
/// are re-extracted from their cached tree so unsaved edits produce correct
/// ranges; everything else comes from the index.
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
        let Some(snapshot) = documents.snapshot(&uri) else {
            continue;
        };
        let occs = extractor::file_occurrences_tree(
            &snapshot.tree,
            &snapshot.text,
            &path,
            &index.extract_config(),
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u32, start: u32, end: u32) -> Range {
        Range {
            start: Position {
                line,
                character: start,
            },
            end: Position {
                line,
                character: end,
            },
        }
    }

    /// The range a token of `text` would have if it started at column `col`.
    fn token_range(col: u32, text: &str) -> Range {
        at(4, col, col + text.encode_utf16().count() as u32)
    }

    #[test]
    fn name_suffix_range_covers_the_name_in_every_notation() {
        // `::db`, `::ig/db` and `:readx.db/db` all end with the name, so one
        // rule rewrites every notation and leaves the notation itself alone.
        for token in ["::db", "::ig/db", ":readx.db/db"] {
            let range = token_range(20, token);
            let suffix = name_suffix_range(range, token, "db")
                .unwrap_or_else(|| panic!("no suffix for {}", token));
            assert_eq!(suffix.end, range.end, "{}", token);
            assert_eq!(suffix.start.character, range.end.character - 2, "{}", token);
        }
    }

    #[test]
    fn name_suffix_range_counts_utf16_units_not_bytes() {
        // `naïve` is 5 UTF-16 units but 6 bytes; the edit covers 5 columns.
        let token = ":naïve.ns/naïve";
        let range = token_range(3, token);
        let suffix = name_suffix_range(range, token, "naïve").unwrap();
        assert_eq!(suffix.start.character, range.end.character - 5);
    }

    #[test]
    fn name_suffix_range_refuses_what_it_cannot_rewrite() {
        // A `{::keys [db]}` / `{:keys [app/db]}` entry reads the keyword but is
        // written as a symbol, and binds a local of that name besides.
        assert!(name_suffix_range(token_range(0, "db"), "db", "db").is_none());
        assert!(name_suffix_range(token_range(0, "app/db"), "app/db", "db").is_none());
        // A longer name that merely ends the same is a different keyword.
        assert!(name_suffix_range(token_range(0, ":ns/mydb"), ":ns/mydb", "db").is_none());
    }

    #[test]
    fn token_at_slices_by_utf16_columns() {
        let text = "(def x :naïve/db)\n";
        // `:naïve/db` starts at column 7 and is 9 UTF-16 units long.
        assert_eq!(token_at(text, at(0, 7, 16)).as_deref(), Some(":naïve/db"));
        // A range past the end of the line is not a token.
        assert_eq!(token_at(text, at(0, 7, 99)), None);
        assert_eq!(token_at(text, at(9, 0, 1)), None);
    }
}
