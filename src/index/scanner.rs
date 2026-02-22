use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;

use super::extractor;
use super::{Index, NsMeta, Symbol};

pub fn build_index(_root: &Path, source_paths: &[PathBuf]) -> Result<Index> {
    let index = Index::new();
    let files = collect_clojure_files(source_paths);

    let results: Vec<(NsMeta, Vec<Symbol>)> = files
        .par_iter()
        .filter_map(|file| {
            let source = match std::fs::read_to_string(file) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("failed to read {}: {}", file.display(), e);
                    return None;
                }
            };

            match extractor::extract(&source, file) {
                Ok(result) => Some(result),
                Err(e) => {
                    tracing::warn!("failed to extract {}: {}", file.display(), e);
                    None
                }
            }
        })
        .collect();

    for (meta, symbols) in results {
        index.insert_file(meta, symbols);
    }

    Ok(index)
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
