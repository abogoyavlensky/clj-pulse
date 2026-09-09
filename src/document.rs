use std::sync::Mutex;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use ropey::Rope;
use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent, Url};
use tree_sitter::{InputEdit, Parser, Point, Tree};

use crate::index::extractor;

/// The keyword token under the cursor, as completion needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct KeywordContext {
    /// Whether the token is auto-resolved (`::name`, `::alias/name`) rather
    /// than plain (`:name`, `:ns/name`).
    pub auto_resolved: bool,
    /// What follows the marker up to the cursor — the text candidates are
    /// matched against. May contain `/` (`alias/na`), and is empty when the
    /// cursor sits right after a bare `:` or `::`.
    pub text: String,
    /// Position of the first `:` of the token.
    pub start: Position,
    /// End of the whole token, past the cursor when it sits mid-token.
    pub end: Position,
}

/// One open document's text and parse tree, taken under a single lock so the
/// two always describe the same version. Handlers work from this instead of
/// re-parsing: the tree is a reference-counted handle, so cloning it out of
/// the store is cheap, and `Tree` is `Send + Sync`, so it can travel to a
/// blocking thread.
pub struct Snapshot {
    /// The rope materialized once.
    pub text: String,
    pub tree: Tree,
}

impl Snapshot {
    /// A snapshot of `text` parsed on the spot, for callers with no document
    /// store (tests, mostly). `None` only if tree-sitter refuses to parse.
    pub fn parse(text: &str) -> Option<Self> {
        let tree = extractor::parse_tree(text)?;
        Some(Self {
            text: text.to_string(),
            tree,
        })
    }
}

/// The per-document state: the rope and the tree parsed from exactly its
/// current contents. `apply_changes` keeps them in step by editing the tree and
/// reparsing incrementally before it returns, so the tree never lags the rope.
struct Doc {
    rope: Rope,
    /// `None` only if tree-sitter refused to parse (it never does once the
    /// language is set); `snapshot` repairs that lazily.
    tree: Option<Tree>,
}

pub struct DocumentStore {
    docs: DashMap<Url, Doc>,
    /// Latest LSP version per open document, used to discard superseded
    /// debounced diagnostic passes.
    versions: DashMap<Url, i32>,
    /// Per-document count of lint triggers that supersede a pending pass. The
    /// version alone cannot tell a save from the edit just before it — both
    /// carry the same version — so a save bumps this, and the debounced change
    /// pass still waiting on that edit sees the mismatch and stands down.
    lint_epochs: DashMap<Url, u64>,
    /// One parser for every document. `Parser` is not `Sync`, and parsing an
    /// edit is milliseconds even on a very large buffer, so one shared parser
    /// behind a mutex costs nothing measurable.
    parser: Mutex<Parser>,
}

