use std::path::{Path, PathBuf};

/// Discovers classpath entries from the project's `.cpcache/` directory.
///
/// Reads the most recently modified `.cp` file in `.cpcache/` and returns
/// all entries that exist on disk. Uses `std::env::split_paths` for
/// cross-platform parsing (`:` on Unix, `;` on Windows).
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let cpcache = root.join(".cpcache");
    if !cpcache.exists() {
        return vec![];
    }

    let cp_file = match find_most_recent_cp(&cpcache) {
        Some(f) => f,
        None => return vec![],
    };

    let content = match std::fs::read_to_string(&cp_file) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to read {}: {}", cp_file.display(), e);
            return vec![];
        }
    };

    std::env::split_paths(content.trim())
        .filter(|p| p.exists())
        .collect()
}

fn find_most_recent_cp(cpcache: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(cpcache).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "cp").unwrap_or(false))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(path, _)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_discover_no_cpcache() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = discover(dir.path());
        assert!(result.is_empty());
    }

    #[test]
    fn test_discover_returns_existing_paths_filters_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        let cpcache = root.join(".cpcache");
        fs::create_dir(&cpcache).unwrap();

        // Create a real directory to put in the classpath
        let lib_dir = root.join("lib");
        fs::create_dir(&lib_dir).unwrap();

        // Use the OS path separator for the classpath
        let cp_content = if cfg!(windows) {
            format!("{};/nonexistent/path.jar", lib_dir.display())
        } else {
            format!("{}:/nonexistent/path.jar", lib_dir.display())
        };
        fs::write(cpcache.join("abc123.cp"), &cp_content).unwrap();

        let result = discover(root);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], lib_dir);
    }

    #[test]
    fn test_discover_picks_most_recent_cp() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let cpcache = root.join(".cpcache");
        fs::create_dir(&cpcache).unwrap();

        let lib1 = root.join("lib1");
        let lib2 = root.join("lib2");
        fs::create_dir(&lib1).unwrap();
        fs::create_dir(&lib2).unwrap();

        // Write two .cp files; the second will have a later mtime
        let sep = if cfg!(windows) { ";" } else { ":" };
        fs::write(
            cpcache.join("old.cp"),
            format!("{}{}{}", lib1.display(), sep, lib2.display()),
        )
        .unwrap();
        // Brief sleep to ensure different mtime
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(cpcache.join("new.cp"), lib2.display().to_string()).unwrap();

        let result = discover(root);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], lib2);
    }
}
