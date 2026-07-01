//! Locates the "ignored" Clojure forms an editor should dim: `#_` discard forms
//! and `(comment …)` blocks. Returns their whole-form ranges (multi-line
//! included) so a client can lay a decoration over them — a job semantic tokens
//! can't do, since they never override bracket-pair colorization. Plain `;` line
//! comments are excluded (the grammar already handles those). Purely syntactic —
//! no name resolution. Reuses the extractor's tree-sitter parser (`language()`)
//! and UTF-16 position conversion (`point_to_position`).

use tower_lsp::lsp_types::Range;
use tree_sitter::{Node, Parser};

use crate::index::extractor;

/// Collects the whole-form ranges of every `#_` discard form and `(comment …)`
/// block in `source`, in document order. Purely syntactic — no `Index`. Never
/// panics: a parser or language failure yields an empty vec. Plain `;` line
/// comments are excluded (the grammar handles those), and a `(comment …)` inside
/// quoted data is inert and excluded.
pub fn ignored_form_ranges(source: &str) -> Vec<Range> {
    let mut parser = Parser::new();
    if parser.set_language(extractor::language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(tree.root_node(), source, false, &mut out);
    out
}

/// Pre-order walk. A `#_` discard (`dis_expr`) is always an ignored form —
/// including stacked and multi-line — and matches regardless of quoting (it is a
/// reader-level discard). A `(comment …)` list is an ignored form only outside
/// quoted data. On a match, push the whole-node range and stop, so inner forms
/// are swallowed. `quoted` is set once inside a reader-quote/syntax-quote or a
/// spelled-out `(quote …)`, where a `(comment …)` list is inert data.
fn walk(node: Node, source: &str, quoted: bool, out: &mut Vec<Range>) {
    if node.kind() == "dis_expr" {
        out.push(node_range(node, source));
        return;
    }
    if !quoted && node.kind() == "list_lit" && is_comment_form(node, source) {
        out.push(node_range(node, source));
        return;
    }
    let quoted = quoted
        || matches!(node.kind(), "quoting_lit" | "syn_quoting_lit")
        || (node.kind() == "list_lit" && is_quote_form(node, source));
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, source, quoted, out);
    }
}

/// The node's whole span as an LSP range, with UTF-16 columns.
fn node_range(node: Node, source: &str) -> Range {
    Range {
        start: extractor::point_to_position(node.start_position(), node.start_byte(), source),
        end: extractor::point_to_position(node.end_position(), node.end_byte(), source),
    }
}

/// The head (operator-position) form of a `list_lit`, if it is a symbol.
/// Gap/metadata nodes (`comment`, `dis_expr`, `meta_lit`, `old_meta_lit`) are
/// skipped so the *first real form* is what's returned; `None` for an empty
/// list or one whose head is not a `sym_lit`.
fn head_symbol<'a>(list: Node<'a>) -> Option<Node<'a>> {
    for i in 0..list.named_child_count() {
        let child = list.named_child(i)?;
        if matches!(
            child.kind(),
            "comment" | "dis_expr" | "meta_lit" | "old_meta_lit"
        ) {
            continue;
        }
        return (child.kind() == "sym_lit").then_some(child);
    }
    None
}

/// A `list_lit` is a `(comment …)` block when its head is the symbol `comment`,
/// unqualified or `clojure.core/comment`. Purely syntactic (no resolution); the
/// exact-name match keeps `(commentary …)` and `(comment-foo …)` out.
fn is_comment_form(list: Node, source: &str) -> bool {
    let Some(head) = head_symbol(list) else {
        return false;
    };
    let Some(name) = head.child_by_field_name("name") else {
        return false;
    };
    if node_text(name, source) != "comment" {
        return false;
    }
    match head.child_by_field_name("namespace") {
        None => true,
        Some(ns) => node_text(ns, source) == "clojure.core",
    }
}

/// A `list_lit` is a spelled-out `(quote …)` when its head is the unqualified
/// symbol `quote` — the special form equivalent to the `'` reader macro, so its
/// body is data.
fn is_quote_form(list: Node, source: &str) -> bool {
    let Some(head) = head_symbol(list) else {
        return false;
    };
    let name_is_quote = head
        .child_by_field_name("name")
        .map(|n| node_text(n, source) == "quote")
        .unwrap_or(false);
    head.child_by_field_name("namespace").is_none() && name_is_quote
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flattens ranges to `(start_line, start_char, end_line, end_char)` tuples.
    fn ranges(src: &str) -> Vec<(u32, u32, u32, u32)> {
        ignored_form_ranges(src)
            .into_iter()
            .map(|r| (r.start.line, r.start.character, r.end.line, r.end.character))
            .collect()
    }

    #[test]
    fn discard_single_line() {
        // `#_ x` -> one range over the whole discard form.
        assert_eq!(ranges("#_ x"), vec![(0, 0, 0, 4)]);
    }

    #[test]
    fn discard_multiline_is_one_range() {
        // `#_ (a<nl>b)` -> a single range spanning both lines.
        assert_eq!(ranges("#_ (a\nb)"), vec![(0, 0, 1, 2)]);
    }

    #[test]
    fn discard_swallows_inner_forms() {
        // Stacked `#_ #_ 1 2` discards two forms -> one range, nothing nested.
        assert_eq!(ranges("#_ #_ 1 2"), vec![(0, 0, 0, 9)]);
    }

    #[test]
    fn comment_form_single_line() {
        // `(comment (+ 1 2))` -> one whole-list range.
        assert_eq!(ranges("(comment (+ 1 2))"), vec![(0, 0, 0, 17)]);
    }

    #[test]
    fn comment_form_multiline_is_one_range() {
        // `(comment<nl>  :x)` -> a single range spanning both lines.
        assert_eq!(ranges("(comment\n  :x)"), vec![(0, 0, 1, 5)]);
    }

    #[test]
    fn quoted_comment_forms_are_data() {
        // Reader-quote, syntax-quote, and the `(quote …)` special form all make
        // a `(comment …)` inert data -> no range.
        assert!(ranges("'(comment 1)").is_empty());
        assert!(ranges("`(comment 1)").is_empty());
        assert!(ranges("(quote (comment 1))").is_empty());
    }

    #[test]
    fn negative_guards_are_not_comment_forms() {
        assert!(ranges("(commentary 1)").is_empty());
        assert!(ranges("(comment-foo 1)").is_empty());
    }

    #[test]
    fn line_comments_and_literals_are_excluded() {
        // `;` line comments are the grammar's job; literals are never dimmed.
        assert!(ranges("; line comment").is_empty());
        assert!(ranges("42").is_empty());
    }
}
