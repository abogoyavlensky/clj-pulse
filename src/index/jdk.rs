//! JDK source (`src.zip`) indexing for built-in Java navigation, hover,
//! completion, and signature help.
//!
//! Reads the JDK's bundled `.java` sources from `lib/src.zip` and parses them
//! with tree-sitter-java. Built-in (JDK) Java only — library `.class` bytecode
//! is out of scope (a later phase).
//!
//! The class → entry map is built eagerly from zip entry names alone (no
//! parsing). Individual classes are parsed lazily on first use and cached.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use tower_lsp::lsp_types::Range;
use tree_sitter::{Node, Parser};
use tree_sitter_language::LanguageFn;

use super::extractor::point_to_position;

static JAVA_LANGUAGE: OnceLock<tree_sitter::Language> = OnceLock::new();

/// The tree-sitter-java grammar, cached. Mirrors `extractor::language()`.
fn java_language() -> &'static tree_sitter::Language {
    JAVA_LANGUAGE.get_or_init(|| {
        let lang_fn: LanguageFn = tree_sitter_java::LANGUAGE;
        lang_fn.into()
    })
}

/// A member (method or field) of a Java class.
#[derive(Debug, Clone)]
pub struct JavaMember {
    pub name: String,
    /// Parameter source text, e.g. `["String name", "int flags"]`. Empty for
    /// fields.
    pub params: Vec<String>,
    /// Return type (method) or declared type (field), as written.
    pub return_type: Option<String>,
    pub is_static: bool,
    /// Range of the member's name token, for go-to-definition.
    pub name_range: Range,
    pub javadoc: Option<String>,
}

/// A constructor of a Java class.
#[derive(Debug, Clone)]
pub struct JavaCtor {
    pub params: Vec<String>,
    /// Range of the constructor's name token (the class name).
    pub name_range: Range,
    pub javadoc: Option<String>,
}

/// Parsed structure of a single Java class, derived from its `src.zip` source.
#[derive(Debug, Clone)]
pub struct JavaClassInfo {
    pub fqn: String,
    /// The `src.zip` entry this was parsed from.
    pub entry: String,
    /// Range of the class declaration's name token.
    pub decl_name_range: Range,
    pub extends: Option<String>,
    pub implements: Vec<String>,
    pub methods: Vec<JavaMember>,
    pub fields: Vec<JavaMember>,
    pub ctors: Vec<JavaCtor>,
}

/// Index of a JDK's bundled Java source.
pub struct JdkIndex {
    src_zip: PathBuf,
    /// Fully-qualified class name → zip entry path.
    class_entries: HashMap<String, String>,
    /// Lazily-parsed classes, keyed by fully-qualified name.
    parsed: DashMap<String, Arc<JavaClassInfo>>,
}

impl JdkIndex {
    /// Discovers the JDK's `src.zip` and builds the class → entry map. Returns
    /// `None` when no JDK source can be found (feature stays off).
    pub fn discover() -> Option<JdkIndex> {
        let src_zip = find_src_zip()?;
        tracing::debug!("using JDK source: {}", src_zip.display());
        JdkIndex::discover_from(src_zip)
    }

    /// Builds an index from an explicit `src.zip` path (test seam + override).
    pub fn discover_from(src_zip: PathBuf) -> Option<JdkIndex> {
        let file = std::fs::File::open(&src_zip).ok()?;
        let mut zip = zip::ZipArchive::new(file).ok()?;
        let mut class_entries = HashMap::new();
        for i in 0..zip.len() {
            let entry = match zip.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.is_dir() {
                continue;
            }
            let name = entry.name();
            if let Some(fqn) = entry_to_fqn(name) {
                class_entries.insert(fqn, name.to_string());
            }
        }
        if class_entries.is_empty() {
            return None;
        }
        Some(JdkIndex {
            src_zip,
            class_entries,
            parsed: DashMap::new(),
        })
    }

    /// Number of indexed classes (for the startup log).
    pub fn class_count(&self) -> usize {
        self.class_entries.len()
    }

    /// The `src.zip` entry for a fully-qualified class, if present.
    pub fn entry_for(&self, fqn: &str) -> Option<&str> {
        self.class_entries.get(fqn).map(String::as_str)
    }

    /// The discovered `src.zip` path, for building `jar:` navigation URIs.
    pub fn src_zip(&self) -> &Path {
        &self.src_zip
    }

    /// Whether a fully-qualified class is known (entry-name lookup, no parse).
    pub fn has_class(&self, fqn: &str) -> bool {
        self.class_entries.contains_key(fqn)
    }

