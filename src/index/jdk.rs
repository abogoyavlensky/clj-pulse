//! JDK source (`src.zip`) indexing for built-in Java navigation, hover,
//! completion, and signature help.
//!
//! Reads the JDK's bundled `.java` sources from `lib/src.zip` and parses them
//! with tree-sitter-java. Built-in (JDK) Java only — library `.class` bytecode
//! is out of scope (a later phase).

#[cfg(test)]
mod tests {
    use tree_sitter::{Node, Parser};
    use tree_sitter_language::LanguageFn;

    /// Confirms the tree-sitter-java grammar loads and parses against the
    /// pinned tree-sitter 0.25 — the one residual ABI risk from adding the
    /// dependency. Mirrors how `extractor::language()` builds a `Language` from
    /// a `LanguageFn`.
    #[test]
    fn parses_java() {
        let lang_fn: LanguageFn = tree_sitter_java::LANGUAGE;
        let language: tree_sitter::Language = lang_fn.into();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .expect("tree-sitter-java must load against tree-sitter 0.25");

        let tree = parser
            .parse("class A { static int f(int x) { return x; } }", None)
            .expect("parse");
        let root = tree.root_node();
        assert!(!root.has_error(), "fixture should parse cleanly");
        assert!(
            find_kind(root, "method_declaration"),
            "expected a method_declaration node"
        );
    }

    fn find_kind(node: Node, kind: &str) -> bool {
        if node.kind() == kind {
            return true;
        }
        (0..node.child_count()).any(|i| find_kind(node.child(i).unwrap(), kind))
    }
}
