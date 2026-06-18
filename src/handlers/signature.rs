use anyhow::Result;
use tower_lsp::lsp_types::*;

use crate::document::DocumentStore;
use crate::index::Index;

use super::{resolve_symbol, ResolvedSymbol};

pub fn handle(
    index: &Index,
    documents: &DocumentStore,
    params: SignatureHelpParams,
) -> Result<Option<SignatureHelp>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let text = match documents.text_up_to(&uri, pos) {
        Some(t) => t,
        None => return Ok(None),
    };

    let (fn_word, active_arg) = match find_call_context(&text) {
        Some(ctx) => ctx,
        None => return Ok(None),
    };

    tracing::info!("signature_help: fn={} arg={}", fn_word, active_arg);

    let path = match crate::uri::to_index_path(&uri) {
        Some(p) => p,
        None => return Ok(None),
    };
    let current_ns = index.file_ns(&path).unwrap_or_default();

    let (name, arities, doc) = match resolve_symbol(index, &fn_word, &current_ns) {
        Some(ResolvedSymbol::Project(sym)) => (sym.name.clone(), sym.params.clone(), sym.doc),
        Some(ResolvedSymbol::Core(core)) => {
            let doc = if core.doc.is_empty() {
                None
            } else {
                Some(core.doc.clone())
            };
            (core.name.clone(), split_arity_list(&core.params), doc)
        }
        // Special forms / native core fns: no signature help here.
        Some(ResolvedSymbol::SpecialForm(_)) | Some(ResolvedSymbol::LetgoNative(_)) => {
            return Ok(None)
        }
        None => return Ok(None),
    };

    if arities.is_empty() {
        return Ok(None);
    }

    let signatures: Vec<SignatureInformation> = arities
        .iter()
        .map(|arity| {
            let params = split_params(arity);
            let label = if params.is_empty() {
                format!("({})", name)
            } else {
                format!("({} {})", name, params.join(" "))
            };
            SignatureInformation {
                label,
                documentation: doc.as_ref().map(|d| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: d.clone(),
                    })
                }),
                parameters: Some(
                    params
                        .iter()
                        .map(|p| ParameterInformation {
                            label: ParameterLabel::Simple(p.clone()),
                            documentation: None,
                        })
                        .collect(),
                ),
                active_parameter: None,
            }
        })
        .collect();

    let (active_signature, active_parameter) = pick_active(&arities, active_arg);

    Ok(Some(SignatureHelp {
        signatures,
        active_signature: Some(active_signature),
        active_parameter: Some(active_parameter),
    }))
}

/// Picks the arity that fits `active_arg` (first one with enough positional
/// params, or one with a rest param) and clamps the parameter index to it.
fn pick_active(arities: &[String], active_arg: u32) -> (u32, u32) {
    let mut fallback = arities.len().saturating_sub(1);
    for (i, arity) in arities.iter().enumerate() {
        let params = split_params(arity);
        let has_rest = params.iter().any(|p| p.starts_with('&'));
        if (active_arg as usize) < params.len() || has_rest {
            let clamped = (active_arg as usize).min(params.len().saturating_sub(1));
            return (i as u32, clamped as u32);
        }
        fallback = i;
    }
    let params = split_params(&arities[fallback]);
    let clamped = (active_arg as usize).min(params.len().saturating_sub(1));
    (fallback as u32, clamped as u32)
}

