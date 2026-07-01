//! Tier 1 (syntactic) semantic tokens: colors the lexical structure of a
//! buffer straight from the tree-sitter parse — comments, strings, regexes,
//! numbers, keywords, plus `#_` discard forms and `(comment …)` blocks rendered
//! as single grey spans. No name resolution (that is Tier 2). Reuses the
//! extractor's parser (`language()`) and UTF-16 position conversion
//! (`point_to_position`).

use tower_lsp::lsp_types::{SemanticToken, SemanticTokenType, SemanticTokensLegend};
use tree_sitter::{Node, Parser};

use crate::index::extractor;

/// Token types advertised in the legend, in legend order. A node maps to the
/// index of its type in this list (the `token_type` in the encoded stream), so
/// the `TYPE_*` constants below must stay in sync with these positions.
pub const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::COMMENT,
    SemanticTokenType::STRING,
    SemanticTokenType::REGEXP,
    SemanticTokenType::NUMBER,
    SemanticTokenType::KEYWORD,
];

const TYPE_COMMENT: u32 = 0;
const TYPE_STRING: u32 = 1;
const TYPE_REGEXP: u32 = 2;
const TYPE_NUMBER: u32 = 3;
const TYPE_KEYWORD: u32 = 4;

/// The semantic-tokens legend (types + modifiers) shared by the server
/// capability and the encoder. Tier 1 has no modifiers.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: LEGEND_TYPES.to_vec(),
        token_modifiers: Vec::new(),
    }
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

/// Parses `source` and collects lexical semantic tokens in document order.
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
    walk(tree.root_node(), source, &mut tokens);
    tokens
}

/// The legend index for a node that maps to a single token type, or `None` for
/// container nodes we recurse through. A `dis_expr` (`#_ form`) is a comment as
/// a whole — including stacked `#_ #_` and multi-line forms.
fn token_type_for(node: &Node) -> Option<u32> {
    match node.kind() {
        "comment" | "dis_expr" => Some(TYPE_COMMENT),
        "str_lit" | "char_lit" => Some(TYPE_STRING),
        "regex_lit" => Some(TYPE_REGEXP),
        "num_lit" => Some(TYPE_NUMBER),
        "kwd_lit" => Some(TYPE_KEYWORD),
        _ => None,
    }
}

/// Pre-order walk. On a tokenized node, emit its token(s) and **stop** — never
/// descend into it, so a `str_lit` inside a `#_` form (or a `kwd_ns` inside a
/// `kwd_lit`) is never re-tokenized. Otherwise recurse over named children.
fn walk(node: Node, source: &str, out: &mut Vec<AbsToken>) {
    if let Some(type_index) = token_type_for(&node) {
        push_node(node, source, type_index, out);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, source, out);
    }
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
    fn string_literal() {
        assert_eq!(tuples(r#""hi""#), vec![(0, 0, 4, TYPE_STRING)]);
    }

    #[test]
    fn char_literal_is_string_typed() {
        assert_eq!(tuples(r"\a"), vec![(0, 0, 2, TYPE_STRING)]);
    }

    #[test]
    fn multiline_string_splits_per_line_utf16() {
        // `"a<nl>bc"` -> line0 `"a` (2 units), line1 `bc"` (3 units).
        assert_eq!(
            tuples("\"a\nbc\""),
            vec![(0, 0, 2, TYPE_STRING), (1, 0, 3, TYPE_STRING)]
        );
    }

    #[test]
    fn regex_literal() {
        // `#"\d+"` is 6 UTF-16 units.
        assert_eq!(tuples(r##"#"\d+""##), vec![(0, 0, 6, TYPE_REGEXP)]);
    }

    #[test]
    fn number_literals_int_float_ratio() {
        assert_eq!(tuples("42"), vec![(0, 0, 2, TYPE_NUMBER)]);
        assert_eq!(tuples("3.14"), vec![(0, 0, 4, TYPE_NUMBER)]);
        assert_eq!(tuples("1/2"), vec![(0, 0, 3, TYPE_NUMBER)]);
    }

    #[test]
    fn keyword_plain_and_qualified_are_single_tokens() {
        assert_eq!(tuples(":foo"), vec![(0, 0, 4, TYPE_KEYWORD)]);
        // The whole `:ns/name` is one keyword token (never split into ns/name).
        assert_eq!(tuples(":ns/name"), vec![(0, 0, 8, TYPE_KEYWORD)]);
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
        // `"café"` -> quotes + café = 6 UTF-16 units.
        assert_eq!(tuples("\"café\""), vec![(0, 0, 6, TYPE_STRING)]);
        // `; →` -> `;`, space, arrow = 3 UTF-16 units.
        assert_eq!(tuples("; →"), vec![(0, 0, 3, TYPE_COMMENT)]);
    }

    #[test]
    fn legend_type_indices_match_constants() {
        let l = legend();
        assert_eq!(
            l.token_types[TYPE_COMMENT as usize],
            SemanticTokenType::COMMENT
        );
        assert_eq!(
            l.token_types[TYPE_STRING as usize],
            SemanticTokenType::STRING
        );
        assert_eq!(
            l.token_types[TYPE_REGEXP as usize],
            SemanticTokenType::REGEXP
        );
        assert_eq!(
            l.token_types[TYPE_NUMBER as usize],
            SemanticTokenType::NUMBER
        );
        assert_eq!(
            l.token_types[TYPE_KEYWORD as usize],
            SemanticTokenType::KEYWORD
        );
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
