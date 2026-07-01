//! Tier 1 (syntactic) semantic tokens. Emits `comment`-typed tokens straight
//! from the tree-sitter parse for the comment forms an editor's TextMate grammar
//! can't handle: `#_` discard forms and `(comment …)` blocks — rendered as
//! single grey spans, including nested/multi-line — plus plain `;` line
//! comments, which ride the same token type. Strings, numbers, keywords, and
//! regexes are deliberately left to the grammar: emitting them would only
//! duplicate what it already does well and risk overriding correct coloring
//! mid-edit, for no gain. No name resolution — that is Tier 2. Reuses the
//! extractor's parser (`language()`) and UTF-16 position conversion
//! (`point_to_position`).

use anyhow::Result;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensLegend, SemanticTokensParams,
    SemanticTokensResult,
};
use tree_sitter::{Node, Parser};

use crate::document::DocumentStore;
use crate::index::extractor;

/// Token types advertised in the legend. Tier 1 emits only `comment` (for `#_`
/// discards, `(comment …)` blocks, and plain `;` line comments); every other
/// lexical category is left to the editor's grammar. The `TYPE_*` constant(s)
/// below index into this list. Tier 2 will extend it.
pub const LEGEND_TYPES: &[SemanticTokenType] = &[SemanticTokenType::COMMENT];

const TYPE_COMMENT: u32 = 0;

/// The semantic-tokens legend (types + modifiers) shared by the server
/// capability and the encoder. Tier 1 has no modifiers.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: LEGEND_TYPES.to_vec(),
        token_modifiers: Vec::new(),
    }
}

/// Answers `textDocument/semanticTokens/full`: reads the live document text and
/// returns delta-encoded Tier-1 tokens, or `Ok(None)` when the document is not
/// open. Purely syntactic (no `Index`) and non-panicking — a parse failure just
/// yields an empty token set from `compute_tokens`.
pub fn semantic_tokens_full(
    documents: &DocumentStore,
    params: SemanticTokensParams,
) -> Result<Option<SemanticTokensResult>> {
    let uri = params.text_document.uri;
    let Some(text) = documents.text(&uri) else {
        return Ok(None);
    };
    let data = encode(&compute_tokens(&text));
    Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data,
    })))
}

/// One absolute (non-delta) semantic token covering a single line. Multi-line
/// source nodes are split into one `AbsToken` per line by `push_node`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsToken {
    line: u32,
    start_char: u32,
    len: u32,
    type_index: u32,
}

