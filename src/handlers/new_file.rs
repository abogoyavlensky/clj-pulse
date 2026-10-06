//! `workspace/didCreateFiles`: the `ns` form an empty Clojure file created in
//! the editor gets, derived from its path relative to the project's source
//! roots (`src/foo/bar_baz.clj` → `(ns foo.bar-baz)`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::*;

/// The namespace a file at `path` declares by convention, relative to the
/// longest `roots` entry containing it; `None` outside every root, and for a
/// name no namespace can spell (`my file.clj`, `foo.bar.clj`) — no `ns` is
/// better than one the reader rejects or one that mismatches the path.
pub fn ns_for_path(path: &Path, roots: &[PathBuf]) -> Option<String> {
    let root = roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())?;
    let rel = path.strip_prefix(root).ok()?.with_extension("");
    let segments = rel
        .components()
        .map(|c| {
            let segment = c.as_os_str().to_str()?.replace('_', "-");
            is_ns_segment(&segment).then_some(segment)
        })
        .collect::<Option<Vec<_>>>()?;
    (!segments.is_empty()).then(|| segments.join("."))
}

/// Whether `segment` can stand between the dots of a namespace symbol.
fn is_ns_segment(segment: &str) -> bool {
    let Some(first) = segment.chars().next() else {
        return false;
    };
    !first.is_ascii_digit()
        && !segment
            .chars()
            .any(|c| c.is_whitespace() || "().[]{}\"',;@^`~\\#".contains(c))
}

/// The edit inserting `(ns <ns>)` at the top of the empty document `uri`.
pub fn ns_insert_edit(uri: Url, ns: &str) -> WorkspaceEdit {
    let at = Position::new(0, 0);
    let edit = TextEdit {
        range: Range::new(at, at),
        new_text: format!("(ns {ns})\n"),
    };
    WorkspaceEdit {
        changes: Some(HashMap::from([(uri, vec![edit])])),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(rel: &[&str]) -> Vec<PathBuf> {
        rel.iter().map(|r| Path::new("/p").join(r)).collect()
    }

    fn ns(path: &str, rel_roots: &[&str]) -> Option<String> {
        ns_for_path(&Path::new("/p").join(path), &roots(rel_roots))
    }

    #[test]
    fn source_and_test_files() {
        let r = &["src", "test"];
        assert_eq!(ns("src/foo/bar_baz.clj", r).as_deref(), Some("foo.bar-baz"));
        assert_eq!(
            ns("test/foo/bar_test.cljc", r).as_deref(),
            Some("foo.bar-test")
        );
        assert_eq!(ns("src/app/main.lg", r).as_deref(), Some("app.main"));
        assert_eq!(ns("src/core.cljs", r).as_deref(), Some("core"));
    }

    #[test]
    fn longest_root_wins() {
        let r = &["src", "src/main/clojure"];
        assert_eq!(ns("src/main/clojure/a/b.clj", r).as_deref(), Some("a.b"));
    }

    #[test]
    fn outside_every_root_or_the_root_itself() {
        let r = &["src", "test"];
        assert_eq!(ns("resources/x.clj", r), None);
        assert_eq!(ns("src", r), None);
    }

    #[test]
    fn names_that_cannot_be_a_namespace() {
        let r = &["src"];
        assert_eq!(ns("src/my file.clj", r), None);
        assert_eq!(ns("src/foo.bar.clj", r), None);
        assert_eq!(ns("src/1st/x.clj", r), None);
    }

    #[test]
    fn insert_edit_shape() {
        let uri = Url::parse("file:///p/src/foo/bar.clj").unwrap();
        let edit = ns_insert_edit(uri.clone(), "foo.bar");
        let edits = &edit.changes.unwrap()[&uri];
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(0, 0), Position::new(0, 0))
        );
        assert_eq!(edits[0].new_text, "(ns foo.bar)\n");
    }
}
