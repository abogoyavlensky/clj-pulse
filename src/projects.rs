//! Multi-project (monorepo) support: subproject detection, the per-project
//! config model, EDN/JSON parsing, and per-key merge into resolved projects.

use std::path::{Path, PathBuf};

/// Detects subproject directories under `root`: directories containing a
/// `deps.edn`, `project.clj`, or `lgx.edn`, excluding `root` itself. Paths are
/// returned relative to `root`, sorted. The walk respects `.gitignore` and is
/// capped at depth 4.
pub fn detect(root: &Path) -> Vec<PathBuf> {
    const MANIFESTS: [&str; 3] = ["deps.edn", "project.clj", "lgx.edn"];

    let mut found: Vec<PathBuf> = Vec::new();
    for entry in ignore::WalkBuilder::new(root).max_depth(Some(4)).build() {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if !path.is_dir() || path == root {
            continue;
        }
        if MANIFESTS.iter().any(|m| path.join(m).exists()) {
            if let Ok(rel) = path.strip_prefix(root) {
                found.push(rel.to_path_buf());
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const MANIFESTS: [&str; 3] = ["deps.edn", "project.clj", "lgx.edn"];

    /// Creates `dir` under `root` with the given manifest file.
    fn mk_project(root: &Path, dir: &str, manifest: &str) {
        let d = root.join(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(manifest), "{}").unwrap();
    }

    #[test]
    fn detect_finds_subprojects_for_each_manifest_kind() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        for (i, m) in MANIFESTS.iter().enumerate() {
            mk_project(root, &format!("apps/p{i}"), m);
        }

        assert_eq!(
            detect(root),
            vec![
                PathBuf::from("apps/p0"),
                PathBuf::from("apps/p1"),
                PathBuf::from("apps/p2"),
            ]
        );
    }

    #[test]
    fn detect_excludes_the_root_itself() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        assert!(detect(root).is_empty());
    }

    #[test]
    fn detect_finds_nested_subproject_at_depth_three() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        mk_project(root, "libs/group/common", "deps.edn");
        assert_eq!(detect(root), vec![PathBuf::from("libs/group/common")]);
    }

    #[test]
    fn detect_skips_dirs_past_depth_four() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // Manifest at depth 5 — the project dir is past the depth-4 cap.
        mk_project(root, "a/b/c/d/e", "deps.edn");
        // Depth 4 is still within the cap.
        mk_project(root, "a/b/c/d", "deps.edn");
        assert_eq!(detect(root), vec![PathBuf::from("a/b/c/d")]);
    }

    #[test]
    fn detect_honors_gitignore() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // gitignore rules only apply inside a git repo.
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        mk_project(root, "ignored/sub", "deps.edn");
        mk_project(root, "kept", "deps.edn");
        assert_eq!(detect(root), vec![PathBuf::from("kept")]);
    }

    #[test]
    fn detect_empty_repo_yields_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(detect(tmp.path()).is_empty());
    }
}
