use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::{CoreSymbol, DefKind, Index, Symbol};

use super::{resolve_symbol, ResolvedSymbol};

pub fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: HoverParams,
) -> Result<Option<Hover>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let word = match documents.word_at(&uri, pos) {
        Some(w) => w,
        None => return Ok(None),
    };

    tracing::info!("hover: word={}", word);

    let path = match crate::uri::to_index_path(&uri) {
        Some(p) => p,
        None => return Ok(None),
    };
    let current_ns = index.file_ns(&path).unwrap_or_default();

    let md = resolve_and_format(index, &word, &current_ns);

    Ok(md.map(|value| Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: None,
    }))
}

pub fn resolve_and_format(index: &Index, word: &str, current_ns: &str) -> Option<String> {
    match resolve_symbol(index, word, current_ns)? {
        ResolvedSymbol::Project(sym) => Some(format_for_symbol(&sym)),
        ResolvedSymbol::Core(core) => Some(format_for_core(&core)),
        ResolvedSymbol::SpecialForm(sf) => Some(format_for_special_form(sf)),
        ResolvedSymbol::LetgoNative(core) => Some(format_for_letgo_native(&core)),
    }
}

pub fn format_for_special_form(sf: &super::letgo_builtins::SpecialForm) -> String {
    let mut md = String::new();
    md.push_str(&format!("```clojure\n{}\n```\n", sf.usage));
    md.push_str("*special form*\n");
    if !sf.doc.is_empty() {
        md.push('\n');
        md.push_str(sf.doc);
    }
    md
}

/// A let-go native core fn: rendered like a fn, but labelled native (its doc
/// and arglists are borrowed from the clojure.core table).
pub fn format_for_letgo_native(sym: &CoreSymbol) -> String {
    let mut md = String::new();

    let params = if sym.params.is_empty() {
        String::new()
    } else {
        format!(" {}", sym.params)
    };

    md.push_str(&format!("```clojure\n({}{})\n```\n", sym.name, params));
    md.push_str("*let-go core (native)*\n");

    if !sym.doc.is_empty() {
        md.push('\n');
        md.push_str(&sym.doc);
    }

    md
}

pub fn format_for_symbol(sym: &Symbol) -> String {
    let mut md = String::new();

    let params = if sym.params.is_empty() {
        String::new()
    } else {
        format!(" {}", sym.params.join(" "))
    };

    md.push_str(&format!(
        "```clojure\n({} {}{})\n```\n",
        defkind_str(&sym.kind),
        sym.name,
        params
    ));

    md.push_str(&format!("*{}*\n", sym.ns));

    if let Some(doc) = &sym.doc {
        md.push('\n');
        md.push_str(doc);
    }

    md
}

pub fn format_for_core(sym: &CoreSymbol) -> String {
    let mut md = String::new();

    let params = if sym.params.is_empty() {
        String::new()
    } else {
        format!(" {}", sym.params)
    };

    md.push_str(&format!("```clojure\n({}{})\n```\n", sym.name, params));
    md.push_str("*clojure.core*\n");

    if !sym.doc.is_empty() {
        md.push('\n');
        md.push_str(&sym.doc);
    }

    md
}

fn defkind_str(kind: &DefKind) -> &'static str {
    match kind {
        DefKind::Def => "def",
        DefKind::Defonce => "defonce",
        DefKind::Defn => "defn",
        DefKind::DefnPrivate => "defn-",
        DefKind::Defmacro => "defmacro",
        DefKind::Defmulti => "defmulti",
        DefKind::Defmethod => "defmethod",
        DefKind::Defprotocol => "defprotocol",
        DefKind::Defrecord => "defrecord",
        DefKind::Deftype => "deftype",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_form_hover_is_labelled() {
        let sf = crate::handlers::letgo_builtins::special_form("if").unwrap();
        let md = format_for_special_form(sf);
        assert!(md.contains("*special form*"), "missing label: {}", md);
        assert!(md.contains("(if test then else?)"), "missing usage: {}", md);
    }

    #[test]
    fn letgo_native_hover_is_labelled() {
        let core = CoreSymbol {
            name: "count".to_string(),
            params: "([coll])".to_string(),
            doc: "Returns the number of items in the collection.".to_string(),
        };
        let md = format_for_letgo_native(&core);
        assert!(
            md.contains("*let-go core (native)*"),
            "missing label: {}",
            md
        );
        assert!(md.contains("([coll])"), "missing arglists: {}", md);
    }
}
