use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::jdk::{JavaClassInfo, JavaCtor, JavaMember};
use crate::index::{CoreSymbol, DefKind, Index, Symbol};

use super::java::JavaTargetKind;
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
    if let Some(resolved) = resolve_symbol(index, word, current_ns) {
        return Some(match resolved {
            ResolvedSymbol::Project(sym) => format_for_symbol(&sym),
            ResolvedSymbol::Core(core) => format_for_core(&core),
            ResolvedSymbol::SpecialForm(sf) => format_for_special_form(sf),
            ResolvedSymbol::LetgoNative(core) => format_for_letgo_native(&core),
        });
    }
    // Built-in Java interop fallback (class, static member, constructor).
    format_for_java(index, word, current_ns)
}

/// Hover markdown for a built-in Java class, static member, or constructor.
fn format_for_java(index: &Index, word: &str, current_ns: &str) -> Option<String> {
    let target = super::java::resolve_java_word(index, word, current_ns)?;
    let info = index.jdk()?.class(&target.class_fqn)?;
    let md = match target.kind {
        JavaTargetKind::StaticMember => {
            let member = target.member.as_deref().unwrap_or_default();
            if let Some(m) = info.methods.iter().find(|m| m.name == member) {
                format_java_method(&info.fqn, m)
            } else if let Some(f) = info.fields.iter().find(|f| f.name == member) {
                format_java_field(&info.fqn, f)
            } else {
                format_java_class(&info)
            }
        }
        JavaTargetKind::Ctor => info
            .ctors
            .first()
            .map(|c| format_java_ctor(&info.fqn, c))
            .unwrap_or_else(|| format_java_class(&info)),
        JavaTargetKind::Class => format_java_class(&info),
    };
    Some(md)
}

fn java_md(signature: &str, label: &str, javadoc: Option<&str>) -> String {
    let mut md = format!("```java\n{signature}\n```\n*{label}*\n");
    if let Some(doc) = javadoc {
        md.push('\n');
        md.push_str(doc);
    }
    md
}

fn format_java_class(info: &JavaClassInfo) -> String {
    let mut sig = format!("class {}", info.fqn);
    if let Some(sup) = &info.extends {
        sig.push_str(&format!(" extends {sup}"));
    }
    if !info.implements.is_empty() {
        sig.push_str(&format!(" implements {}", info.implements.join(", ")));
    }
    java_md(&sig, "java class", info.javadoc.as_deref())
}

fn format_java_method(class_fqn: &str, m: &JavaMember) -> String {
    let stat = if m.is_static { "static " } else { "" };
    let ret = m.return_type.as_deref().unwrap_or("void");
    let sig = format!("{stat}{ret} {}({})", m.name, m.params.join(", "));
    java_md(&sig, class_fqn, m.javadoc.as_deref())
}

fn format_java_field(class_fqn: &str, f: &JavaMember) -> String {
    let stat = if f.is_static { "static " } else { "" };
    let ty = f.return_type.as_deref().unwrap_or("");
    let sig = format!("{stat}{ty} {}", f.name);
    java_md(sig.trim(), class_fqn, f.javadoc.as_deref())
}

fn format_java_ctor(class_fqn: &str, c: &JavaCtor) -> String {
    let simple = class_fqn.rsplit('.').next().unwrap_or(class_fqn);
    let sig = format!("{simple}({})", c.params.join(", "));
    java_md(&sig, class_fqn, c.javadoc.as_deref())
}

pub fn format_for_special_form(sf: &super::builtins::SpecialForm) -> String {
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
        DefKind::IntegrantKey => "defmethod",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_form_hover_is_labelled() {
        let sf = crate::handlers::builtins::special_form("if", true).unwrap();
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

    #[test]
    fn java_member_hover_has_signature_and_javadoc() {
        let (index, _zip) = crate::handlers::java::test_fixture();
        let md = resolve_and_format(&index, "Greeter/greet", "app.core").unwrap();
        assert!(md.contains("static String greet(String name)"), "{}", md);
        assert!(md.contains("Greet by name"), "{}", md);
    }

    #[test]
    fn java_class_hover_has_class_and_javadoc() {
        let (index, _zip) = crate::handlers::java::test_fixture();
        let md = resolve_and_format(&index, "Greeter", "app.core").unwrap();
        assert!(md.contains("class demo.lib.Greeter"), "{}", md);
        assert!(md.contains("A greeter"), "{}", md);
    }
}
