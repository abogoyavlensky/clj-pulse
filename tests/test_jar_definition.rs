use std::io::Write;
use std::path::PathBuf;

use clj_pulse::handlers::{resolve_symbol, ResolvedSymbol};
use clj_pulse::index::{Index, SymbolSource};
use tower_lsp::lsp_types::Url;

fn make_jar(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
    let tmp = tempfile::Builder::new().suffix(".jar").tempfile().unwrap();
    let file = std::fs::File::create(tmp.path()).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    for (name, content) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(content).unwrap();
    }
    zip.finish().unwrap();
    tmp
}

#[test]
fn test_goto_definition_jar_returns_jar_uri() {
    let tmp = make_jar(&[(
        "mylib/core.clj",
        b"(ns mylib.core)\n\n(defn hello [] \"hello\")",
    )]);
    let jar_path = tmp.path().to_path_buf();

    let results = clj_pulse::index::jar::index_jar(&jar_path).unwrap();
    let index = Index::new();
    for (meta, syms) in results {
        index.insert_file(meta, syms, vec![]);
    }

    let result = resolve_symbol(&index, "mylib.core/hello", "user");
    let sym = match result {
        Some(ResolvedSymbol::Project(s)) => s,
        other => panic!("expected Project symbol, got {:?}", other),
    };

    assert!(matches!(sym.source, SymbolSource::Jar(_)));

    // Build the URI the same way definition.rs does
    let file_str = sym.file.to_string_lossy();
    let (jar_part, entry_part) = file_str.split_once("!/").expect("virtual path has !/");
    let jar_url = Url::from_file_path(jar_part).expect("valid jar path");
    let jar_uri = format!("jar:{}!/{}", jar_url, entry_part);

    assert!(jar_uri.starts_with("jar:file://"), "URI: {}", jar_uri);
    assert!(
        jar_uri.contains("!/mylib/core.clj"),
        "URI should contain entry path: {}",
        jar_uri
    );

    // Verify round-trip: parse the URI back
    let (parsed_path, parsed_entry) = clj_pulse::jar_content::parse_jar_uri(&jar_uri).unwrap();
    assert_eq!(parsed_path, jar_path);
    assert_eq!(parsed_entry, "mylib/core.clj");
}

#[test]
fn test_jar_content_extract_roundtrip() {
    let source = b"(ns mylib.core)\n\n(defn hello [] \"hello\")";
    let tmp = make_jar(&[("mylib/core.clj", source)]);

    let content = clj_pulse::jar_content::extract_content(tmp.path(), "mylib/core.clj").unwrap();
    assert!(
        content.contains("(ns mylib.core)"),
        "unexpected content: {}",
        content
    );
}

#[test]
fn test_parse_jar_uri_roundtrip() {
    let uri = "jar:file:///tmp/test.jar!/mylib/core.clj";
    let (jar_path, entry_path) = clj_pulse::jar_content::parse_jar_uri(uri).unwrap();
    assert_eq!(jar_path, PathBuf::from("/tmp/test.jar"));
    assert_eq!(entry_path, "mylib/core.clj");
}
