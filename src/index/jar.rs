use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::extractor;
use super::{DefKind, NsMeta, Symbol, SymbolSource};

/// Indexes all Clojure source files within a JAR (ZIP) file.
///
/// Returns one `(NsMeta, Vec<Symbol>)` pair per namespace found.
/// All symbols are tagged with `SymbolSource::Jar(jar_path)`.
/// Private symbols (`defn-`) and impl/internal namespaces are filtered out.
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
                // Skip impl/internal namespaces
                if meta.name.ends_with(".impl") || meta.name.ends_with(".internal") {
                    continue;
                }

                // Tag all symbols as JAR-sourced and drop private ones
                for sym in &mut symbols {
                    sym.source = SymbolSource::Jar(jar_path.to_path_buf());
                }
                let symbols: Vec<Symbol> = symbols
                    .into_iter()
                    .filter(|s| s.kind != DefKind::DefnPrivate)
                    .collect();

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
    fn test_index_jar_filters_private_symbols() {
        let tmp = make_jar(&[(
            "mylib/core.clj",
            b"(ns mylib.core)\n(defn public-fn [] nil)\n(defn- private-fn [] nil)",
        )]);

        let results = index_jar(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        let (_, symbols) = &results[0];
        // private-fn should be filtered out
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "public-fn");
    }

    #[test]
    fn test_index_jar_skips_impl_namespace() {
        let tmp = make_jar(&[("mylib/impl.clj", b"(ns mylib.impl)\n(defn internal [] nil)")]);

        let results = index_jar(tmp.path()).unwrap();
        assert!(results.is_empty());
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