/// Finds the function call surrounding the cursor: scans the text before the
/// cursor tracking parens/strings/comments, returns the head symbol of the
/// innermost unclosed `(...)` and the index of the argument being typed.
pub fn find_call_context(text: &str) -> Option<(String, u32)> {
    struct Frame {
        kind: char,
        start: usize, // char index of the opening delimiter
        tokens: usize,
        in_token: bool,
    }

    let chars: Vec<char> = text.chars().collect();
    let mut stack: Vec<Frame> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut in_comment = false;

    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
                if let Some(f) = stack.last_mut() {
                    f.in_token = false;
                }
            }
            continue;
        }
        if in_comment {
            if c == '\n' {
                in_comment = false;
            }
            continue;
        }
        match c {
            '(' | '[' | '{' => {
                // A nested form is a single argument of its parent
                if let Some(f) = stack.last_mut() {
                    if !f.in_token {
                        f.tokens += 1;
                    }
                    f.in_token = false;
                }
                stack.push(Frame {
                    kind: c,
                    start: i,
                    tokens: 0,
                    in_token: false,
                });
            }
            ')' | ']' | '}' => {
                stack.pop();
                if let Some(f) = stack.last_mut() {
                    f.in_token = false;
                }
            }
            '"' => {
                if let Some(f) = stack.last_mut() {
                    if !f.in_token {
                        f.tokens += 1;
                    }
                    f.in_token = true;
                }
                in_string = true;
            }
            ';' => {
                in_comment = true;
                if let Some(f) = stack.last_mut() {
                    f.in_token = false;
                }
            }
            c if c.is_whitespace() || c == ',' => {
                if let Some(f) = stack.last_mut() {
                    f.in_token = false;
                }
            }
            _ => {
                if let Some(f) = stack.last_mut() {
                    if !f.in_token {
                        f.tokens += 1;
                    }
                    f.in_token = true;
                }
            }
        }
    }

    // Innermost unclosed list form
    let idx = stack.iter().rposition(|f| f.kind == '(')?;
    let frame = &stack[idx];
    if frame.tokens == 0 || (frame.tokens == 1 && frame.in_token) {
        // Cursor is before or still typing the head symbol
        return None;
    }

    // Head symbol: first token after the opening paren
    let mut j = frame.start + 1;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    let mut name = String::new();
    while j < chars.len() && crate::document::is_clj_ident_char(chars[j]) {
        name.push(chars[j]);
        j += 1;
    }
    if name.is_empty() {
        return None;
    }

    let consumed = frame.tokens - 1; // arguments seen, excluding the head
                                     // The cursor is inside an argument either mid-token or inside a nested
                                     // form ("(let [x 1…") that is still open above this frame.
    let inside_open_arg = frame.in_token || idx + 1 < stack.len();
    let active = if inside_open_arg {
        consumed.saturating_sub(1)
    } else {
        consumed
    };
    Some((name, active as u32))
}

/// Splits a params vector like `[a b & more]` or `[{:keys [x]} y]` into
/// top-level parameter strings (destructuring forms stay intact, `&` is
/// merged into its rest param).
fn split_params(arity: &str) -> Vec<String> {
    let inner = arity
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(arity);

    let mut params: Vec<String> = Vec::new();
    // Metadata/type hints belong to the following parameter: "^String s"
    // (and stacked "^:private ^String s") read as one parameter.
    let mut pending_meta = String::new();
    let push_token = |tok: String, params: &mut Vec<String>, pending: &mut String| {
        if tok.starts_with('^') {
            if !pending.is_empty() {
                pending.push(' ');
            }
            pending.push_str(&tok);
        } else if pending.is_empty() {
            params.push(tok);
        } else {
            params.push(format!("{} {}", pending, tok));
            pending.clear();
        }
    };

    let mut current = String::new();
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '[' | '{' | '(' => {
                depth += 1;
                current.push(c);
            }
            ']' | '}' | ')' => {
                depth -= 1;
                current.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    push_token(std::mem::take(&mut current), &mut params, &mut pending_meta);
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        push_token(current, &mut params, &mut pending_meta);
    }
    if !pending_meta.is_empty() {
        params.push(pending_meta);
    }

    // "& more" reads as one rest parameter
    let mut merged: Vec<String> = Vec::new();
    let mut iter = params.into_iter();
    while let Some(p) = iter.next() {
        if p == "&" {
            match iter.next() {
                Some(rest) => merged.push(format!("& {}", rest)),
                None => merged.push(p),
            }
        } else {
            merged.push(p);
        }
    }
    merged
}

