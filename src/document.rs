use anyhow::{anyhow, Result};
use dashmap::DashMap;
use ropey::Rope;
use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent, Url};

pub struct DocumentStore {
    docs: DashMap<Url, Rope>,
    /// Latest LSP version per open document, used to discard superseded
    /// debounced diagnostic passes.
    versions: DashMap<Url, i32>,
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            docs: DashMap::new(),
            versions: DashMap::new(),
        }
    }
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, uri: Url, text: String) {
        self.docs.insert(uri, Rope::from_str(&text));
    }

    pub fn close(&self, uri: &Url) {
        self.docs.remove(uri);
        self.versions.remove(uri);
    }

    pub fn set_version(&self, uri: &Url, version: i32) {
        self.versions.insert(uri.clone(), version);
    }

    /// The latest recorded version for `uri`, if open.
    pub fn current_version(&self, uri: &Url) -> Option<i32> {
        self.versions.get(uri).map(|r| *r)
    }

    pub fn apply_changes(
        &self,
        uri: &Url,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Result<()> {
        let mut rope = self
            .docs
            .get_mut(uri)
            .ok_or_else(|| anyhow!("document not found: {}", uri))?;

        for change in changes {
            match change.range {
                Some(range) => {
                    let start_idx = position_to_char(&rope, range.start)
                        .ok_or_else(|| anyhow!("position out of range: {:?}", range.start))?;
                    let end_idx = position_to_char(&rope, range.end)
                        .ok_or_else(|| anyhow!("position out of range: {:?}", range.end))?;

                    rope.remove(start_idx..end_idx);
                    rope.insert(start_idx, &change.text);
                }
                None => {
                    *rope = Rope::from_str(&change.text);
                }
            }
        }

        Ok(())
    }

    pub fn word_at(&self, uri: &Url, pos: Position) -> Option<String> {
        let rope = self.docs.get(uri)?;
        let line_idx = pos.line as usize;
        if line_idx >= rope.len_lines() {
            return None;
        }

        let line_start = rope.line_to_char(line_idx);
        let line = rope.line(line_idx);
        let chars: Vec<char> = line.chars().collect();
        let col = utf16_col_to_char(&chars, pos.character as usize);

        if col > chars.len() {
            return None;
        }

        let mut start = col;
        while start > 0 && is_clj_ident_char(chars[start - 1]) {
            start -= 1;
        }

        let mut end = col;
        while end < chars.len() && is_clj_ident_char(chars[end]) {
            end += 1;
        }

        if start == end {
            return None;
        }

        let _ = line_start; // used for rope offset calculations if needed
        Some(chars[start..end].iter().collect())
    }

    /// Returns the full text of an open document.
    pub fn text(&self, uri: &Url) -> Option<String> {
        self.docs.get(uri).map(|rope| rope.to_string())
    }

    /// URIs of all currently open documents.
    pub fn open_uris(&self) -> Vec<Url> {
        self.docs.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Returns the document text from the start up to (not including) `pos`.
    pub fn text_up_to(&self, uri: &Url, pos: Position) -> Option<String> {
        let rope = self.docs.get(uri)?;
        let char_idx = position_to_char(&rope, pos)?;
        Some(rope.slice(..char_idx).to_string())
    }

    pub fn line_text(&self, uri: &Url, line: u32) -> Option<String> {
        let rope = self.docs.get(uri)?;
        let line_idx = line as usize;
        if line_idx >= rope.len_lines() {
            return None;
        }
        Some(rope.line(line_idx).chars().collect())
    }
}

/// Converts an LSP position (UTF-16 code units) to a rope char index.
/// Columns past the end of the line clamp to the line end.
fn position_to_char(rope: &Rope, pos: Position) -> Option<usize> {
    let line_idx = pos.line as usize;
    if line_idx >= rope.len_lines() {
        return None;
    }
    let chars: Vec<char> = rope.line(line_idx).chars().collect();
    let col = utf16_col_to_char(&chars, pos.character as usize);
    Some(rope.line_to_char(line_idx) + col)
}

/// Converts a UTF-16 column to a char offset within a line, clamping to
/// the line end.
fn utf16_col_to_char(chars: &[char], utf16_col: usize) -> usize {
    let mut units = 0;
    for (i, c) in chars.iter().enumerate() {
        if units >= utf16_col {
            return i;
        }
        units += c.len_utf16();
    }
    chars.len()
}

pub fn is_clj_ident_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '-' | '_'
                | '/'
                | '.'
                | '?'
                | '!'
                | '*'
                | '+'
                | '>'
                | '<'
                | '='
                | '#'
                | '\''
                | '&'
                | '%'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(text: &str) -> (DocumentStore, Url) {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.clj").unwrap();
        store.open(uri.clone(), text.to_string());
        (store, uri)
    }

    /// LSP positions are UTF-16 code units: '😀' is two units but one char.
    /// A single-char token makes any off-by-one shift miss the word.
    #[test]
    fn test_word_at_utf16_after_emoji() {
        let line = "(def s \"😀\") (f 1 2)";
        let (store, uri) = store_with(line);

        // Cursor on `f`, in UTF-16 units
        let prefix = "(def s \"😀\") (";
        let col = prefix.encode_utf16().count() as u32;
        let word = store.word_at(&uri, Position::new(0, col));
        assert_eq!(word.as_deref(), Some("f"));
    }

    #[test]
    fn test_apply_changes_utf16_after_emoji() {
        let (store, uri) = store_with("(str \"😀\")");
        // Insert right before the closing paren, position in UTF-16 units
        let col = "(str \"😀\"".encode_utf16().count() as u32;
        store
            .apply_changes(
                &uri,
                vec![TextDocumentContentChangeEvent {
                    range: Some(tower_lsp::lsp_types::Range {
                        start: Position::new(0, col),
                        end: Position::new(0, col),
                    }),
                    range_length: None,
                    text: " :x".to_string(),
                }],
            )
            .unwrap();
        assert_eq!(
            store.line_text(&uri, 0).unwrap().trim_end(),
            "(str \"😀\" :x)"
        );
    }

    #[test]
    fn test_text_up_to_utf16() {
        let (store, uri) = store_with("(str \"😀\") tail");
        let col = "(str \"😀\")".encode_utf16().count() as u32;
        let text = store.text_up_to(&uri, Position::new(0, col)).unwrap();
        assert_eq!(text, "(str \"😀\")");
    }
}
