use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;

use super::extractor;
use super::jar_cache;
use super::{Index, NsMeta, Symbol};

pub fn build_index(_root: &Path, source_paths: &[PathBuf]) -> Result<Index> {
    let index = Index::new();
    let files = collect_clojure_files(source_paths);

    type Extracted = (NsMeta, Vec<Symbol>, Vec<super::Occurrence>);
    let results: Vec<Extracted> = files
        .par_iter()
        .filter_map(|file| {
            let source = match std::fs::read_to_string(file) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("failed to read {}: {}", file.display(), e);
                    return None;
                }
            };

            match extractor::extract_full(&source, file) {
                Ok(result) => Some(result),
                Err(e) => {
                    tracing::warn!("failed to extract {}: {}", file.display(), e);
                    None
                }
            }
        })
        .collect();

    for (meta, symbols, occurrences) in results {
        index.insert_file(meta, symbols, occurrences);
    }

    Ok(index)
}

/// Indexes library sources from a classpath: JAR files (with a per-JAR disk
/// cache) and source directories (git deps in ~/.gitlibs, :local/root deps).
///
/// Results are inserted directly into the shared `index`; project symbols
/// always win over library symbols with the same fqn.
pub fn index_classpath_libs(root: &Path, classpath: Vec<PathBuf>, index: &Index) {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let mut jars: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in classpath {
        // Canonicalize so relative entries ("." or "src") resolve and the
        // project-root check below catches them.
        let Ok(entry) = entry.canonicalize() else {
            continue;
        };
        if entry.extension().map(|e| e == "jar").unwrap_or(false) {
            jars.push(entry);
        } else if entry.is_dir() && !entry.starts_with(&root) {
            // The project's own source dirs are indexed separately.
            dirs.push(entry);
        }
    }

    for dir in &dirs {
        index_classpath_dir(dir, index);
    }

    if jars.is_empty() {
        return;
    }

    index_classpath_jars(&root, jars, index);
}

/// Indexes a library source directory from the classpath. No disk cache:
/// directories are cheap to walk and, unlike JARs, can change in place.
fn index_classpath_dir(dir: &Path, index: &Index) {
    let files = collect_clojure_files(&[dir.to_path_buf()]);
    let results: Vec<(NsMeta, Vec<Symbol>)> = files
        .par_iter()
        .filter_map(|file| {
            let source = std::fs::read_to_string(file).ok()?;
            extractor::extract(&source, file).ok()
        })
        .collect();

    for (meta, mut symbols) in results {
        // Same filtering as JAR indexing (see jar.rs)
        if meta.name.ends_with(".impl") || meta.name.ends_with(".internal") {
            continue;
        }
        for sym in &mut symbols {
            sym.source = super::SymbolSource::Dir(dir.to_path_buf());
        }
        let symbols: Vec<Symbol> = symbols
            .into_iter()
            .filter(|s| s.kind != super::DefKind::DefnPrivate)
            .collect();
        index.insert_lib_file(meta, symbols);
    }
}

fn index_classpath_jars(root: &Path, jars: Vec<PathBuf>, index: &Index) {
    let cache_dir = root.join(".clj-lsp").join("jar-cache");

    tracing::info!("indexing {} JAR(s) from classpath", jars.len());

    // Process JARs in parallel, collect results
    let all_results: Vec<Vec<(NsMeta, Vec<Symbol>)>> = jars
        .par_iter()
        .map(|jar| {
            // Try the disk cache first
            if let Some(cached) = jar_cache::load(&cache_dir, jar) {
                tracing::debug!("cache hit: {}", jar.display());
                // Reconstruct per-namespace pairs from the flat cache
                return cached
                    .namespaces
                    .into_iter()
                    .map(|ns| {
                        let syms: Vec<Symbol> = cached
                            .symbols
                            .iter()
                            .filter(|s| s.ns == ns.name)
                            .cloned()
                            .collect();
                        (ns, syms)
                    })
                    .collect();
            }

            // Cache miss — index the JAR
            match super::jar::index_jar(jar) {
                Ok(pairs) => {
                    // Persist to cache
                    if let Some(mtime) = jar_mtime(jar) {
                        let all_ns: Vec<NsMeta> = pairs.iter().map(|(m, _)| m.clone()).collect();
                        let all_syms: Vec<Symbol> =
                            pairs.iter().flat_map(|(_, s)| s.iter().cloned()).collect();
                        let entry = jar_cache::JarCacheEntry {
                            format_version: jar_cache::CACHE_FORMAT_VERSION,
                            mtime,
                            namespaces: all_ns,
                            symbols: all_syms,
                        };
                        if let Err(e) = jar_cache::save(&cache_dir, jar, &entry) {
                            tracing::warn!("failed to save cache for {}: {}", jar.display(), e);
                        }
                    }
                    pairs
                }
                Err(e) => {
                    tracing::warn!("failed to index {}: {}", jar.display(), e);
                    vec![]
                }
            }
        })
        .collect();

    // Insert all results into the shared index; project symbols always win
    // over JAR symbols with the same fqn (e.g. the project itself installed
    // in ~/.m2).
    for jar_results in all_results {
        for (meta, symbols) in jar_results {
            index.insert_lib_file(meta, symbols);
        }
    }
}

fn collect_clojure_files(source_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in source_paths {
        if !path.exists() {
            continue;
        }
        for entry in ignore::WalkBuilder::new(path).build() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    let ext = ext.to_string_lossy();
                    if ext == "clj" || ext == "cljs" || ext == "cljc" {
                        files.push(path.to_path_buf());
                    }
                }
            }
        }
    }
    files
}

fn jar_mtime(jar: &Path) -> Option<u64> {
    jar_cache::jar_mtime(jar)
}