/// Splits a core-symbol arity list like `([] [x] [x y & more])` into
/// individual arity vectors.
fn split_arity_list(params: &str) -> Vec<String> {
    let mut arities = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in params.chars() {
        match c {
            '[' => {
                depth += 1;
                current.push(c);
            }
            ']' => {
                depth -= 1;
                current.push(c);
                if depth == 0 {
                    arities.push(std::mem::take(&mut current));
                }
            }
            _ if depth > 0 => current.push(c),
            _ => {}
        }
    }
    arities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_call_context_first_arg() {
        assert_eq!(find_call_context("(map "), Some(("map".to_string(), 0)));
    }

    #[test]
    fn test_call_context_second_arg() {
        assert_eq!(find_call_context("(map inc "), Some(("map".to_string(), 1)));
    }

    #[test]
    fn test_call_context_typing_arg() {
        // Cursor inside the first argument still highlights param 0
        assert_eq!(find_call_context("(map in"), Some(("map".to_string(), 0)));
    }

    #[test]
    fn test_call_context_typing_head() {
        assert_eq!(find_call_context("(ma"), None);
        assert_eq!(find_call_context("("), None);
    }

    #[test]
    fn test_call_context_nested_form_is_one_arg() {
        assert_eq!(
            find_call_context("(map (fn [x] x) "),
            Some(("map".to_string(), 1))
        );
    }

    #[test]
    fn test_call_context_inside_nested_call() {
        assert_eq!(
            find_call_context("(map (assoc m "),
            Some(("assoc".to_string(), 1))
        );
    }

    #[test]
    fn test_call_context_ignores_parens_in_strings_and_comments() {
        assert_eq!(
            find_call_context("(str \"(((\" "),
            Some(("str".to_string(), 1))
        );
        assert_eq!(
            find_call_context("(map ; comment (((\n  inc "),
            Some(("map".to_string(), 1))
        );
    }

    #[test]
    fn test_call_context_qualified_symbol() {
        assert_eq!(
            find_call_context("(core/add 1 "),
            Some(("core/add".to_string(), 1))
        );
    }

    #[test]
    fn test_call_context_inside_vector_belongs_to_list_head() {
        assert_eq!(find_call_context("(let [x 1"), Some(("let".to_string(), 0)));
    }

    #[test]
    fn test_split_params_rest_and_destructuring() {
        assert_eq!(split_params("[a b & more]"), vec!["a", "b", "& more"]);
        assert_eq!(
            split_params("[{:keys [x y]} z]"),
            vec!["{:keys [x y]}", "z"]
        );
        assert!(split_params("[]").is_empty());
    }

    #[test]
    fn test_split_params_type_hints_and_metadata() {
        assert_eq!(split_params("[^String s]"), vec!["^String s"]);
        assert_eq!(
            split_params("[^String s ^long n]"),
            vec!["^String s", "^long n"]
        );
        // Stacked metadata stays with its parameter
        assert_eq!(
            split_params("[^:private ^String s]"),
            vec!["^:private ^String s"]
        );
        // Metadata map form
        assert_eq!(
            split_params("[^{:tag String} s x]"),
            vec!["^{:tag String} s", "x"]
        );
    }

    #[test]
    fn test_split_arity_list() {
        assert_eq!(
            split_arity_list("([] [x] [x y & more])"),
            vec!["[]", "[x]", "[x y & more]"]
        );
    }

    #[test]
    fn test_pick_active_multi_arity() {
        let arities = vec!["[x]".to_string(), "[x y]".to_string()];
        assert_eq!(pick_active(&arities, 0), (0, 0));
        assert_eq!(pick_active(&arities, 1), (1, 1));
        // Beyond all arities: clamp to last
        assert_eq!(pick_active(&arities, 5), (1, 1));
    }

    #[test]
    fn test_pick_active_rest_param() {
        let arities = vec!["[x & more]".to_string()];
        assert_eq!(pick_active(&arities, 4), (0, 1));
    }
}