    /// Fully-qualified names whose simple name starts with `prefix`.
    pub fn class_names_with_prefix<'a>(&'a self, prefix: &str) -> Vec<&'a str> {
        self.class_entries
            .keys()
            .filter(|fqn| simple_name(fqn).starts_with(prefix))
            .map(String::as_str)
            .collect()
    }

    /// Returns the parsed class, lazily reading and parsing its `.java` the
    /// first time, then caching it.
    pub fn class(&self, fqn: &str) -> Option<Arc<JavaClassInfo>> {
        if let Some(info) = self.parsed.get(fqn) {
            return Some(info.clone());
        }
        let entry = self.class_entries.get(fqn)?.clone();
        let source = self.read_entry(&entry)?;
        let info = Arc::new(parse_class(fqn, &entry, &source)?);
        self.parsed.insert(fqn.to_string(), info.clone());
        Some(info)
    }

    fn read_entry(&self, entry: &str) -> Option<String> {
        let file = std::fs::File::open(&self.src_zip).ok()?;
        let mut zip = zip::ZipArchive::new(file).ok()?;
        let mut e = zip.by_name(entry).ok()?;
        let mut source = String::new();
        e.read_to_string(&mut source).ok()?;
        Some(source)
    }
}

/// Locates the JDK `src.zip`, without spawning any process:
/// `CLJ_PULSE_JDK_SRC` override → `$JAVA_HOME/lib/src.zip` → `java` on `PATH`.
fn find_src_zip() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CLJ_PULSE_JDK_SRC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let p = Path::new(&home).join("lib").join("src.zip");
        if p.is_file() {
            return Some(p);
        }
    }
    src_zip_from_path_java()
}

