use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::extractor;
use super::{NsMeta, Symbol, SymbolSource};

/// Indexes all Clojure source files within a JAR (ZIP) file.
///
/// Returns one `(NsMeta, Vec<Symbol>)` pair per namespace found.
/// All symbols are tagged with `SymbolSource::Jar(jar_path)`.
/// Private symbols (`defn-`) are filtered out.
pub fn index_jar(jar_path: &Path) -> Result<Vec<(NsMeta, Vec<Symbol>)>> {
    let file = std::fs::File::open(jar_path)?;
    let mut zip = zip::ZipArchive::new(file)?;

    // Collect entry names first so we can borrow the archive mutably below
    let entry_names: Vec<String> = (0..zip.len())
        .filter_map(|i| {
            let entry = zip.by_index(i).ok()?;
            let name = entry.name().to_string();
            if is_clojure_source(&name) {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    let mut results = Vec::new();

    for name in entry_names {
        let mut entry = match zip.by_name(&name) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let mut source = String::new();
        if entry.read_to_string(&mut source).is_err() {
            continue;
        }

        // Virtual path used as the key in file_to_ns; unique per entry
        let virtual_path = PathBuf::from(format!("{}!/{}", jar_path.display(), name));

        match extractor::extract(&source, &virtual_path) {
            Ok((meta, mut symbols)) => {
                // `.impl`/`.internal` namespaces and private (`defn-`) symbols
                // are all indexed so navigation, hover, and references reach
                // library internals from inside library sources. Completion keeps
                // `.impl`/`.internal` out of its require list, and workspace
                // symbol search is project-only — so neither floods the user.
                for sym in &mut symbols {
                    sym.source = SymbolSource::Jar(jar_path.to_path_buf());
                }
                results.push((meta, symbols));
            }
            Err(e) => {
                tracing::debug!("failed to extract {}!/{}: {}", jar_path.display(), name, e);
            }
        }
    }

    Ok(results)
}

fn is_clojure_source(name: &str) -> bool {
    name.ends_with(".clj") || name.ends_with(".cljs") || name.ends_with(".cljc")
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
    fn test_index_jar_basic() {
        let tmp = make_jar(&[(
            "mylib/core.clj",
            b"(ns mylib.core)\n\n(defn hello [] \"hello\")",
        )]);

        let jar_path = tmp.path().to_path_buf();
        let results = index_jar(&jar_path).unwrap();

        assert_eq!(results.len(), 1);
        let (meta, symbols) = &results[0];
        assert_eq!(meta.name, "mylib.core");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "hello");
        assert_eq!(symbols[0].source, SymbolSource::Jar(jar_path));
    }

    #[test]
    fn test_index_jar_skips_non_clojure() {
        let tmp = make_jar(&[("Main.class", b"\xCA\xFE\xBA\xBE")]);
        let results = index_jar(tmp.path()).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_index_jar_keeps_private_symbols() {
        // Private (`defn-`) symbols are indexed so navigation into library
        // internals works; they are not surfaced by completion/workspace search.
        let tmp = make_jar(&[(
            "mylib/core.clj",
            b"(ns mylib.core)\n(defn public-fn [] nil)\n(defn- private-fn [] nil)",
        )]);

        let results = index_jar(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        let (_, symbols) = &results[0];
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"public-fn") && names.contains(&"private-fn"));
    }

    #[test]
    fn test_index_jar_includes_impl_namespace() {
        // `.impl`/`.internal` namespaces are indexed so navigation into library
        // internals works; completion hides them separately.
        let tmp = make_jar(&[("mylib/impl.clj", b"(ns mylib.impl)\n(defn helper [] nil)")]);

        let results = index_jar(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.name, "mylib.impl");
        assert_eq!(results[0].1[0].name, "helper");
    }

    #[test]
    fn test_index_jar_source_tagged() {
        let tmp = make_jar(&[("mylib/util.clj", b"(ns mylib.util)\n(defn helper [] nil)")]);

        let jar_path = tmp.path().to_path_buf();
        let results = index_jar(&jar_path).unwrap();
        assert_eq!(results.len(), 1);
        let (_, symbols) = &results[0];
        for sym in symbols {
            assert_eq!(sym.source, SymbolSource::Jar(jar_path.clone()));
        }
    }
}
