use std::io::Read;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::Url;

/// Parses a JAR URI of the form `jar:file:///path/to.jar!/entry/path.clj`
/// into `(PathBuf("/path/to.jar"), "entry/path.clj")`.
pub fn parse_jar_uri(uri: &str) -> anyhow::Result<(PathBuf, String)> {
    let without_jar = uri
        .strip_prefix("jar:")
        .ok_or_else(|| anyhow::anyhow!("URI does not start with 'jar:': {}", uri))?;

    let (file_url_str, entry_path) = without_jar
        .split_once("!/")
        .ok_or_else(|| anyhow::anyhow!("JAR URI missing '!/' separator: {}", uri))?;

    let file_url = Url::parse(file_url_str)
        .map_err(|e| anyhow::anyhow!("Invalid file URL '{}': {}", file_url_str, e))?;
    let jar_path = file_url
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("Cannot convert URL to path: {}", file_url_str))?;

    Ok((jar_path, entry_path.to_string()))
}

/// Opens the JAR at `jar_path` and returns the UTF-8 text of `entry_path`.
pub fn extract_content(jar_path: &Path, entry_path: &str) -> anyhow::Result<String> {
    let file = std::fs::File::open(jar_path)
        .map_err(|e| anyhow::anyhow!("Could not open JAR '{}': {}", jar_path.display(), e))?;

    let mut zip = zip::ZipArchive::new(file).map_err(|e| {
        anyhow::anyhow!("Could not read ZIP archive '{}': {}", jar_path.display(), e)
    })?;

    let mut entry = zip.by_name(entry_path).map_err(|_| {
        anyhow::anyhow!(
            "Entry '{}' not found in JAR '{}'",
            entry_path,
            jar_path.display()
        )
    })?;

    let mut content = String::new();
    entry
        .read_to_string(&mut content)
        .map_err(|e| anyhow::anyhow!("Failed to read entry '{}': {}", entry_path, e))?;

    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
    fn test_parse_jar_uri_valid() {
        let (path, entry) = parse_jar_uri("jar:file:///tmp/lib.jar!/clojure/string.clj").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/lib.jar"));
        assert_eq!(entry, "clojure/string.clj");
    }

    #[test]
    fn test_parse_jar_uri_missing_separator() {
        let result = parse_jar_uri("jar:file:///tmp/lib.jar/clojure/string.clj");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("missing '!/'"), "unexpected error: {}", msg);
    }

    #[test]
    fn test_extract_content_found() {
        let source = b"(ns clojure.string)\n(defn blank? [s] (empty? s))";
        let tmp = make_jar(&[("clojure/string.clj", source)]);

        let content = extract_content(tmp.path(), "clojure/string.clj").unwrap();
        assert_eq!(content, std::str::from_utf8(source).unwrap());
    }

    #[test]
    fn test_extract_content_missing_entry() {
        let tmp = make_jar(&[("clojure/string.clj", b"(ns clojure.string)")]);

        let result = extract_content(tmp.path(), "clojure/core.clj");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not found"), "unexpected error: {}", msg);
    }
}
