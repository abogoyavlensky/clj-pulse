use anyhow::Result;
use std::path::Path;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::{extractor, Index};
use crate::uri;

use super::{resolve_symbol, ResolvedSymbol};

pub fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: GotoDefinitionParams,
) -> Result<Option<GotoDefinitionResponse>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

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

    // Local bindings (let/fn/loop/…) shadow vars, so a cursor on a locally-bound
    // name navigates to its binding site in the same file — checked before the
    // var/alias/core resolvers below.
    if let Some(loc) = local_definition(documents, &uri, pos) {
        return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
    }

    // Position-aware resolution first. It is context-aware — a protocol method
    // impl resolves to the protocol's declaration even when the bare name also
    // names a core/current-ns var — computed from the live buffer (like
    // references/rename) so unsaved edits resolve correctly, and it works even
    // when the cursor sits on a keyword's `:`/`::` marker, where there is no
    // word token. When it doesn't resolve to a known symbol, fall through to the
    // bare-word resolver, which also handles aliases, namespaces, and core.
    let resolved = super::references::resolve_fqn_at(index, documents, &uri, pos);
    tracing::debug!("goto_definition: resolved={:?}", resolved);
    if let Some(fqn) = resolved {
        if let Some(sym) = index.lookup(&fqn) {
            let location = location_for(&sym.file, sym.name_range)?;
            return Ok(Some(GotoDefinitionResponse::Scalar(location)));
        }
        // A resolved keyword (colon-prefixed fqn) has no var counterpart; never
        // fall through to bare-word resolution, which would match a same-named
        // var (`::counter` → `(defn counter …)`).
        if fqn.starts_with(':') {
            return Ok(None);
        }
    }
    // Unqualified keywords aren't recorded, so `resolve_fqn_at` returns None for
    // them; the token check still stops `:counter` from resolving to a var.
    if documents.is_keyword_at(&uri, pos) {
        return Ok(None);
    }

    // Bare-word resolution (var / alias / namespace) needs a word token.
    let word = match documents.word_at(&uri, pos) {
        Some(w) => w,
        None => return Ok(None),
    };
    tracing::info!("goto_definition: word={}", word);

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
        // Special forms and native core fns have no `.lg` source to navigate
        // to. But a native name can also be a require alias (`[clojure.string
        // :as str]`); when the cursor is on the alias declaration itself,
        // navigate to that namespace, mirroring the Core arm above.
        Some(ResolvedSymbol::SpecialForm(_)) | Some(ResolvedSymbol::LetgoNative(_)) => {
            if on_alias_declaration(documents, &uri, pos.line, &word) {
                return namespace_location(index, &current_ns, &word);
            }
            Ok(None)
        }
        None => {
            // The word may be a require alias (`[ring.util.response :as
            // response]` with the cursor on `response`) or a namespace name
            // itself — navigate to the top of that namespace's file.
            if let Some(resp) = namespace_location(index, &current_ns, &word)? {
                return Ok(Some(resp));
            }
            // Last resort: built-in Java interop (class, static member, ctor).
            java_definition(index, &word, &current_ns)
        }
    }
}

/// Resolves a cursor on a locally-bound name (`let`/`fn`/`loop`/`for`/
/// destructuring/…) to its binding site in the same document. Returns `None`
/// for keywords, qualified words (never locals), or any name not bound in scope
/// at `pos`, so ordinary var/alias/namespace resolution proceeds unchanged.
/// The innermost binding wins, so a local correctly shadows an outer one or a
/// same-named global var.
fn local_definition(documents: &DocumentStore, uri: &Url, pos: Position) -> Option<Location> {
    if documents.is_keyword_at(uri, pos) {
        return None;
    }
    let word = documents.word_at(uri, pos)?;
    if word.contains('/') {
        return None;
    }
    let text = documents.text(uri)?;
    let binding = extractor::locals_in_scope_at(&text, pos)
        .into_iter()
        .rev()
        .find(|b| b.name == word)?;
    Some(Location {
        uri: uri.clone(),
        range: binding.name_range,
    })
}

/// Built-in Java interop fallback: navigate to a JDK class, static member, or
/// constructor in the indexed `src.zip`. Reached only after Clojure resolution
/// yields nothing, so ordinary aliases (`str/join`) never get here.
fn java_definition(
    index: &Index,
    word: &str,
    current_ns: &str,
) -> Result<Option<GotoDefinitionResponse>> {
    use super::java::JavaTargetKind;

    let Some(target) = super::java::resolve_java_word(index, word, current_ns) else {
        return Ok(None);
    };
    let Some(jdk) = index.jdk() else {
        return Ok(None);
    };
    let Some(info) = jdk.class(&target.class_fqn) else {
        return Ok(None);
    };

    let range = match target.kind {
        JavaTargetKind::Class => info.decl_name_range,
        JavaTargetKind::Ctor => info
            .ctors
            .first()
            .map(|c| c.name_range)
            .unwrap_or(info.decl_name_range),
        JavaTargetKind::StaticMember => {
            let member = target.member.as_deref().unwrap_or_default();
            info.methods
                .iter()
                .chain(info.fields.iter())
                .find(|m| m.name == member)
                .map(|m| m.name_range)
                .unwrap_or(info.decl_name_range)
        }
    };

    // Virtual `<src.zip>!/<entry>` path → `jar:` URI via `location_for`.
    let virtual_path = format!("{}!/{}", jdk.src_zip().display(), info.entry);
    let location = location_for(Path::new(&virtual_path), range)?;
    Ok(Some(GotoDefinitionResponse::Scalar(location)))
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
