use anyhow::{anyhow, Result};
use dashmap::DashMap;
use ropey::Rope;
use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent, Url};

pub struct DocumentStore {
    docs: DashMap<Url, Rope>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self {
            docs: DashMap::new(),
        }
    }

    pub fn open(&self, uri: Url, text: String) {
        self.docs.insert(uri, Rope::from_str(&text));
    }

    pub fn close(&self, uri: &Url) {
        self.docs.remove(uri);
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
                    let start_line = range.start.line as usize;
                    let start_char = range.start.character as usize;
                    let end_line = range.end.line as usize;
                    let end_char = range.end.character as usize;

                    let start_idx = rope.line_to_char(start_line) + start_char;
                    let end_idx = rope.line_to_char(end_line) + end_char;

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
        let line_text: String = line.chars().collect();
        let col = pos.character as usize;

        if col > line_text.len() {
            return None;
        }

        let chars: Vec<char> = line_text.chars().collect();

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

    pub fn line_text(&self, uri: &Url, line: u32) -> Option<String> {
        let rope = self.docs.get(uri)?;
        let line_idx = line as usize;
        if line_idx >= rope.len_lines() {
            return None;
        }
        Some(rope.line(line_idx).chars().collect())
    }
}

fn is_clj_ident_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '-' | '_' | '/' | '.' | '?' | '!' | '*' | '+' | '>' | '<' | '=' | '#' | '\'' | '&'
                | '%'
        )
}