/// Parses `source` and collects comment-form semantic tokens in document order.
/// Purely syntactic — no `Index`, no resolution. Never panics: a parser or
/// language failure yields an empty vec (same contract as `extractor::extract`).
pub fn compute_tokens(source: &str) -> Vec<AbsToken> {
    let mut parser = Parser::new();
    if parser.set_language(extractor::language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut tokens = Vec::new();
    walk(tree.root_node(), source, false, &mut tokens);
    tokens
}

/// The legend index for a node that maps to a token, or `None` for container
/// nodes we recurse through. Only comment forms are tokenized: a `comment` node
/// (plain `;` line comment) and a `dis_expr` (`#_ form`, as a whole — including
/// stacked `#_ #_` and multi-line). `(comment …)` blocks are handled in `walk`.
/// Strings, numbers, keywords, and regexes are intentionally not tokenized —
/// the grammar already colors them.
fn token_type_for(node: &Node) -> Option<u32> {
    match node.kind() {
        "comment" | "dis_expr" => Some(TYPE_COMMENT),
        _ => None,
    }
}

/// Pre-order walk. On a tokenized (comment-form) node, emit its token(s) and
/// **stop** — never descend into it, so the inner forms of a `#_` discard or a
/// `(comment …)` block are swallowed into the one grey span rather than tokenized
/// separately. Otherwise recurse over named children.
///
/// `quoted` marks a quoted-data context — a reader quote or syntax-quote (`'`,
/// `` ` ``) or a spelled-out `(quote …)` — where a `(comment …)` list is inert
/// data (the macro never runs) and must not be greyed. It gates only the
/// `(comment …)` heuristic; nothing else is tokenized, so quoted literals are
/// simply left to the editor's grammar.
///
/// Known limitation: an unquote (`~`/`~@`) inside a syntax-quote re-enters
/// evaluated code, but the flag is not cleared there, so a `(comment …)` in that
/// position is left ungreyed — a benign miss. Clearing it correctly would need
/// to distinguish hard from soft quote (unquote is literal under `'`), which is
/// out of scope for this syntactic Tier-1 heuristic.
fn walk(node: Node, source: &str, quoted: bool, out: &mut Vec<AbsToken>) {
    if let Some(type_index) = token_type_for(&node) {
        push_node(node, source, type_index, out);
        return;
    }
    if !quoted && node.kind() == "list_lit" && is_comment_form(node, source) {
        push_node(node, source, TYPE_COMMENT, out);
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

/// Emits one `AbsToken` per line the node spans — LSP tokens cannot cross
/// lines. The first segment starts at the node's start column; later segments
/// start at column 0. A trailing `\r` is stripped so CRLF files never color the
/// carriage return, and empty segments are skipped. Lengths are UTF-16 units.
fn push_node(node: Node, source: &str, type_index: u32, out: &mut Vec<AbsToken>) {
    let start = extractor::point_to_position(node.start_position(), node.start_byte(), source);
    let text = &source[node.start_byte()..node.end_byte()];
    for (i, segment) in text.split('\n').enumerate() {
        let segment = segment.strip_suffix('\r').unwrap_or(segment);
        let len = segment.encode_utf16().count() as u32;
        if len == 0 {
            continue;
        }
        let (line, start_char) = if i == 0 {
            (start.line, start.character)
        } else {
            (start.line + i as u32, 0)
        };
        out.push(AbsToken {
            line,
            start_char,
            len,
            type_index,
        });
    }
}

/// Delta-encodes absolute tokens into the LSP flat stream: each token becomes
/// `[Δline, Δstart_char, len, token_type, token_modifiers]` relative to the
/// previous token, with `start_char` measured absolutely whenever the line
/// advances. Sorts by `(line, start_char)` defensively — `compute_tokens`
/// already emits in order, but the encoder must not rely on it.
pub fn encode(tokens: &[AbsToken]) -> Vec<SemanticToken> {
    let mut ordered: Vec<&AbsToken> = tokens.iter().collect();
    ordered.sort_by_key(|t| (t.line, t.start_char));

    let mut out = Vec::with_capacity(ordered.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for t in ordered {
        let delta_line = t.line - prev_line;
        let delta_start = if delta_line == 0 {
            t.start_char - prev_start
        } else {
            t.start_char
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: t.len,
            token_type: t.type_index,
            token_modifiers_bitset: 0,
        });
        prev_line = t.line;
        prev_start = t.start_char;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flattens `compute_tokens` to `(line, start_char, len, type_index)` tuples
    /// for compact assertions.
    fn tuples(src: &str) -> Vec<(u32, u32, u32, u32)> {
        compute_tokens(src)
            .into_iter()
            .map(|t| (t.line, t.start_char, t.len, t.type_index))
            .collect()
    }

    #[test]
    fn line_comment_is_one_comment_token() {
        assert_eq!(tuples("; hello"), vec![(0, 0, 7, TYPE_COMMENT)]);
    }

    #[test]
    fn lexical_literals_are_left_to_the_grammar() {
        // Strings, chars, regexes, numbers, and keywords are deliberately NOT
        // tokenized — the editor's grammar already colors them. Only comment
        // forms produce tokens in Tier 1.
        for src in [
            r#""hi""#,     // str_lit
            r"\a",         // char_lit
            r##"#"\d+""##, // regex_lit
            "42",          // num_lit
            "3.14",
            "1/2",
            ":foo", // kwd_lit
            ":ns/name",
        ] {
            let ts = compute_tokens(src);
            assert!(ts.is_empty(), "{src:?} should not be tokenized: {ts:?}");
        }
    }

    #[test]
    fn discard_single_line_swallows_inner() {
        // `#_ 42` -> one comment span; the inner number is not tokenized.
        assert_eq!(tuples("#_ 42"), vec![(0, 0, 5, TYPE_COMMENT)]);
    }

    #[test]
    fn discard_multiline_swallows_inner_and_splits() {
        // `#_ (1<nl>2)` -> grey split over two lines; inner numbers swallowed.
        let ts = tuples("#_ (1\n2)");
        assert_eq!(ts, vec![(0, 0, 5, TYPE_COMMENT), (1, 0, 2, TYPE_COMMENT)]);
        assert!(ts.iter().all(|t| t.3 == TYPE_COMMENT));
    }

    #[test]
    fn stacked_discard_swallows_both_forms() {
        // `#_ #_ 1 2` discards two forms -> one comment span, no number tokens.
        let ts = tuples("#_ #_ 1 2");
        assert_eq!(ts, vec![(0, 0, 9, TYPE_COMMENT)]);
    }

    #[test]
    fn string_inside_discard_not_separately_tokenized() {
        assert_eq!(tuples(r#"#_ "x""#), vec![(0, 0, 6, TYPE_COMMENT)]);
    }

    #[test]
    fn non_ascii_lengths_are_utf16() {
        // `; café →` -> `;`, space, café (é = 1 UTF-16 unit), space, arrow
        // (1 UTF-16 unit) = 8 units, counted in UTF-16 rather than bytes.
        assert_eq!(tuples("; café →"), vec![(0, 0, 8, TYPE_COMMENT)]);
        // Non-ASCII inside a `#_` span is counted the same way: `#_ "café"`.
        assert_eq!(tuples("#_ \"café\""), vec![(0, 0, 9, TYPE_COMMENT)]);
    }

    /// Asserts `src` (single line) yields exactly one comment token over it.
    fn single_comment_over_all(src: &str) {
        let ts = compute_tokens(src);
        assert_eq!(ts.len(), 1, "expected one token for {src:?}: {ts:?}");
        let t = &ts[0];
        assert_eq!((t.line, t.start_char, t.type_index), (0, 0, TYPE_COMMENT));
        assert_eq!(t.len as usize, src.encode_utf16().count());
    }

    #[test]
    fn comment_form_is_one_span_swallowing_inner() {
        // Inner num_lit/kwd_lit must not be separately tokenized.
        single_comment_over_all("(comment (+ 1 2) :x)");
    }

    #[test]
    fn empty_comment_form_is_a_comment() {
        single_comment_over_all("(comment)");
    }

    #[test]
    fn qualified_comment_form_matches() {
        single_comment_over_all("(clojure.core/comment :x)");
    }

    #[test]
    fn multiline_comment_form_splits_per_line() {
        // `(comment<nl>  :x)` -> grey `(comment` then grey `  :x)`; no keyword.
        assert_eq!(
            tuples("(comment\n  :x)"),
            vec![(0, 0, 8, TYPE_COMMENT), (1, 0, 5, TYPE_COMMENT)]
        );
    }

    #[test]
    fn commentary_is_not_a_comment_form() {
        // Exact-name guard: `(commentary 1)` is live code, not a comment span.
        let ts = compute_tokens("(commentary 1)");
        assert!(ts.is_empty(), "should not be greyed: {ts:?}");
    }

    #[test]
    fn comment_foo_is_not_a_comment_form() {
        let ts = compute_tokens("(comment-foo 1)");
        assert!(ts.is_empty(), "should not be greyed: {ts:?}");
    }

    #[test]
    fn quoted_comment_list_is_data_not_a_comment() {
        // `'(comment 1 :x)` is quoted data: the macro never runs, so the list is
        // not greyed (its literals are left to the grammar → no tokens at all).
        let ts = compute_tokens("'(comment 1 :x)");
        assert!(
            ts.is_empty(),
            "quoted (comment …) must not be greyed: {ts:?}"
        );
    }

    #[test]
    fn syntax_quoted_comment_list_is_data_not_a_comment() {
        let ts = compute_tokens("`(comment 1)");
        assert!(
            ts.is_empty(),
            "syntax-quoted (comment …) must not be greyed: {ts:?}"
        );
    }

    #[test]
    fn special_form_quote_makes_comment_list_data() {
        // The spelled-out `(quote (comment 1))` is data, just like `'(comment 1)`.
        let ts = compute_tokens("(quote (comment 1))");
        assert!(
            ts.is_empty(),
            "(quote (comment …)) must not be greyed: {ts:?}"
        );
    }

    #[test]
    fn nested_unquoted_comment_form_is_still_a_comment() {
        // A real `(comment …)` nested in ordinary (unquoted) code is greyed, and
        // is the only token — the surrounding code is left to the grammar.
        let ts = tuples("(defn f [] (comment 1) 2)");
        assert_eq!(
            ts,
            vec![(0, 11, 11, TYPE_COMMENT)],
            "the nested (comment 1) should be the sole comment span: {ts:?}"
        );
    }

    #[test]
    fn legend_is_comment_only_with_no_modifiers() {
        let l = legend();
        assert_eq!(
            l.token_types[TYPE_COMMENT as usize],
            SemanticTokenType::COMMENT
        );
        // Tier 1 advertises exactly one type — no lexical types.
        assert_eq!(l.token_types.len(), 1);
        assert!(l.token_modifiers.is_empty());
    }

    #[test]
    fn encode_delta_same_line_and_line_advance() {
        let abs = vec![
            AbsToken {
                line: 0,
                start_char: 0,
                len: 7,
                type_index: 0,
            },
            AbsToken {
                line: 0,
                start_char: 10,
                len: 4,
                type_index: 1,
            },
            AbsToken {
                line: 2,
                start_char: 3,
                len: 2,
                type_index: 3,
            },
        ];
        let flat: Vec<u32> = encode(&abs)
            .iter()
            .flat_map(|t| {
                [
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    t.token_type,
                    t.token_modifiers_bitset,
                ]
            })
            .collect();
        assert_eq!(
            flat,
            vec![
                0, 0, 7, 0, 0, // first token, absolute
                0, 10, 4, 1, 0, // same line: delta_start = 10 - 0
                2, 3, 2, 3, 0, // line +2: delta_start resets to absolute 3
            ]
        );
    }

    #[test]
    fn encode_sorts_out_of_order_input() {
        let abs = vec![
            AbsToken {
                line: 1,
                start_char: 0,
                len: 2,
                type_index: 3,
            },
            AbsToken {
                line: 0,
                start_char: 5,
                len: 3,
                type_index: 4,
            },
        ];
        let enc = encode(&abs);
        assert_eq!((enc[0].delta_line, enc[0].delta_start), (0, 5));
        assert_eq!((enc[1].delta_line, enc[1].delta_start), (1, 0));
    }
}
