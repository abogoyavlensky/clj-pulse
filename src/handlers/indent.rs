//! Structural indent computation for indent-on-Enter (Tier A).
//!
//! The rule (Clojure Sublimed's default): `indent = column just after the
//! innermost unclosed open delimiter + offset`, where `offset = 1` iff the
//! delimiter is `(` / `#(` and the first form inside is a symbol. Vectors,
//! maps, sets and non-symbol-headed lists align to their first element; a
//! cursor inside a string/regex gets no indent (never change string content);
//! top level is column 0.
//!
//! The core is a hand-written scanner over the text *before* the cursor: a
//! single forward pass maintaining a stack of open delimiters, skipping
//! comments, strings, regexes and char literals. Prefix-only ⇒ robust to
//! unbalanced mid-edit code and independent of anything right of the cursor.

use tower_lsp::lsp_types::{DocumentOnTypeFormattingParams, Position, Range, TextEdit};

use crate::document::DocumentStore;

/// `textDocument/onTypeFormatting`, triggered on `\n`: one edit replacing the
/// new line's leading whitespace with the structural indent. Anything
/// unexpected (unknown document, in-string position, already-correct indent)
/// returns no edits — indentation is best-effort, never an error.
pub fn on_type_formatting(
    documents: &DocumentStore,
    params: DocumentOnTypeFormattingParams,
) -> anyhow::Result<Option<Vec<TextEdit>>> {
    if params.ch != "\n" {
        return Ok(None);
    }
    let pos = params.text_document_position.position;
    let uri = params.text_document_position.text_document.uri;
    let Some(text) = documents.text(&uri) else {
        return Ok(None);
    };
    let Some(indent) = indent_at(&text, pos) else {
        return Ok(None);
    };

    let line_start = position_to_byte(&text, Position::new(pos.line, 0));
    let line_end = text[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(text.len());
    let line = &text[line_start..line_end];
    let ws_len = line.len() - line.trim_start_matches([' ', '\t']).len();
    let desired = " ".repeat(indent as usize);
    if line[..ws_len] == desired {
        return Ok(None);
    }
    Ok(Some(vec![TextEdit {
        // Leading whitespace is spaces/tabs only, so bytes == UTF-16 units.
        range: Range::new(
            Position::new(pos.line, 0),
            Position::new(pos.line, ws_len as u32),
        ),
        new_text: desired,
    }]))
}

/// One unclosed open delimiter to the left of the cursor.
#[derive(Clone, Copy)]
struct Frame {
    /// UTF-16 column just after the opener token (after `(`, `[`, `#{`, …).
    col_after: u32,
    /// `(` / `#(` — the only frames eligible for the symbol-head offset.
    paren_like: bool,
    /// The closer that ends this frame; a mismatched closer (`]` against `(`)
    /// is ignored rather than popping the wrong frame — mid-edit code is
    /// routinely malformed and must not collapse the context to top level.
    closer: char,
    /// Whether the first form inside is a symbol; `None` until one is seen.
    first_form_symbol: Option<bool>,
}

/// Records the first form of the innermost open frame (no-op once seen).
fn mark_first_form(stack: &mut [Frame], is_symbol: bool) {
    if let Some(frame) = stack.last_mut() {
        if frame.first_form_symbol.is_none() {
            frame.first_form_symbol = Some(is_symbol);
        }
    }
}

/// Whether a token starting with `c` (followed by `next`) reads as a symbol.
/// Keywords, numbers, strings, quotes/derefs/metadata and reader macros do
/// not; `+`/`-` do unless they start a number literal (`-5`).
fn symbol_start(c: char, next: Option<char>) -> bool {
    match c {
        '+' | '-' => !matches!(next, Some(d) if d.is_ascii_digit()),
        '*' | '!' | '_' | '?' | '<' | '>' | '=' | '&' | '%' | '$' | '.' | '/' | '|' => true,
        c => c.is_alphabetic(),
    }
}

/// The target indent column (UTF-16 units) for a new line whose cursor sits at
/// `pos`, or `None` when the position is inside a string/regex (don't touch).
pub(crate) fn indent_at(source: &str, pos: Position) -> Option<u32> {
    let prefix = &source[..position_to_byte(source, pos)];
    let mut stack: Vec<Frame> = Vec::new();
    let mut in_string = false;
    let mut col: u32 = 0;
    let mut chars = prefix.chars().peekable();

    // `col` is always the column of the next unread char.
    fn bump(col: &mut u32, c: char) {
        if c == '\n' {
            *col = 0;
        } else {
            *col += c.len_utf16() as u32;
        }
    }

    while let Some(c) = chars.next() {
        bump(&mut col, c);
        match c {
            ' ' | '\t' | ',' | '\n' | '\r' => {}
            ';' => {
                for sc in chars.by_ref() {
                    bump(&mut col, sc);
                    if sc == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                mark_first_form(&mut stack, false);
                in_string = true;
                while let Some(sc) = chars.next() {
                    bump(&mut col, sc);
                    if sc == '\\' {
                        if let Some(esc) = chars.next() {
                            bump(&mut col, esc);
                        }
                    } else if sc == '"' {
                        in_string = false;
                        break;
                    }
                }
            }
            '\\' => {
                // Char literal: `\(`, `\newline`, `\\`. Consuming one char is
                // enough — any literal-name tail is made of plain ident chars.
                mark_first_form(&mut stack, false);
                if let Some(lit) = chars.next() {
                    bump(&mut col, lit);
                }
            }
            '#' => match chars.peek() {
                Some('(') => {
                    mark_first_form(&mut stack, false);
                    bump(&mut col, chars.next().unwrap());
                    stack.push(Frame {
                        col_after: col,
                        paren_like: true,
                        closer: ')',
                        first_form_symbol: None,
                    });
                }
                Some('{') => {
                    mark_first_form(&mut stack, false);
                    bump(&mut col, chars.next().unwrap());
                    stack.push(Frame {
                        col_after: col,
                        paren_like: false,
                        closer: '}',
                        first_form_symbol: None,
                    });
                }
                Some('"') => {
                    mark_first_form(&mut stack, false);
                    bump(&mut col, chars.next().unwrap());
                    in_string = true;
                    while let Some(sc) = chars.next() {
                        bump(&mut col, sc);
                        if sc == '\\' {
                            if let Some(esc) = chars.next() {
                                bump(&mut col, esc);
                            }
                        } else if sc == '"' {
                            in_string = false;
                            break;
                        }
                    }
                }
                Some('_') => {
                    // Discard: transparent for bracket balance, but it *is*
                    // the enclosing form's first form when leading (→ align).
                    mark_first_form(&mut stack, false);
                    bump(&mut col, chars.next().unwrap());
                }
                // #' #? #foo — reader macros; the wrapped form is not a
                // bare symbol head.
                _ => mark_first_form(&mut stack, false),
            },
            '(' => {
                mark_first_form(&mut stack, false);
                stack.push(Frame {
                    col_after: col,
                    paren_like: true,
                    closer: ')',
                    first_form_symbol: None,
                });
            }
            '[' | '{' => {
                mark_first_form(&mut stack, false);
                stack.push(Frame {
                    col_after: col,
                    paren_like: false,
                    closer: if c == '[' { ']' } else { '}' },
                    first_form_symbol: None,
                });
            }
            ')' | ']' | '}' => {
                if stack.last().is_some_and(|frame| frame.closer == c) {
                    stack.pop();
                }
            }
            c => {
                let is_symbol = symbol_start(c, chars.peek().copied());
                mark_first_form(&mut stack, is_symbol);
            }
        }
    }

    if in_string {
        return None;
    }
    match stack.last() {
        None => Some(0),
        Some(frame) => {
            let offset = u32::from(frame.paren_like && frame.first_form_symbol == Some(true));
            Some(frame.col_after + offset)
        }
    }
}

/// Byte offset of the LSP position (UTF-16 columns), clamping the column to
/// the line end and the line to the document end.
fn position_to_byte(source: &str, pos: Position) -> usize {
    let mut line_start = 0usize;
    for _ in 0..pos.line {
        match source[line_start..].find('\n') {
            Some(i) => line_start += i + 1,
            None => return source.len(),
        }
    }
    let line_end = source[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(source.len());

    let mut units = 0u32;
    let mut byte = line_start;
    for c in source[line_start..line_end].chars() {
        if units >= pos.character {
            break;
        }
        units += c.len_utf16() as u32;
        byte += c.len_utf8();
    }
    byte
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Indent for a cursor at the very end of `source` (the start of the new
    /// line just created by Enter).
    fn indent(source: &str) -> Option<u32> {
        let line = (source.split('\n').count() - 1) as u32;
        indent_at(source, Position::new(line, 0))
    }

    #[test]
    fn vector_aligns_to_first_element() {
        assert_eq!(indent("(let [a 1\n"), Some(6));
        assert_eq!(indent("[a 1\n"), Some(1));
        assert_eq!(indent("{:a 1\n"), Some(1));
        assert_eq!(indent("#{a\n"), Some(2));
    }

    #[test]
    fn symbol_headed_list_indents_two_spaces() {
        assert_eq!(indent("(when x\n"), Some(2));
        assert_eq!(indent("(foo bar\n"), Some(2));
        assert_eq!(indent("#(foo\n"), Some(3));
    }

    #[test]
    fn non_symbol_head_aligns() {
        assert_eq!(indent("((f)\n"), Some(1));
        assert_eq!(indent("(:k v\n"), Some(1));
        assert_eq!(indent("(1 2\n"), Some(1));
        assert_eq!(indent("(-5 3\n"), Some(1));
        assert_eq!(indent("(\"s\" x\n"), Some(1));
    }

    #[test]
    fn minus_symbol_head_is_a_symbol() {
        assert_eq!(indent("(- 5\n"), Some(2));
    }

    #[test]
    fn nested_uses_innermost_open_form() {
        assert_eq!(indent("(a (b c\n"), Some(5));
    }

    #[test]
    fn inside_string_or_regex_returns_none() {
        assert_eq!(indent("\"ab\n"), None);
        assert_eq!(indent("#\"ab\n"), None);
        assert_eq!(indent("(f \"ab\n"), None);
        // Trailing escape keeps the string open.
        assert_eq!(indent("\"ab\\\n"), None);
    }

    #[test]
    fn top_level_is_zero() {
        assert_eq!(indent("(foo)\n"), Some(0));
        assert_eq!(indent("(foo)\nbar\n"), Some(0));
        assert_eq!(indent("\n"), Some(0));
    }

    #[test]
    fn openers_in_skipped_constructs_do_not_count() {
        // ; comment
        assert_eq!(indent("; (a\n(foo x\n"), Some(2));
        // closed string containing a bracket
        assert_eq!(indent("\"(a\" x\n"), Some(0));
        // closed regex containing a bracket
        assert_eq!(indent("#\"(a\" (f\n"), Some(8));
        // char literal bracket
        assert_eq!(indent("(f \\( x\n"), Some(2));
        // char literal backslash followed by a real opener
        assert_eq!(indent("(f \\\\ (g\n"), Some(8));
    }

    #[test]
    fn discard_is_transparent_for_balance() {
        assert_eq!(indent("(foo #_(bar) baz\n"), Some(2));
        assert_eq!(indent("(foo #_(bar\n"), Some(9));
    }

    #[test]
    fn unmatched_closer_is_ignored() {
        assert_eq!(indent(")\n(foo x\n"), Some(2));
    }

    #[test]
    fn mismatched_closer_does_not_pop_the_open_frame() {
        // `]` must not close `(foo` — mid-edit buffers are routinely broken.
        assert_eq!(indent("(foo ]\n"), Some(2));
        // `)` against an open `[` is ignored; the vector stays innermost.
        assert_eq!(indent("(a [b ) c\n"), Some(4));
    }

    #[test]
    fn columns_are_utf16_units() {
        // 😀 is two UTF-16 units: ( f ␠ " 😀 😀 " ␠ ( g → opener ends at 9.
        assert_eq!(indent("(f \"😀\" (g\n"), Some(10));
    }

    #[test]
    fn position_to_byte_handles_utf16() {
        // é: 2 bytes, 1 UTF-16 unit → col 5 is `(` at byte 6.
        assert_eq!(position_to_byte("café (x\n", Position::new(0, 5)), 6);
        // →: 3 bytes, 1 unit.
        assert_eq!(position_to_byte("a→b\n", Position::new(0, 2)), 4);
        // 😀: 4 bytes, 2 units.
        assert_eq!(position_to_byte("😀x", Position::new(0, 2)), 4);
    }

    #[test]
    fn position_to_byte_clamps() {
        // Column past the line end clamps to the line end.
        assert_eq!(position_to_byte("ab\ncd", Position::new(0, 99)), 2);
        // Line past the document end clamps to the document end.
        assert_eq!(position_to_byte("ab\ncd", Position::new(9, 0)), 5);
        assert_eq!(position_to_byte("ab\ncd", Position::new(1, 1)), 4);
    }
}