impl Default for DocumentStore {
    fn default() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(extractor::language())
            .expect("the bundled Clojure grammar matches this tree-sitter");
        Self {
            docs: DashMap::new(),
            versions: DashMap::new(),
            lint_epochs: DashMap::new(),
            parser: Mutex::new(parser),
        }
    }
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, uri: Url, text: String) {
        let rope = Rope::from_str(&text);
        let tree = self.parse(&rope, None);
        self.docs.insert(uri, Doc { rope, tree });
    }

    pub fn close(&self, uri: &Url) {
        self.docs.remove(uri);
        self.versions.remove(uri);
        self.lint_epochs.remove(uri);
    }

    pub fn set_version(&self, uri: &Url, version: i32) {
        self.versions.insert(uri.clone(), version);
    }

    /// The latest recorded version for `uri`, if open.
    pub fn current_version(&self, uri: &Url) -> Option<i32> {
        self.versions.get(uri).map(|r| *r)
    }

    /// Retires every lint pass still pending for `uri` and returns the epoch
    /// the next pass should carry. Called for each trigger that starts a pass
    /// of its own (an edit, a save).
    pub fn bump_lint_epoch(&self, uri: &Url) -> u64 {
        let mut epoch = self.lint_epochs.entry(uri.clone()).or_insert(0);
        *epoch += 1;
        *epoch
    }

    /// The current lint epoch for `uri`: a pass whose captured epoch differs
    /// has been superseded and must not publish.
    pub fn lint_epoch(&self, uri: &Url) -> u64 {
        self.lint_epochs.get(uri).map(|e| *e).unwrap_or(0)
    }

    /// Applies the editor's changes in order. Each incremental change is turned
    /// into a tree-sitter `InputEdit` against the rope *before* the change is
    /// applied to it, then the tree is told about it; a full-text change drops
    /// the tree instead. One incremental reparse at the end brings the tree
    /// back in step with the rope, so a reader never sees them disagree.
    pub fn apply_changes(
        &self,
        uri: &Url,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Result<()> {
        let mut doc = self
            .docs
            .get_mut(uri)
            .ok_or_else(|| anyhow!("document not found: {}", uri))?;
        let Doc { rope, tree } = &mut *doc;
        let mut old_tree = tree.take();

        for change in changes {
            match change.range {
                Some(range) => {
                    let start_idx = position_to_char(rope, range.start)
                        .ok_or_else(|| anyhow!("position out of range: {:?}", range.start))?;
                    let end_idx = position_to_char(rope, range.end)
                        .ok_or_else(|| anyhow!("position out of range: {:?}", range.end))?;
                    let edit = input_edit(rope, start_idx, end_idx, &change.text);

                    rope.remove(start_idx..end_idx);
                    rope.insert(start_idx, &change.text);
                    if let Some(tree) = old_tree.as_mut() {
                        tree.edit(&edit);
                    }
                }
                None => {
                    *rope = Rope::from_str(&change.text);
                    old_tree = None;
                }
            }
        }

        *tree = self.parse(rope, old_tree.as_ref());
        Ok(())
    }

    /// The text and tree of an open document, from one lock acquisition, so
    /// they are always the same version.
    pub fn snapshot(&self, uri: &Url) -> Option<Snapshot> {
        let mut doc = self.docs.get_mut(uri)?;
        if doc.tree.is_none() {
            doc.tree = self.parse(&doc.rope, None);
        }
        Some(Snapshot {
            text: doc.rope.to_string(),
            tree: doc.tree.clone()?,
        })
    }

    /// Parses the rope, incrementally against `old` when given, reading chunks
    /// straight out of the rope so no `String` is built.
    fn parse(&self, rope: &Rope, old: Option<&Tree>) -> Option<Tree> {
        let len = rope.len_bytes();
        let mut read = |byte: usize, _: Point| -> &[u8] {
            if byte >= len {
                return &[];
            }
            let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
            &chunk.as_bytes()[byte - chunk_start..]
        };
        self.parser
            .lock()
            .unwrap()
            .parse_with_options(&mut read, old, None)
    }

    pub fn word_at(&self, uri: &Url, pos: Position) -> Option<String> {
        let doc = self.docs.get(uri)?;
        let rope = &doc.rope;
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

    /// Whether the identifier token under `pos` is a Clojure keyword — i.e.
    /// immediately preceded by `:` (covers `:kw`, `::kw`, `:ns/kw`, `::ns/kw`).
    /// Used by goto-definition to avoid resolving a keyword to a same-named var.
    pub fn is_keyword_at(&self, uri: &Url, pos: Position) -> bool {
        self.keyword_at(uri, pos).is_some()
    }

    /// The keyword token being typed at `pos`, or `None` when the cursor is not
    /// inside one. Completion needs more than [`DocumentStore::is_keyword_at`]
    /// reports: which notation is being typed, what has been typed so far, and
    /// the span an accepted item replaces.
    pub fn keyword_at(&self, uri: &Url, pos: Position) -> Option<KeywordContext> {
        let doc = self.docs.get(uri)?;
        let rope = &doc.rope;
        let line_idx = pos.line as usize;
        if line_idx >= rope.len_lines() {
            return None;
        }
        let chars: Vec<char> = rope.line(line_idx).chars().collect();
        let col = utf16_col_to_char(&chars, pos.character as usize).min(chars.len());

        // Walk to the start of the ident token (same boundary rule as word_at),
        // then require the `:` (or `::`) that makes it a keyword.
        let mut start = col;
        while start > 0 && is_clj_ident_char(chars[start - 1]) {
            start -= 1;
        }
        if start == 0 || chars[start - 1] != ':' {
            return None;
        }
        let mut marker = start - 1;
        let auto_resolved = marker > 0 && chars[marker - 1] == ':';
        if auto_resolved {
            marker -= 1;
        }

        // The token may continue past the cursor (`:na|me`): an accepted item
        // replaces the whole of it, never just the half before the cursor.
        let mut end = col;
        while end < chars.len() && is_clj_ident_char(chars[end]) {
            end += 1;
        }

        Some(KeywordContext {
            auto_resolved,
            text: chars[start..col].iter().collect(),
            start: Position::new(pos.line, char_col_to_utf16(&chars, marker)),
            end: Position::new(pos.line, char_col_to_utf16(&chars, end)),
        })
    }

    /// Returns the full text of an open document. Callers that go on to parse
    /// it should take [`DocumentStore::snapshot`] instead.
    pub fn text(&self, uri: &Url) -> Option<String> {
        self.docs.get(uri).map(|doc| doc.rope.to_string())
    }

    /// URIs of all currently open documents.
    pub fn open_uris(&self) -> Vec<Url> {
        self.docs.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Returns the document text from the start up to (not including) `pos`.
    pub fn text_up_to(&self, uri: &Url, pos: Position) -> Option<String> {
        let doc = self.docs.get(uri)?;
        let rope = &doc.rope;
        let char_idx = position_to_char(rope, pos)?;
        Some(rope.slice(..char_idx).to_string())
    }

    pub fn line_text(&self, uri: &Url, line: u32) -> Option<String> {
        let doc = self.docs.get(uri)?;
        let rope = &doc.rope;
        let line_idx = line as usize;
        if line_idx >= rope.len_lines() {
            return None;
        }
        Some(rope.line(line_idx).chars().collect())
    }
}

/// The tree-sitter edit describing the replacement of chars `start..end` of
/// `rope` (as it is *before* the change) by `text`. Byte offsets come from the
/// rope's char-to-byte mapping; the new end is found by walking the inserted
/// text for newlines. Points use the rope's line numbering, which agrees with
/// tree-sitter's `\n` count for every line ending Clojure sources actually use.
fn input_edit(rope: &Rope, start: usize, end: usize, text: &str) -> InputEdit {
    let start_byte = rope.char_to_byte(start);
    let old_end_byte = rope.char_to_byte(end);
    let start_position = byte_to_point(rope, start_byte);
    let new_end_position = match text.rfind('\n') {
        Some(last_newline) => Point {
            row: start_position.row + text.matches('\n').count(),
            column: text.len() - last_newline - 1,
        },
        None => Point {
            row: start_position.row,
            column: start_position.column + text.len(),
        },
    };
    InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte: start_byte + text.len(),
        start_position,
        old_end_position: byte_to_point(rope, old_end_byte),
        new_end_position,
    }
}

/// A byte offset in `rope` as a tree-sitter point (row, byte column).
fn byte_to_point(rope: &Rope, byte: usize) -> Point {
    let row = rope.byte_to_line(byte);
    Point {
        row,
        column: byte - rope.line_to_byte(row),
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

/// Converts a char offset within a line to a UTF-16 column — the inverse of
/// [`utf16_col_to_char`], for ranges handed back to the editor.
fn char_col_to_utf16(chars: &[char], col: usize) -> u32 {
    chars[..col.min(chars.len())]
        .iter()
        .map(|c| c.len_utf16() as u32)
        .sum()
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

    /// The cursor position, in UTF-16 units, right after `needle` in `line`.
    fn after(line: &str, needle: &str) -> Position {
        let idx = line.find(needle).expect("needle not in line") + needle.len();
        Position::new(0, line[..idx].encode_utf16().count() as u32)
    }

    #[test]
    fn test_keyword_at_notations() {
        let line = "(f :id ::local :ns/x ::al/y)";
        let (store, uri) = store_with(line);

        let ctx = store.keyword_at(&uri, after(line, ":i")).unwrap();
        assert!(!ctx.auto_resolved);
        assert_eq!(ctx.text, "i");
        assert_eq!(ctx.start.character, 3);
        assert_eq!(ctx.end.character, 6, "end spans the whole `:id`");

        let ctx = store.keyword_at(&uri, after(line, "::loc")).unwrap();
        assert!(ctx.auto_resolved);
        assert_eq!(ctx.text, "loc");

        let ctx = store.keyword_at(&uri, after(line, ":ns/")).unwrap();
        assert!(!ctx.auto_resolved);
        assert_eq!(ctx.text, "ns/");

        let ctx = store.keyword_at(&uri, after(line, "::al/")).unwrap();
        assert!(ctx.auto_resolved);
        assert_eq!(ctx.text, "al/");
    }

    #[test]
    fn test_keyword_at_bare_markers_have_empty_text() {
        let line = "(f : ::)";
        let (store, uri) = store_with(line);

        let ctx = store.keyword_at(&uri, after(line, "(f :")).unwrap();
        assert!(!ctx.auto_resolved);
        assert_eq!(ctx.text, "");
        assert_eq!(ctx.start.character, 3, "start is the colon");
        assert_eq!(ctx.end.character, 4, "the colon itself is replaced");

        let ctx = store.keyword_at(&uri, after(line, "::")).unwrap();
        assert!(ctx.auto_resolved);
        assert_eq!(ctx.text, "");
        assert_eq!(ctx.start.character, 5, "start is the first colon");
    }

    #[test]
    fn test_keyword_at_mid_token_spans_whole_token() {
        // `:na|me`: only `na` is matched against, but the accepted item replaces
        // the whole `:name` — otherwise the buffer ends up with `:nameme`.
        let line = "(f :name)";
        let (store, uri) = store_with(line);

        let ctx = store.keyword_at(&uri, after(line, ":na")).unwrap();
        assert_eq!(ctx.text, "na");
        assert_eq!(ctx.start.character, 3);
        assert_eq!(ctx.end.character, 8);
    }

    #[test]
    fn test_keyword_at_rejects_non_keywords() {
        let line = "(inc x)";
        let (store, uri) = store_with(line);
        assert!(store.keyword_at(&uri, after(line, "(in")).is_none());
        // A bare `(` with nothing before it must not walk off the line start.
        assert!(store.keyword_at(&uri, Position::new(0, 0)).is_none());
    }

    #[test]
    fn test_keyword_at_utf16_after_emoji() {
        // LSP columns are UTF-16 units: the returned range has to be, too.
        let line = "(str \"😀\" :id)";
        let (store, uri) = store_with(line);

        let ctx = store.keyword_at(&uri, after(line, ":i")).unwrap();
        assert_eq!(ctx.text, "i");
        // `(str "😀" ` is ten UTF-16 units: six chars, the two-unit emoji, then
        // the quote and the space.
        assert_eq!(ctx.start.character, 10);
        assert_eq!(ctx.end.character, 13);
    }

    /// Every named node of the cached tree must sit where a fresh parse of the
    /// same text puts it: same structure *and* the same byte offsets and points.
    /// Structure alone would not prove the coordinates navigation and
    /// diagnostics consume, and a wrong `InputEdit` skews exactly those.
    fn assert_snapshot_matches_fresh_parse(store: &DocumentStore, uri: &Url, expected: &str) {
        let snap = store.snapshot(uri).expect("open document has a snapshot");
        assert_eq!(snap.text, expected, "snapshot text");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(crate::index::extractor::language())
            .unwrap();
        let fresh = parser.parse(&snap.text, None).unwrap();
        assert_eq!(
            snap.tree.root_node().to_sexp(),
            fresh.root_node().to_sexp(),
            "tree structure"
        );
        let cached = named_nodes(snap.tree.root_node());
        let expected_nodes = named_nodes(fresh.root_node());
        assert_eq!(cached, expected_nodes, "node coordinates");
    }

    /// `(kind, start_byte, end_byte, start_point, end_point)` for every named
    /// node in pre-order.
    fn named_nodes(
        root: tree_sitter::Node,
    ) -> Vec<(String, usize, usize, tree_sitter::Point, tree_sitter::Point)> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            out.push((
                node.kind().to_string(),
                node.start_byte(),
                node.end_byte(),
                node.start_position(),
                node.end_position(),
            ));
            let mut cursor = node.walk();
            let kids: Vec<_> = node.named_children(&mut cursor).collect();
            stack.extend(kids.into_iter().rev());
        }
        out
    }

    fn change(start: (u32, u32), end: (u32, u32), text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(tower_lsp::lsp_types::Range {
                start: Position::new(start.0, start.1),
                end: Position::new(end.0, end.1),
            }),
            range_length: None,
            text: text.to_string(),
        }
    }

    const SNAPSHOT_SRC: &str = "(ns my.app\n  (:require [clojure.string :as str]))\n\n(defn greet [name]\n  (str/join \" \" [\"hi\" name]))\n";

    #[test]
    fn snapshot_tree_matches_after_single_line_insert() {
        let (store, uri) = store_with(SNAPSHOT_SRC);
        assert_snapshot_matches_fresh_parse(&store, &uri, SNAPSHOT_SRC);
        store
            .apply_changes(&uri, vec![change((3, 17), (3, 17), " greeting")])
            .unwrap();
        let expected = SNAPSHOT_SRC.replace("[name]", "[name greeting]");
        assert_snapshot_matches_fresh_parse(&store, &uri, &expected);
    }

    #[test]
    fn snapshot_tree_matches_after_delete_across_lines() {
        let (store, uri) = store_with(SNAPSHOT_SRC);
        // From the end of `(ns my.app` through the require clause: the ns form
        // loses its second line entirely.
        store
            .apply_changes(&uri, vec![change((0, 10), (1, 37), "")])
            .unwrap();
        let expected = "(ns my.app)\n\n(defn greet [name]\n  (str/join \" \" [\"hi\" name]))\n";
        assert_snapshot_matches_fresh_parse(&store, &uri, expected);
    }

    #[test]
    fn snapshot_tree_matches_after_insert_with_newlines() {
        let (store, uri) = store_with(SNAPSHOT_SRC);
        store
            .apply_changes(
                &uri,
                vec![change((2, 0), (2, 0), "(def a 1)\n(def b\n  2)\n")],
            )
            .unwrap();
        let expected = SNAPSHOT_SRC.replacen("\n\n", "\n(def a 1)\n(def b\n  2)\n\n", 1);
        assert_snapshot_matches_fresh_parse(&store, &uri, &expected);
    }

    #[test]
    fn snapshot_tree_matches_after_edit_past_emoji() {
        // LSP columns are UTF-16 units; the tree edit needs bytes. An emoji is
        // two units but four bytes, so a naive conversion lands mid-token.
        let src = "(def s \"😀\") (f 1 2)\n";
        let (store, uri) = store_with(src);
        let col = "(def s \"😀\") (f 1".encode_utf16().count() as u32;
        store
            .apply_changes(&uri, vec![change((0, col), (0, col), "0")])
            .unwrap();
        assert_snapshot_matches_fresh_parse(&store, &uri, "(def s \"😀\") (f 10 2)\n");
    }

    #[test]
    fn snapshot_tree_matches_after_full_text_replacement() {
        let (store, uri) = store_with(SNAPSHOT_SRC);
        let replacement = "(ns other)\n(def x 1)\n";
        store
            .apply_changes(
                &uri,
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: replacement.to_string(),
                }],
            )
            .unwrap();
        assert_snapshot_matches_fresh_parse(&store, &uri, replacement);
    }

    #[test]
    fn snapshot_tree_matches_after_edit_sequence() {
        // Type a form character by character, mistype, back up, then delete a
        // whole line: the way an editor actually drives didChange.
        let (store, uri) = store_with(SNAPSHOT_SRC);
        let mut text = SNAPSHOT_SRC.to_string();
        let mut col = 0;
        for ch in "(defn shout [s] (str/upper-case s)".chars() {
            let s = ch.to_string();
            store
                .apply_changes(&uri, vec![change((5, col), (5, col), &s)])
                .unwrap();
            text.push_str(&s);
            col += 1;
        }
        // The buffer is temporarily unbalanced here; the tree must still track.
        assert_snapshot_matches_fresh_parse(&store, &uri, &text);
        store
            .apply_changes(&uri, vec![change((5, col), (5, col), "))")])
            .unwrap();
        text.push_str("))");
        assert_snapshot_matches_fresh_parse(&store, &uri, &text);
        store
            .apply_changes(&uri, vec![change((5, col + 1), (5, col + 2), "")])
            .unwrap();
        text.pop();
        assert_snapshot_matches_fresh_parse(&store, &uri, &text);
        // Two changes in one notification, applied in order.
        store
            .apply_changes(
                &uri,
                vec![
                    change((2, 0), (3, 0), ""),
                    change((0, 4), (0, 10), "renamed.app"),
                ],
            )
            .unwrap();
        let expected = text
            .replacen("\n\n(defn greet", "\n(defn greet", 1)
            .replacen("my.app", "renamed.app", 1);
        assert_snapshot_matches_fresh_parse(&store, &uri, &expected);
    }

    #[test]
    fn test_text_up_to_utf16() {
        let (store, uri) = store_with("(str \"😀\") tail");
        let col = "(str \"😀\")".encode_utf16().count() as u32;
        let text = store.text_up_to(&uri, Position::new(0, col)).unwrap();
        assert_eq!(text, "(str \"😀\")");
    }
}
