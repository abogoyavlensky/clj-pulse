//! Translation between editor URIs and the paths the index keys on.
//!
//! Project and directory-library files use plain `file:` URIs whose paths are
//! real on-disk paths. Files inside JARs use `jar:file:///lib.jar!/entry.clj`
//! URIs, but the index keys them by a *virtual path* `lib.jar!/entry.clj`
//! (see `jar::index_jar`). These helpers convert between the two so handlers
//! can treat a JAR entry as the "current file" just like a project file.

use std::path::{Path, PathBuf};

use anyhow::Result;
use tower_lsp::lsp_types::Url;

use crate::jar_content;

/// The index path for an editor URI: a real path for `file:` URIs, the virtual
/// `jar.jar!/entry` path for `jar:` URIs. `None` when the URI is neither a
/// usable file URI nor a well-formed JAR URI.
pub fn to_index_path(uri: &Url) -> Option<PathBuf> {
    if uri.scheme() == "jar" {
        let (jar_path, entry) = jar_content::parse_jar_uri(uri.as_str()).ok()?;
        Some(PathBuf::from(format!("{}!/{}", jar_path.display(), entry)))
    } else {
        uri.to_file_path().ok()
    }
}

/// The editor URI for an index path: a `jar:` URI when the path is a virtual
/// JAR path (contains `!/`), a plain `file:` URI otherwise.
pub fn from_index_path(path: &Path) -> Result<Url> {
    let path_str = path.to_string_lossy();
    if let Some((jar_part, entry_part)) = path_str.split_once("!/") {
        let jar_url = Url::from_file_path(jar_part)
            .map_err(|_| anyhow::anyhow!("invalid jar path: {}", jar_part))?;
        let jar_uri = format!("jar:{}!/{}", jar_url, entry_part);
        Url::parse(&jar_uri).map_err(|e| anyhow::anyhow!("invalid jar URI {}: {}", jar_uri, e))
    } else {
        Url::from_file_path(path).map_err(|_| anyhow::anyhow!("invalid path: {}", path_str))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_to_index_path() {
        let uri = Url::parse("file:///a/b.clj").unwrap();
        assert_eq!(to_index_path(&uri), Some(PathBuf::from("/a/b.clj")));
    }

    #[test]
    fn jar_uri_to_virtual_path() {
        let uri = Url::parse("jar:file:///x.jar!/mylib/util.clj").unwrap();
        assert_eq!(
            to_index_path(&uri),
            Some(PathBuf::from("/x.jar!/mylib/util.clj"))
        );
    }

    #[test]
    fn real_path_to_file_uri() {
        let url = from_index_path(Path::new("/a/b.clj")).unwrap();
        assert_eq!(url.as_str(), "file:///a/b.clj");
    }

    #[test]
    fn virtual_path_to_jar_uri() {
        let url = from_index_path(Path::new("/x.jar!/mylib/util.clj")).unwrap();
        assert_eq!(url.as_str(), "jar:file:///x.jar!/mylib/util.clj");
    }

    #[test]
    fn round_trips_both_shapes() {
        for p in ["/a/b.clj", "/x.jar!/mylib/util.clj"] {
            let path = PathBuf::from(p);
            let back = to_index_path(&from_index_path(&path).unwrap()).unwrap();
            assert_eq!(back, path);
        }
    }
}
