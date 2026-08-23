use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;

use super::extractor;
use super::jar_cache;
use super::{ExtractConfig, Index, NsMeta, Symbol};

/// A source root to scan: `path` is walked with gitignore ancestry stopping at
/// `project_dir` — a configured project may itself live under a dir the
/// workspace `.gitignore` excludes (e.g. `repos/foo`), and its scan must not
/// inherit that exclusion.
#[derive(Debug, Clone)]
pub struct ScanRoot {
    pub project_dir: PathBuf,
    pub path: PathBuf,
}

pub fn build_index(root: &Path, source_paths: &[PathBuf], cfg: &ExtractConfig) -> Result<Index> {
    let roots: Vec<ScanRoot> = source_paths
        .iter()
        .map(|p| ScanRoot {
            project_dir: root.to_path_buf(),
            path: p.clone(),
        })
        .collect();
    build_index_scoped(&roots, cfg)
}

/// [`build_index`] over source roots that may belong to different projects,
/// each scanned with project-scoped ignore rules (see [`ScanRoot`]).
pub fn build_index_scoped(roots: &[ScanRoot], cfg: &ExtractConfig) -> Result<Index> {
    let index = Index::new();
    let files = collect_clojure_files(roots);

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

            match extractor::extract_full_with(&source, file, cfg) {
                Ok(result) => Some(result),
                Err(e) => {
                    tracing::warn!("failed to extract {}: {}", file.display(), e);
                    None
                }
            }
        })
        .collect();

    for (meta, symbols, occurrences) in results {
        // Cross-project namespace collisions (two projects both defining ns
        // `user` in their dev dirs): last one wins, but say so.
        if let Some(existing) = index.namespaces.get(&meta.name) {
            if existing.file != meta.file {
                tracing::warn!(
                    "namespace {} defined in both {} and {}; last one wins",
                    meta.name,
                    existing.file.display(),
                    meta.file.display()
                );
            }
        }
        index.insert_file(meta, symbols, occurrences);
    }

    // Index keyword occurrences from Integrant/Aero EDN config files. Gated on
    // a `#ig/ref` tag so build manifests (deps.edn, bb.edn, shadow-cljs.edn)
    // are never indexed. EDN files are few, so this stays sequential.
    for file in collect_edn_files(roots) {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        if !extractor::is_integrant_edn(&file, &source) {
            continue;
        }
        let occurrences = extractor::extract_edn(&source);
        if !occurrences.is_empty() {
            index.insert_edn_file(file, occurrences);
        }
    }

    Ok(index)
}

/// Collects `.edn` files under the given source roots (Integrant configs live
/// in `:paths`/resources). Mirrors [`collect_clojure_files`].
fn collect_edn_files(roots: &[ScanRoot]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        if !root.path.exists() {
            continue;
        }
        for entry in scoped_walker(root) {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("edn") {
                files.push(path.to_path_buf());
            }
        }
    }
    files
}

/// The walker for one project source root: gitignore rules from the project
/// dir down apply (whether or not a `.git` exists), rules from above it do
/// not. The walk starts at the *project dir* — so the native walker chains
/// every gitignore from there down with real git precedence, negations
/// included — and prunes everything off the path to the source root.
/// `parents(false)` cuts the ancestry discovery above the project dir.
fn scoped_walker(root: &ScanRoot) -> ignore::Walk {
    // Normalize `..`/`.` away first: `:paths ["../shared/src"]` joins to
    // `project/../shared/src`, which lexically starts_with the project dir
    // while the walker's real paths never would — the filter must compare
    // clean paths. A target outside the project dir (that `../` case)
    // degrades to a plain scoped walk of itself.
    let target = normalize_lexically(&root.path);
    let project_dir = normalize_lexically(&root.project_dir);
    let walk_from = if target.starts_with(&project_dir) {
        project_dir
    } else {
        target.clone()
    };
    let mut builder = ignore::WalkBuilder::new(&walk_from);
    builder.parents(false).require_git(false);
    if walk_from != target {
        // Keep only the target subtree and the dirs leading down to it.
        builder.filter_entry(move |entry| {
            let path = entry.path();
            path.starts_with(&target) || target.starts_with(path)
        });
    }
    builder.build()
}

/// Removes `.` and `..` components lexically (no filesystem access, symlinks
/// untouched), so path comparisons see the same clean form the walker yields.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
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

/// Indexes explicit library source directories (e.g. resolved lgx deps).
/// Unlike [`index_classpath_libs`], it never splits out JARs or skips dirs
/// under the project root — every dir is a real dependency, including a
/// `:local/root` dep that happens to live inside the workspace. Paths are
/// canonicalized so navigation targets are clean absolute paths.
pub fn index_dir_libs(dirs: &[PathBuf], index: &Index) {
    for dir in dirs {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        index_classpath_dir(&dir, index);
    }
}

