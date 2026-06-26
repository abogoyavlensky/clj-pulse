//! Resolution of Java interop forms (class references, static members,
//! constructors) to built-in JDK classes in the [`JdkIndex`](crate::index::jdk).
//!
//! Handlers consult this only as a fallback after ordinary Clojure resolution
//! returns nothing, so an alias like `str/join` resolves Clojure-side and never
//! reaches here. The `:import` map expands simple names; it is not a competing
//! precedence signal.

use std::collections::HashMap;

use crate::index::jdk::JdkIndex;
use crate::index::Index;

/// What a Java interop word under the cursor refers to.
#[derive(Debug, Clone, PartialEq)]
pub struct JavaTarget {
    pub class_fqn: String,
    /// The member (method/field) name for a `Class/member` usage; `None` for a
    /// bare class reference or a constructor.
    pub member: Option<String>,
    pub kind: JavaTargetKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JavaTargetKind {
    /// A class reference: `String`, imported `Instant`, FQN `java.util.Date`, or
    /// a `^Class` type hint.
    Class,
    /// A static member: `Math/sqrt`, `Instant/now`.
    StaticMember,
    /// A constructor: `(StringBuilder. …)`.
    Ctor,
}

/// Classifies the interop `word` under the cursor and resolves its class to a
/// JDK class. Returns `None` when no JDK source is indexed or the word is not a
/// known Java class.
pub fn resolve_java_word(index: &Index, word: &str, current_ns: &str) -> Option<JavaTarget> {
    let jdk = index.jdk()?;
    let imports = index
        .ns_meta(current_ns)
        .map(|m| m.imports)
        .unwrap_or_default();

    // Constructor: `Class.`
    if let Some(class) = word.strip_suffix('.') {
        if class.is_empty() {
            return None;
        }
        let class_fqn = resolve_class_fqn(class, &imports, jdk)?;
        return Some(JavaTarget {
            class_fqn,
            member: None,
            kind: JavaTargetKind::Ctor,
        });
    }

    // Static member: `Class/member` (the class part may be a dotted FQN).
    if let Some((class, member)) = word.split_once('/') {
        if class.is_empty() || member.is_empty() || member.contains('/') {
            return None;
        }
        let class_fqn = resolve_class_fqn(class, &imports, jdk)?;
        return Some(JavaTarget {
            class_fqn,
            member: Some(member.to_string()),
            kind: JavaTargetKind::StaticMember,
        });
    }

    // Bare class reference or fully-qualified class.
    let class_fqn = resolve_class_fqn(word, &imports, jdk)?;
    Some(JavaTarget {
        class_fqn,
        member: None,
        kind: JavaTargetKind::Class,
    })
}

/// Resolves a class name (simple or fully-qualified) to a known JDK class FQN:
/// an explicit `:import`, then a fully-qualified name used directly, then the
/// auto-imported `java.lang` package.
fn resolve_class_fqn(
    name: &str,
    imports: &HashMap<String, String>,
    jdk: &JdkIndex,
) -> Option<String> {
    if let Some(fqn) = imports.get(name) {
        if jdk.has_class(fqn) {
            return Some(fqn.clone());
        }
    }
    if name.contains('.') && jdk.has_class(name) {
        return Some(name.to_string());
    }
    let java_lang = format!("java.lang.{name}");
    if jdk.has_class(&java_lang) {
        return Some(java_lang);
    }
    None
}

/// Shared test fixture: an [`Index`] with JDK source (`demo.lib.Greeter` — a
/// static method, constructor, and field, each with Javadoc — and
/// `java.lang.Sample`) plus a project ns importing `demo.lib.Greeter`. The
/// returned tempfile must be kept alive while `jdk.class(..)` may be called.
#[cfg(test)]
pub(crate) fn test_fixture() -> (Index, tempfile::NamedTempFile) {
    use crate::index::extractor::extract;
    use crate::index::jdk::{make_src_zip, JdkIndex};

    const GREETER: &str = "package demo.lib;\n\
/** A greeter. */\n\
public class Greeter {\n\
    /** The version. */\n\
    public static final int VERSION = 1;\n\
    /** Make a greeter. */\n\
    public Greeter(int seed) {}\n\
    /** Greet by name. */\n\
    public static String greet(String name) { return name; }\n\
}\n";
    const SAMPLE: &str = "package java.lang;\n\
public class Sample {\n\
    public static Sample of(long n) { return null; }\n\
}\n";

    let zip = make_src_zip(&[
        ("java.base/demo/lib/Greeter.java", GREETER),
        ("java.base/java/lang/Sample.java", SAMPLE),
    ]);
    let index = Index::new();
    index.set_jdk(JdkIndex::discover_from(zip.path().to_path_buf()).unwrap());
    let (meta, syms) = extract(
        "(ns app.core (:import [demo.lib Greeter]))",
        std::path::Path::new("app/core.clj"),
    )
    .unwrap();
    index.insert_file(meta, syms, vec![]);
    (index, zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_imported_class() {
        let (index, _zip) = test_fixture();
        assert_eq!(
            resolve_java_word(&index, "Greeter", "app.core"),
            Some(JavaTarget {
                class_fqn: "demo.lib.Greeter".to_string(),
                member: None,
                kind: JavaTargetKind::Class,
            })
        );
    }

    #[test]
    fn resolves_static_member_and_ctor() {
        let (index, _zip) = test_fixture();
        let stat = resolve_java_word(&index, "Greeter/greet", "app.core").unwrap();
        assert_eq!(stat.class_fqn, "demo.lib.Greeter");
        assert_eq!(stat.member.as_deref(), Some("greet"));
        assert_eq!(stat.kind, JavaTargetKind::StaticMember);

        let ctor = resolve_java_word(&index, "Greeter.", "app.core").unwrap();
        assert_eq!(ctor.class_fqn, "demo.lib.Greeter");
        assert_eq!(ctor.kind, JavaTargetKind::Ctor);
    }

    #[test]
    fn resolves_auto_java_lang_without_import() {
        let (index, _zip) = test_fixture();
        assert_eq!(
            resolve_java_word(&index, "Sample/of", "app.core").map(|t| t.class_fqn),
            Some("java.lang.Sample".to_string())
        );
    }

    #[test]
    fn does_not_resolve_clojure_alias() {
        // Non-regression: `str/join` must not be mistaken for a Java class.
        let (index, _zip) = test_fixture();
        assert!(resolve_java_word(&index, "str/join", "app.core").is_none());
        assert!(resolve_java_word(&index, "Nope", "app.core").is_none());
    }
}
