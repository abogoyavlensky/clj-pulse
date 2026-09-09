//! `textDocument/selectionRange`: the "expand selection" chain for a position,
//! walked along the parse tree. Each step is the next enclosing form, so
//! repeated expansion goes symbol → binding vector → `let` → `defn`.

use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::extractor;

pub fn selection_ranges(
    documents: &DocumentStore,
    params: SelectionRangeParams,
) -> Result<Option<Vec<SelectionRange>>> {
    let Some(snapshot) = documents.snapshot(&params.text_document.uri) else {
        return Ok(None);
    };
    let root = snapshot.tree.root_node();
    // One chain per requested position: the response must line up with the
    // request element for element.
    let ranges = params
        .positions
        .iter()
        .map(|pos| chain_at(root, &snapshot.text, *pos))
        .collect();
    Ok(Some(ranges))
}

/// The expansion chain for one position, innermost range outermost parent.
fn chain_at(root: tree_sitter::Node, source: &str, pos: Position) -> SelectionRange {
    let path = extractor::node_path_at(root, source, pos);
    let mut ranges: Vec<Range> = Vec::new();

    // A qualified token expands through its name part first: `alias/na|me`
    // selects `name`, then `alias/name`. A cursor on the qualifier or on a
    // keyword's `::` marker skips this step — there is no namespace-only
    // selection to offer.
    if let Some(leaf) = path.first() {
        if matches!(leaf.kind(), "sym_lit" | "kwd_lit") {
            if let Some(name) = leaf.child_by_field_name("name") {
                let name_range = extractor::node_to_lsp_range(name, source);
                if contains(name_range, pos) {
                    ranges.push(name_range);
                }
            }
        }
    }
    ranges.extend(
        path.iter()
            .map(|node| extractor::node_to_lsp_range(*node, source)),
    );
    // A wrapper that spans exactly what it wraps (`^:private x`'s `meta_lit`,
    // say) would otherwise offer an expansion that selects nothing new.
    ranges.dedup();

    // Whitespace between top-level forms has no containing named node, but the
    // response still owes this position an entry.
    if ranges.is_empty() {
        ranges.push(Range {
            start: pos,
            end: pos,
        });
    }

    // Build outermost-in, so each step becomes the next one's parent.
    let mut chain: Option<Box<SelectionRange>> = None;
    for range in ranges.into_iter().rev() {
        chain = Some(Box::new(SelectionRange {
            range,
            parent: chain,
        }));
    }
    *chain.expect("chain is never empty")
}

/// Whether `range` contains `pos`, both ends inclusive — the same rule
/// `references::range_contains` uses, so a cursor resting on a token boundary
/// counts as inside it.
fn contains(range: Range, pos: Position) -> bool {
    let after_start = range.start.line < pos.line
        || (range.start.line == pos.line && range.start.character <= pos.character);
    let before_end = pos.line < range.end.line
        || (pos.line == range.end.line && pos.character <= range.end.character);
    after_start && before_end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chain at `pos` as the text each step selects, innermost first.
    fn steps(text: &str, line: u32, character: u32) -> Vec<String> {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.clj").unwrap();
        store.open(uri.clone(), text.to_string());
        let params = SelectionRangeParams {
            text_document: TextDocumentIdentifier { uri },
            positions: vec![Position { line, character }],
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let mut result = selection_ranges(&store, params).unwrap().unwrap();
        assert_eq!(result.len(), 1, "one chain per requested position");

        let mut out = Vec::new();
        let mut node = Some(Box::new(result.remove(0)));
        while let Some(step) = node {
            out.push(slice(text, step.range));
            node = step.parent;
        }
        out
    }

    /// The text `range` covers. ASCII-only, which every test snippet here is.
    fn slice(text: &str, range: Range) -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        if range.start.line == range.end.line {
            let line = lines[range.start.line as usize];
            return line[range.start.character as usize..range.end.character as usize].to_string();
        }
        let mut out =
            lines[range.start.line as usize][range.start.character as usize..].to_string();
        for line in &lines[range.start.line as usize + 1..range.end.line as usize] {
            out.push('\n');
            out.push_str(line);
        }
        out.push('\n');
        out.push_str(&lines[range.end.line as usize][..range.end.character as usize]);
        out
    }

    #[test]
    fn qualified_symbol_expands_from_its_name_part() {
        let text = "(ns app)\n(str/join x)";
        // Cursor inside `join`.
        assert_eq!(steps(text, 1, 6), vec!["join", "str/join", "(str/join x)"]);
    }

    #[test]
    fn cursor_on_the_qualifier_starts_at_the_whole_token() {
        let text = "(ns app)\n(str/join x)";
        // Cursor inside `str`: there is no namespace-only step.
        assert_eq!(steps(text, 1, 2), vec!["str/join", "(str/join x)"]);
    }

    #[test]
    fn keyword_marker_starts_at_the_whole_token() {
        let text = "(ns app)\n(get m ::k)";
        // Cursor on the `::` marker, before the name part.
        assert_eq!(steps(text, 1, 8), vec!["::k", "(get m ::k)"]);
    }

    #[test]
    fn nested_forms_expand_outward() {
        let text = "(defn f [x]\n  (let [y 1]\n    (+ x y)))";
        // Cursor on `y` in the body.
        let got = steps(text, 2, 9);
        assert_eq!(got[0], "y");
        assert_eq!(got[1], "(+ x y)");
        assert_eq!(got[2], "(let [y 1]\n    (+ x y))");
        assert_eq!(got[3], text);
        assert_eq!(got.len(), 4, "{:?}", got);
    }

    #[test]
    fn cursor_after_a_closing_paren_starts_at_the_enclosing_form() {
        let text = "(defn f [x]\n  (+ x 1))";
        // Just past the inner `)`: the cursor is no longer inside the form it
        // closed, so the chain starts at the form that encloses the position.
        let got = steps(text, 1, 9);
        assert_eq!(got, vec![text], "{:?}", got);
    }

    #[test]
    fn whitespace_between_forms_yields_one_empty_range() {
        let text = "(ns app)\n\n(defn f [] 1)";
        let got = steps(text, 1, 0);
        assert_eq!(got, vec![""], "{:?}", got);
    }
}
