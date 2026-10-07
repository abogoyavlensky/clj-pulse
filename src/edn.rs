//! Thin typed accessors over `edn_format::Value`, shared by the `deps.edn`
//! and `lgx.edn` readers, plus the reader-noise masking (`prepare`) that
//! `deps.edn` and Leiningen's `project.clj` reading share.

use std::collections::BTreeMap;

use edn_format::{Keyword, Value};

/// A keyword value (`:name`).
pub(crate) fn kw(name: &str) -> Value {
    Value::Keyword(Keyword::from_name(name))
}

/// A namespaced keyword value (`:namespace/name`).
pub(crate) fn kw_ns(namespace: &str, name: &str) -> Value {
    Value::Keyword(Keyword::from_namespace_and_name(namespace, name))
}

/// Looks up `key` in an EDN map.
pub(crate) fn get(map: &BTreeMap<Value, Value>, key: Value) -> Option<&Value> {
    map.get(&key)
}

/// The string behind a `Value::String`, else `None`.
pub(crate) fn as_str(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// The strings of a `Value::Vector` at `key`. `None` when the key is absent
/// or its value is not a vector; non-string elements are skipped.
pub(crate) fn str_vec_at(map: &BTreeMap<Value, Value>, key: Value) -> Option<Vec<String>> {
    match get(map, key)? {
        Value::Vector(v) => Some(v.iter().filter_map(as_str).map(str::to_string).collect()),
        _ => None,
    }
}

/// Builds two same-length char views of `src`:
///
/// - **locator** — string/comment/char-literal contents *and* reader-discarded
///   (`#_`) forms blanked to spaces, so keywords and delimiters can be located
///   without matching ones that hide in strings, comments, or disabled config.
/// - **parse_buf** — only comments and discarded forms blanked; string contents
///   are preserved. This is the text actually handed to `edn_format`, which
///   rejects `#_` discards inside a vector (it fails the whole parse), so they
///   must be stripped first while keeping the real version/path strings.
///
/// Both are indexed identically, so a position found in `locator` slices
/// `parse_buf` at the same offset.
pub(crate) fn prepare(src: &str) -> (Vec<char>, Vec<char>) {
    let original: Vec<char> = src.chars().collect();
    let mut locator = original.clone();
    let mut parse_buf = original.clone();
    let mut i = 0;
    // Single pass: blank string/comment/char-literal content in `locator`, and
    // comment content in `parse_buf` (edn handles `;` but it is cleaner gone).
    let mut in_string = false;
    let mut in_comment = false;
    while i < original.len() {
        let c = original[i];
        if in_comment {
            if c == '\n' {
                in_comment = false;
            } else {
                locator[i] = ' ';
                parse_buf[i] = ' ';
            }
        } else if in_string {
            if c == '\\' {
                locator[i] = ' ';
                if i + 1 < original.len() {
                    locator[i + 1] = ' ';
                    i += 1;
                }
            } else if c == '"' {
                in_string = false; // keep the closing quote in `locator`
            } else {
                locator[i] = ' '; // blank string content in `locator` only
            }
        } else if c == '"' {
            in_string = true; // keep the opening quote in `locator`
        } else if c == ';' {
            locator[i] = ' ';
            parse_buf[i] = ' ';
            in_comment = true;
        } else if c == '\\' {
            // Character literal (`\[`, `\;`, …): blank in `locator` so it is
            // never mistaken for a delimiter or comment.
            locator[i] = ' ';
            if i + 1 < original.len() {
                locator[i + 1] = ' ';
                i += 1;
            }
        }
        i += 1;
    }

    // Blank `#_`-discarded forms in both views (found on `locator`, where
    // strings are already neutralized so brackets/quotes inside them can't
    // throw off form scanning).
    for (start, end) in discard_ranges(&locator) {
        for k in start..end {
            locator[k] = ' ';
            parse_buf[k] = ' ';
        }
    }

    (locator, parse_buf)
}

/// Spans of every `#_ <form>` reader-discard in `loc` (a string/comment-masked
/// buffer), each from the `#` through the end of the form it discards.
fn discard_ranges(loc: &[char]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i + 1 < loc.len() {
        if loc[i] == '#' && loc[i + 1] == '_' {
            let end = discard_end(loc, i);
            ranges.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    ranges
}

/// Index just past what the `#_` at `start` discards. A discard right after
/// it discards its own form first, so `#_#_ k v` drops both `k` and `v`.
fn discard_end(loc: &[char], start: usize) -> usize {
    let mut j = start + 2;
    while j < loc.len() && loc[j].is_whitespace() {
        j += 1;
    }
    if j + 1 < loc.len() && loc[j] == '#' && loc[j + 1] == '_' {
        form_end(loc, discard_end(loc, j))
    } else {
        form_end(loc, j)
    }
}

/// Index just past the next EDN form starting at/after `start` in `loc`.
/// Handles a bracketed collection (balanced, skipping masked strings), a
/// string, or a bare atom.
fn form_end(loc: &[char], start: usize) -> usize {
    let mut i = start;
    while i < loc.len() && loc[i].is_whitespace() {
        i += 1;
    }
    if i >= loc.len() {
        return i;
    }
    match loc[i] {
        '(' | '[' | '{' => {
            let mut depth = 0usize;
            let mut in_str = false;
            while i < loc.len() {
                match loc[i] {
                    '"' => in_str = !in_str,
                    '(' | '[' | '{' if !in_str => depth += 1,
                    ')' | ']' | '}' if !in_str => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            i
        }
        '"' => {
            i += 1;
            while i < loc.len() && loc[i] != '"' {
                i += 1;
            }
            (i + 1).min(loc.len())
        }
        _ => {
            while i < loc.len() && !loc[i].is_whitespace() && !matches!(loc[i], ')' | ']' | '}') {
                i += 1;
            }
            i
        }
    }
}

/// Parses a whole EDN file the way the Clojure reader would see its data:
/// `#_` discards and `^` metadata (`^:antq/exclude`, `^{:protect false}`)
/// are blanked first, since `edn_format` rejects both and would fail the
/// whole file over one annotated dependency. Strings and comments are left
/// alone, so a `^` or `#_` inside them changes nothing.
pub(crate) fn parse_lenient(src: &str) -> Option<Value> {
    let (locator, mut parse_buf) = prepare(src);
    let mut i = 0;
    while i < locator.len() {
        if locator[i] == '^' {
            let end = form_end(&locator, i + 1);
            for c in &mut parse_buf[i..end] {
                *c = ' ';
            }
            i = end;
        } else {
            i += 1;
        }
    }
    edn_format::parse_str(&parse_buf.iter().collect::<String>()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lenient_reads_past_metadata_and_discards_only() {
        let src = r#"{:a "x ^y #_z" :b ^:m ^{:n 1} [1] #_#_:c 2 :d #_[3] 4}"#;
        let Some(Value::Map(map)) = parse_lenient(src) else {
            panic!("expected a map");
        };
        assert_eq!(get(&map, kw("a")).and_then(as_str), Some("x ^y #_z"));
        assert_eq!(
            get(&map, kw("b")),
            Some(&Value::Vector(vec![Value::Integer(1)]))
        );
        assert_eq!(get(&map, kw("c")), None);
        assert_eq!(get(&map, kw("d")), Some(&Value::Integer(4)));
        // Still `None` for EDN that is broken for real.
        assert!(parse_lenient("{:a").is_none());
    }

    #[test]
    fn str_vec_at_reads_string_vector() {
        let Value::Map(map) = edn_format::parse_str(r#"{:paths ["a" "b"] :n 1}"#).unwrap() else {
            panic!("expected a map");
        };
        assert_eq!(
            str_vec_at(&map, kw("paths")),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        // Missing key.
        assert_eq!(str_vec_at(&map, kw("missing")), None);
        // Present but not a vector.
        assert_eq!(str_vec_at(&map, kw("n")), None);
    }
}