/// Indexes a library source directory from the classpath. No disk cache:
/// directories are cheap to walk and, unlike JARs, can change in place.
fn index_classpath_dir(dir: &Path, index: &Index) {
    let files = collect_clojure_files(&[ScanRoot {
        project_dir: dir.to_path_buf(),
        path: dir.to_path_buf(),
    }]);
    let results: Vec<(NsMeta, Vec<Symbol>)> = files
        .par_iter()
        .filter_map(|file| {
            let source = std::fs::read_to_string(file).ok()?;
            extractor::extract(&source, file).ok()
        })
        .collect();

    for (meta, mut symbols) in results {
        // `.impl`/`.internal` namespaces and private (`defn-`) symbols are all
        // indexed so navigation reaches library internals; completion hides
        // `.impl`/`.internal`, and workspace search is project-only. Same policy
        // as JAR indexing (see jar.rs).
        for sym in &mut symbols {
            sym.source = super::SymbolSource::Dir(dir.to_path_buf());
        }
        index.insert_lib_file(meta, symbols);
    }
}

fn index_classpath_jars(root: &Path, jars: Vec<PathBuf>, index: &Index) {
    let cache_dir = root.join(".clj-pulse").join("jar-cache");

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

fn collect_clojure_files(roots: &[ScanRoot]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        if !root.path.exists() {
            continue;
        }
        for entry in scoped_walker(root) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.is_file() && crate::config::is_clojure_source(path) {
                files.push(path.to_path_buf());
            }
        }
    }
    files
}

fn jar_mtime(jar: &Path) -> Option<u64> {
    jar_cache::jar_mtime(jar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A project living inside a workspace-gitignored dir must still scan (the
    /// workspace `.gitignore` stops applying at the project dir), while the
    /// project's *own* `.gitignore` keeps applying.
    #[test]
    fn scoped_scan_ignores_ancestry_above_the_project_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        fs::write(ws.join(".gitignore"), "repos/\n").unwrap();

        let project = ws.join("repos/foo");
        fs::create_dir_all(project.join("src/generated")).unwrap();
        fs::write(project.join(".gitignore"), "src/generated/\n").unwrap();
        fs::write(project.join("src/app.clj"), "(ns app)\n(defn go [] 1)\n").unwrap();
        fs::write(
            project.join("src/generated/gen.clj"),
            "(ns gen)\n(defn hidden [] 2)\n",
        )
        .unwrap();

        let roots = [ScanRoot {
            project_dir: project.clone(),
            path: project.join("src"),
        }];
        let index = build_index_scoped(&roots, &ExtractConfig::default()).unwrap();

        assert!(
            index.namespaces.contains_key("app"),
            "sources under a workspace-gitignored project dir must be scanned"
        );
        assert!(
            !index.namespaces.contains_key("gen"),
            "the project's own .gitignore must still apply"
        );
    }

    /// The gitignore *between* the project dir and the walk root applies too:
    /// walking `src` directly must still honor the project-root `.gitignore`.
    #[test]
    fn scoped_scan_applies_gitignores_between_project_dir_and_walk_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = tmp.path();
        fs::create_dir_all(project.join("src/skipme")).unwrap();
        fs::write(project.join(".gitignore"), "src/skipme/\n").unwrap();
        fs::write(project.join("src/app.clj"), "(ns app)\n").unwrap();
        fs::write(project.join("src/skipme/x.clj"), "(ns skipme.x)\n").unwrap();

        let roots = [ScanRoot {
            project_dir: project.to_path_buf(),
            path: project.join("src"),
        }];
        let index = build_index_scoped(&roots, &ExtractConfig::default()).unwrap();

        assert!(index.namespaces.contains_key("app"));
        assert!(
            !index.namespaces.contains_key("skipme.x"),
            "project-root .gitignore must apply when walking src directly"
        );
    }

    /// Gitignore negations chain with git precedence across levels: a deeper
    /// `.gitignore` re-including what the project-level one excluded wins.
    #[test]
    fn scoped_scan_honors_nested_gitignore_negations() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = tmp.path();
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join(".gitignore"), "keep.clj\ngone.clj\n").unwrap();
        fs::write(project.join("src/.gitignore"), "!keep.clj\n").unwrap();
        fs::write(project.join("src/keep.clj"), "(ns keep)\n").unwrap();
        fs::write(project.join("src/gone.clj"), "(ns gone)\n").unwrap();

        let roots = [ScanRoot {
            project_dir: project.to_path_buf(),
            path: project.join("src"),
        }];
        let index = build_index_scoped(&roots, &ExtractConfig::default()).unwrap();

        assert!(
            index.namespaces.contains_key("keep"),
            "deeper !negation must re-include the file"
        );
        assert!(
            !index.namespaces.contains_key("gone"),
            "non-negated project-level exclusion must hold"
        );
    }

    /// A parent-relative source root (`:paths ["../shared/src"]` joins to
    /// `project/../shared/src`) must still be scanned — the lexical `..` used
    /// to defeat the subtree filter and silently drop the whole root.
    #[test]
    fn scoped_scan_handles_parent_relative_source_roots() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        fs::create_dir_all(ws.join("app")).unwrap();
        fs::create_dir_all(ws.join("shared/src")).unwrap();
        fs::write(ws.join("shared/src/lib.clj"), "(ns lib)\n").unwrap();

        let roots = [ScanRoot {
            project_dir: ws.join("app"),
            path: ws.join("app/../shared/src"),
        }];
        let index = build_index_scoped(&roots, &ExtractConfig::default()).unwrap();

        assert!(
            index.namespaces.contains_key("lib"),
            "parent-relative source root must be scanned"
        );
    }
}