/// Resolves `java` on `PATH` to its JDK home and checks for `lib/src.zip`.
/// Note: shim-based tool managers (mise/asdf) resolve `java` to the shim, not
/// the JDK, so this yields nothing there — `JAVA_HOME` or the override is the
/// reliable signal in that case.
fn src_zip_from_path_java() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let java = dir.join("java");
        if !java.is_file() {
            continue;
        }
        let Ok(real) = std::fs::canonicalize(&java) else {
            continue;
        };
        // real == <home>/bin/java
        if let Some(home) = real.parent().and_then(Path::parent) {
            let candidate = home.join("lib").join("src.zip");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Maps a `src.zip` entry path to a fully-qualified class name, or `None` for
/// non-class entries. JDK 9+ entries are module-prefixed
/// (`java.base/java/lang/String.java`); the module segment (the only path
/// component containing a `.`) is stripped. Pre-9 entries have no prefix.
fn entry_to_fqn(entry: &str) -> Option<String> {
    let path = entry.strip_suffix(".java")?;
    let mut parts: Vec<&str> = path.split('/').collect();
    if parts.first().is_some_and(|p| p.contains('.')) {
        parts.remove(0);
    }
    let last = parts.last()?;
    if parts.is_empty() || *last == "module-info" || *last == "package-info" {
        return None;
    }
    Some(parts.join("."))
}

fn simple_name(fqn: &str) -> &str {
    fqn.rsplit('.').next().unwrap_or(fqn)
}

fn parse_class(fqn: &str, entry: &str, source: &str) -> Option<JavaClassInfo> {
    let mut parser = Parser::new();
    parser.set_language(java_language()).ok()?;
    let tree = parser.parse(source, None)?;
    let decl = find_type_decl(tree.root_node())?;
    let name_node = decl.child_by_field_name("name")?;

    let extends = decl
        .child_by_field_name("superclass")
        .and_then(|n| n.named_child(0))
        .map(|t| node_text(t, source).to_string());
    let implements = decl
        .child_by_field_name("interfaces")
        .into_iter()
        .flat_map(named_children) // type_list
        .flat_map(named_children) // each type
        .map(|t| node_text(t, source).to_string())
        .collect();

    let mut methods = Vec::new();
    let mut fields = Vec::new();
    let mut ctors = Vec::new();
    if let Some(body) = decl.child_by_field_name("body") {
        for member in named_children(body) {
            match member.kind() {
                "method_declaration" => {
                    if let Some(m) = parse_method(member, source) {
                        methods.push(m);
                    }
                }
                "constructor_declaration" => {
                    if let Some(c) = parse_ctor(member, source) {
                        ctors.push(c);
                    }
                }
                "field_declaration" => parse_fields(member, source, &mut fields),
                _ => {}
            }
        }
    }

    Some(JavaClassInfo {
        fqn: fqn.to_string(),
        entry: entry.to_string(),
        decl_name_range: node_to_lsp_range(name_node, source),
        extends,
        implements,
        methods,
        fields,
        ctors,
    })
}

fn find_type_decl(root: Node) -> Option<Node> {
    named_children(root).into_iter().find(|n| {
        matches!(
            n.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        )
    })
}

fn parse_method(node: Node, source: &str) -> Option<JavaMember> {
    let name_node = node.child_by_field_name("name")?;
    Some(JavaMember {
        name: node_text(name_node, source).to_string(),
        params: parse_params(node, source),
        return_type: node
            .child_by_field_name("type")
            .map(|n| node_text(n, source).trim().to_string()),
        is_static: has_static_modifier(node, source),
        name_range: node_to_lsp_range(name_node, source),
        javadoc: preceding_javadoc(node, source),
    })
}

fn parse_ctor(node: Node, source: &str) -> Option<JavaCtor> {
    let name_node = node.child_by_field_name("name")?;
    Some(JavaCtor {
        params: parse_params(node, source),
        name_range: node_to_lsp_range(name_node, source),
        javadoc: preceding_javadoc(node, source),
    })
}

fn parse_fields(node: Node, source: &str, out: &mut Vec<JavaMember>) {
    let is_static = has_static_modifier(node, source);
    let return_type = node
        .child_by_field_name("type")
        .map(|n| node_text(n, source).trim().to_string());
    let javadoc = preceding_javadoc(node, source);
    for child in named_children(node) {
        if child.kind() == "variable_declarator" {
            if let Some(name_node) = child.child_by_field_name("name") {
                out.push(JavaMember {
                    name: node_text(name_node, source).to_string(),
                    params: Vec::new(),
                    return_type: return_type.clone(),
                    is_static,
                    name_range: node_to_lsp_range(name_node, source),
                    javadoc: javadoc.clone(),
                });
            }
        }
    }
}

fn parse_params(node: Node, source: &str) -> Vec<String> {
    let Some(params) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    named_children(params)
        .into_iter()
        .filter(|p| matches!(p.kind(), "formal_parameter" | "spread_parameter"))
        .map(|p| {
            node_text(p, source)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn has_static_modifier(node: Node, source: &str) -> bool {
    for child in named_children(node) {
        if child.kind() == "modifiers" {
            return node_text(child, source)
                .split_whitespace()
                .any(|t| t == "static");
        }
    }
    false
}

/// The `/** … */` Javadoc immediately preceding a declaration, cleaned up.
fn preceding_javadoc(node: Node, source: &str) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() == "block_comment" {
        let text = node_text(prev, source);
        if text.starts_with("/**") {
            return Some(clean_javadoc(text));
        }
    }
    None
}

fn clean_javadoc(raw: &str) -> String {
    let inner = raw
        .trim_start_matches("/**")
        .trim_end_matches("*/")
        .trim_end_matches('*');
    inner
        .lines()
        .map(|l| l.trim().trim_start_matches('*').trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn named_children(node: Node) -> Vec<Node> {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .collect()
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn node_to_lsp_range(node: Node, source: &str) -> Range {
    Range {
        start: point_to_position(node.start_position(), node.start_byte(), source),
        end: point_to_position(node.end_position(), node.end_byte(), source),
    }
}

/// Builds a temporary `src.zip` from `(entry, contents)` pairs. Test-only and
/// shared across the lib's unit tests (jdk, handlers::java, hover, …).
#[cfg(test)]
pub(crate) fn make_src_zip(entries: &[(&str, &str)]) -> tempfile::NamedTempFile {
    use std::io::Write;
    let tmp = tempfile::Builder::new().suffix(".zip").tempfile().unwrap();
    let file = std::fs::File::create(tmp.path()).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    for (name, content) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms the tree-sitter-java grammar loads and parses against the
    /// pinned tree-sitter 0.25 — the one residual ABI risk from the dependency.
    #[test]
    fn parses_java() {
        let mut parser = Parser::new();
        parser
            .set_language(java_language())
            .expect("tree-sitter-java must load against tree-sitter 0.25");
        let tree = parser
            .parse("class A { static int f(int x) { return x; } }", None)
            .expect("parse");
        let root = tree.root_node();
        assert!(!root.has_error(), "fixture should parse cleanly");
        assert!(find_type_decl(root).is_some());
    }

    #[test]
    fn class_map_strips_module() {
        let zip = make_src_zip(&[(
            "java.base/java/lang/String.java",
            "package java.lang; public class String {}",
        )]);
        let jdk = JdkIndex::discover_from(zip.path().to_path_buf()).expect("discover");
        assert_eq!(
            jdk.entry_for("java.lang.String"),
            Some("java.base/java/lang/String.java")
        );
        assert!(jdk
            .class_names_with_prefix("Strin")
            .contains(&"java.lang.String"));
    }

    #[test]
    fn parses_members() {
        let src = r#"package demo.lib;
/** A greeter. */
public class Greeter {
    /** The version. */
    public static final int VERSION = 1;
    /** Make a greeter. */
    public Greeter(int seed) {}
    /** Greet by name. */
    public static String greet(String name) { return name; }
}
"#;
        let zip = make_src_zip(&[("java.base/demo/lib/Greeter.java", src)]);
        let jdk = JdkIndex::discover_from(zip.path().to_path_buf()).unwrap();
        let info = jdk.class("demo.lib.Greeter").expect("class parsed");

        let greet = info
            .methods
            .iter()
            .find(|m| m.name == "greet")
            .expect("greet");
        assert!(greet.is_static);
        assert_eq!(greet.params, vec!["String name".to_string()]);
        assert_eq!(greet.return_type.as_deref(), Some("String"));
        assert!(greet
            .javadoc
            .as_deref()
            .unwrap_or("")
            .contains("Greet by name"));

        assert_eq!(info.ctors.len(), 1);
        assert_eq!(info.ctors[0].params, vec!["int seed".to_string()]);

        let version = info
            .fields
            .iter()
            .find(|f| f.name == "VERSION")
            .expect("VERSION");
        assert!(version.is_static);
    }
}
