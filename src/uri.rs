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

/// The editor URI for an index path: a `jar:` URI for a virtual JAR path, a
/// plain `file:` URI otherwise.
pub fn from_index_path(path: &Path) -> Result<Url> {
    let path_str = path.to_string_lossy();
    if let Some((jar_part, entry_part)) = split_jar_virtual_path(&path_str) {
        let jar_url = Url::from_file_path(jar_part)
            .map_err(|_| anyhow::anyhow!("invalid jar path: {}", jar_part))?;
        let jar_uri = format!("jar:{}!/{}", jar_url, entry_part);
        Url::parse(&jar_uri).map_err(|e| anyhow::anyhow!("invalid jar URI {}: {}", jar_uri, e))
    } else {
        Url::from_file_path(path).map_err(|_| anyhow::anyhow!("invalid path: {}", path_str))
    }
}

/// Splits an archive virtual path `<archive>.{jar,zip}!/<entry>` into its archive
/// and entry parts. These are built as `format!("{}!/{}", archive, entry)` (see
/// `jar::index_jar` for JARs and `jdk` for the JDK `src.zip`), so the boundary is
/// matched on `.jar!/` or `.zip!/`, whichever comes first. A real filesystem path
/// that merely contains `!/` (e.g. a directory named `work!`) has no such boundary
/// and stays a plain file path.
///
/// Limitation: a real path under a directory named literally `<name>.jar!` /
/// `<name>.zip!` is indistinguishable by string alone from an archive entry and is
/// treated as one. This is inherent to the `path!/entry` representation; resolving
/// it would require filesystem probing (which would break round-tripping for
/// archives not currently on disk) and is not worth it for so pathological a name.
fn split_jar_virtual_path(path: &str) -> Option<(&str, &str)> {
    // `.jar` and `.zip` are both 4 bytes, so one split offset serves both.
    let boundary = [".jar!/", ".zip!/"]
        .iter()
        .filter_map(|sep| path.find(sep))
        .min()?;
    let split = boundary + ".jar".len();
    Some((&path[..split], &path[split + "!/".len()..]))
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
    fn real_path_with_bang_slash_is_not_a_jar() {
        // A directory literally named `work!` contains `!/` but is not a JAR;
        // it must round-trip as a plain file URI, not a bogus jar: URI.
        let url = from_index_path(Path::new("/tmp/work!/app/core.clj")).unwrap();
        assert_eq!(url.as_str(), "file:///tmp/work!/app/core.clj");
    }

    #[test]
    fn jar_under_bang_dir_still_splits_at_archive() {
        // The `.jar!/` boundary is matched even when an ancestor dir ends in `!`.
        let url = from_index_path(Path::new("/tmp/w!/lib.jar!/mylib/util.clj")).unwrap();
        assert_eq!(url.as_str(), "jar:file:///tmp/w!/lib.jar!/mylib/util.clj");
    }

    #[test]
    fn src_zip_virtual_path_round_trips() {
        // The JDK `src.zip` uses the same `<archive>!/<entry>` representation as
        // JARs; the `.zip!/` boundary must split (and round-trip) the same way.
        let virtual_path = "/j/lib/src.zip!/java.base/java/lang/String.java";
        let url = from_index_path(Path::new(virtual_path)).unwrap();
        assert_eq!(
            url.as_str(),
            "jar:file:///j/lib/src.zip!/java.base/java/lang/String.java"
        );
        assert_eq!(to_index_path(&url), Some(PathBuf::from(virtual_path)));
    }

    #[test]
    fn round_trips_both_shapes() {
        for p in [
            "/a/b.clj",
            "/x.jar!/mylib/util.clj",
            "/j/lib/src.zip!/java.base/java/lang/String.java",
        ] {
            let path = PathBuf::from(p);
            let back = to_index_path(&from_index_path(&path).unwrap()).unwrap();
            assert_eq!(back, path);
        }
    }
}
