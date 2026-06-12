use std::path::{Path, PathBuf};

/// Discovers classpath entries from the project's `.cpcache/` directory.
///
/// Tries `.cp` files newest-first and returns the entries of the first one
/// that still resolves (at least one absolute path exists on disk). Older
/// files are a fallback for stale caches — e.g. after an `~/.m2` cleanup or
/// when caches were created on another machine. Uses `std::env::split_paths`
/// for cross-platform parsing (`:` on Unix, `;` on Windows).
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let cpcache = root.join(".cpcache");
    if !cpcache.exists() {
        return vec![];
    }

    for cp_file in cp_files_newest_first(&cpcache) {
        let content = match std::fs::read_to_string(&cp_file) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to read {}: {}", cp_file.display(), e);
                continue;
            }
        };

        let mut entries: Vec<PathBuf> = Vec::new();
        // A .cp file only counts as current if at least one of its
        // *absolute* entries (a library path) still exists — relative
        // entries ("src") resolve under any project root and prove nothing.
        let mut has_lib_entry = false;
        for raw in std::env::split_paths(content.trim()) {
            let was_absolute = raw.is_absolute();
            // Relative entries are relative to the project root, not the
            // server process's cwd.
            let resolved = if was_absolute { raw } else { root.join(raw) };
            if resolved.exists() {
                has_lib_entry |= was_absolute;
                entries.push(resolved);
            }
        }

        if has_lib_entry {
            return entries;
        }
        tracing::debug!("skipping stale classpath file {}", cp_file.display());
    }

    vec![]
}

fn cp_files_newest_first(cpcache: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(cpcache) else {
        return vec![];
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "cp").unwrap_or(false))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .collect();
    files.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    files.into_iter().map(|(p, _)| p).collect()
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

    #[test]
    fn test_discover_falls_back_when_newest_cp_is_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let cpcache = root.join(".cpcache");
        fs::create_dir(&cpcache).unwrap();

        let lib = root.join("lib");
        fs::create_dir(&lib).unwrap();

        let sep = if cfg!(windows) { ";" } else { ":" };
        // Older .cp resolves; newest one references another machine's paths
        fs::write(
            cpcache.join("old.cp"),
            format!("src{}{}", sep, lib.display()),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            cpcache.join("new.cp"),
            format!("src{}/machine/gone/lib.jar", sep),
        )
        .unwrap();

        let result = discover(root);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], lib);
    }
}
