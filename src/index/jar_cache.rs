use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{NsMeta, Symbol};

/// Bump whenever the extractor or `Symbol`/`NsMeta` layout changes, so
/// caches written by older binaries are discarded (JAR mtimes never change,
/// so mtime alone cannot invalidate them).
pub const CACHE_FORMAT_VERSION: u32 = 4;

#[derive(Serialize, Deserialize)]
pub struct JarCacheEntry {
    pub format_version: u32,
    pub mtime: u64,
    pub namespaces: Vec<NsMeta>,
    pub symbols: Vec<Symbol>,
}

/// Loads a cached index entry for the given JAR, returning `None` if the
/// cache file doesn't exist, was written by a different binary version, or
/// the JAR's mtime has changed (stale).
pub fn load(cache_dir: &Path, jar: &Path) -> Option<JarCacheEntry> {
    let cache_file = cache_file_path(cache_dir, jar);
    let bytes = std::fs::read(&cache_file).ok()?;
    let entry: JarCacheEntry = bincode::deserialize(&bytes).ok()?;

    if entry.format_version != CACHE_FORMAT_VERSION {
        return None; // written by an incompatible binary
    }
    let current_mtime = jar_mtime(jar)?;
    if entry.mtime != current_mtime {
        return None; // stale
    }

    Some(entry)
}

/// Saves indexed data for a JAR to the cache directory.
pub fn save(cache_dir: &Path, jar: &Path, entry: &JarCacheEntry) -> Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let cache_file = cache_file_path(cache_dir, jar);
    let bytes = bincode::serialize(entry)?;
    std::fs::write(cache_file, bytes)?;
    Ok(())
}

pub fn jar_mtime(jar: &Path) -> Option<u64> {
    let meta = std::fs::metadata(jar).ok()?;
    let mtime = meta.modified().ok()?;
    let duration = mtime
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?;
    Some(duration.as_secs())
}

fn cache_file_path(cache_dir: &Path, jar: &Path) -> PathBuf {
    let hash = path_hash(jar);
    cache_dir.join(format!("{:016x}.bin", hash))
}

fn path_hash(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{DefKind, SymbolSource};
    use std::collections::HashMap;
    use tower_lsp::lsp_types::{Position, Range};

    fn make_range(line: u32, start: u32, end: u32) -> Range {
        Range {
            start: Position {
                line,
                character: start,
            },
            end: Position {
                line,
                character: end,
            },
        }
    }

    fn make_entry(mtime: u64, jar: &Path) -> JarCacheEntry {
        let ns_meta = NsMeta {
            name: "mylib.core".to_string(),
            file: PathBuf::from(format!("{}!/mylib/core.clj", jar.display())),
            aliases: HashMap::new(),
            refers: HashMap::new(),
        };
        let symbol = Symbol {
            name: "my-fn".to_string(),
            fqn: "mylib.core/my-fn".to_string(),
            ns: "mylib.core".to_string(),
            kind: DefKind::Defn,
            params: vec!["[x]".to_string()],
            doc: None,
            file: PathBuf::from(format!("{}!/mylib/core.clj", jar.display())),
            source: SymbolSource::Jar(jar.to_path_buf()),
            range: make_range(2, 0, 20),
            name_range: make_range(2, 5, 10),
        };
        JarCacheEntry {
            format_version: CACHE_FORMAT_VERSION,
            mtime,
            namespaces: vec![ns_meta],
            symbols: vec![symbol],
        }
    }

    #[test]
    fn test_cache_miss_wrong_format_version() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar = dir.path().join("lib.jar");
        std::fs::write(&jar, b"fake jar").unwrap();
        let mtime = jar_mtime(&jar).unwrap();

        let mut entry = make_entry(mtime, &jar);
        entry.format_version = CACHE_FORMAT_VERSION - 1;
        save(dir.path(), &jar, &entry).unwrap();

        assert!(load(dir.path(), &jar).is_none());
    }

    #[test]
    fn test_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar = dir.path().join("test.jar");
        std::fs::write(&jar, b"").unwrap();

        let mtime = jar_mtime(&jar).unwrap();
        let entry = make_entry(mtime, &jar);
        save(dir.path(), &jar, &entry).unwrap();

        let loaded = load(dir.path(), &jar).unwrap();
        assert_eq!(loaded.namespaces.len(), 1);
        assert_eq!(loaded.namespaces[0].name, "mylib.core");
        assert_eq!(loaded.symbols.len(), 1);
        assert_eq!(loaded.symbols[0].name, "my-fn");
    }

    #[test]
    fn test_cache_miss_stale_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar = dir.path().join("test.jar");
        std::fs::write(&jar, b"").unwrap();

        // Save with mtime=0 (clearly does not match the real mtime)
        let entry = make_entry(0, &jar);
        save(dir.path(), &jar, &entry).unwrap();

        assert!(load(dir.path(), &jar).is_none());
    }

    #[test]
    fn test_cache_hit() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar = dir.path().join("test.jar");
        std::fs::write(&jar, b"").unwrap();

        let mtime = jar_mtime(&jar).unwrap();
        let entry = make_entry(mtime, &jar);
        save(dir.path(), &jar, &entry).unwrap();

        assert!(load(dir.path(), &jar).is_some());
    }

    #[test]
    fn test_no_cache_file_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar = dir.path().join("nonexistent.jar");
        assert!(load(dir.path(), &jar).is_none());
    }
}
