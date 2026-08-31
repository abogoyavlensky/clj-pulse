//! The clj-kondo boundary: config compatibility and the diagnostics bridge.
//!
//! Two jobs live here. The first is configuration compatibility: reading the
//! subset of `.clj-kondo/config.edn` that clj-pulse understands (only
//! `:lint-as`, the map clj-kondo uses to treat a custom macro like a known one,
//! `{defcomponent/defcomponent clojure.core/def}`). We read the project's
//! `.clj-kondo/config.edn` only - the `config/` directory, JAR-exported
//! configs, and the `~/.config` global are not consulted yet. That half returns
//! raw `(macro-fqn, target-fqn)` string pairs and makes no decision about which
//! targets are meaningful; `settings` owns the merge and the mapping to
//! `DefKind`.
//!
//! The second is the diagnostics bridge: turning clj-kondo's JSON findings into
//! LSP diagnostics. Everything that knows clj-kondo's wire format lives here, so
//! the rest of the server sees only `Vec<Diagnostic>`.

use std::path::Path;

use edn_format::Value;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString, Range};

use crate::edn::{get, kw};

/// A `Value::Symbol` rendered as `ns/name` (or `name` when unqualified), else
/// `None` for any non-symbol value.
fn sym_to_string(value: &Value) -> Option<String> {
    let Value::Symbol(sym) = value else {
        return None;
    };
    Some(match sym.namespace() {
        Some(ns) => format!("{}/{}", ns, sym.name()),
        None => sym.name().to_string(),
    })
}

/// Parses the top-level `:lint-as` map of a clj-kondo config, returning each
/// `(macro-fqn, target-fqn)` pair as fully-qualified symbol strings. Returns an
/// empty vec when the input is not a map, has no `:lint-as` map, or that map is
/// empty; non-symbol keys/values are skipped.
pub(crate) fn parse_lint_as(edn: &str) -> Vec<(String, String)> {
    let Ok(Value::Map(top)) = edn_format::parse_str(edn) else {
        return vec![];
    };
    let Some(Value::Map(map)) = get(&top, kw("lint-as")) else {
        return vec![];
    };
    map.iter()
        .filter_map(|(k, v)| Some((sym_to_string(k)?, sym_to_string(v)?)))
        .collect()
}

/// Reads `root/.clj-kondo/config.edn` and returns its `:lint-as` pairs. Missing
/// or unparseable files yield an empty vec - clj-kondo config is optional.
pub(crate) fn lint_as(root: &Path) -> Vec<(String, String)> {
    let path = root.join(".clj-kondo").join("config.edn");
    match std::fs::read_to_string(&path) {
        Ok(src) => parse_lint_as(&src),
        Err(_) => vec![],
    }
}

/// The `source` every diagnostic bridged from clj-kondo carries, so editors
/// (and our own ownership merge) can tell them from clj-pulse's native lints.
pub const SOURCE: &str = "clj-kondo";

/// Parses clj-kondo's `{:output {:format :json}}` stdout into LSP diagnostics.
///
/// `None` means "this is not clj-kondo output" — an empty buffer, a Java stack
/// trace, JSON without a `findings` array. Callers treat that as a failed run
/// and keep the native diagnostics. `Some(vec![])` is a *successful* clean
/// lint, and does cede ownership.
///
/// Individual malformed findings are skipped rather than failing the batch: a
/// future clj-kondo key we mis-read must not blank out the findings beside it.
pub fn parse_findings(json: &str) -> Option<Vec<Diagnostic>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let findings = value.get("findings")?.as_array()?;
    Some(findings.iter().filter_map(finding_to_diagnostic).collect())
}

/// One JSON finding as an LSP diagnostic, or `None` when it lacks the fields a
/// squiggle needs (`type`, `message`, `row`, `col`).
fn finding_to_diagnostic(finding: &serde_json::Value) -> Option<Diagnostic> {
    let code = finding.get("type")?.as_str()?;
    let message = finding.get("message")?.as_str()?;
    let row = finding.get("row")?.as_u64()?;
    let col = finding.get("col")?.as_u64()?;

    Some(Diagnostic {
        range: finding_range(finding, row, col),
        severity: Some(severity(finding.get("level").and_then(|l| l.as_str()))),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some(SOURCE.to_string()),
        message: message.to_string(),
        tags: tags(code),
        ..Default::default()
    })
}

/// clj-kondo positions are 1-based and its `end-col` is exclusive, so both
/// ends simply lose one. `end-row`/`end-col` are optional (bracket-mismatch
/// `syntax` findings omit them); without them the squiggle covers one
/// character, which in 0-based terms is `col - 1 .. col`.
fn finding_range(finding: &serde_json::Value, row: u64, col: u64) -> Range {
    let start = position(row, col);
    let end = match (
        finding.get("end-row").and_then(|v| v.as_u64()),
        finding.get("end-col").and_then(|v| v.as_u64()),
    ) {
        (Some(end_row), Some(end_col)) => position(end_row, end_col),
        _ => tower_lsp::lsp_types::Position::new(start.line, start.character + 1),
    };
    Range { start, end }
}

/// A 1-based (row, col) pair as a 0-based LSP position, saturating so a
/// malformed `0` can never underflow.
fn position(row: u64, col: u64) -> tower_lsp::lsp_types::Position {
    tower_lsp::lsp_types::Position::new(
        row.saturating_sub(1).min(u32::MAX as u64) as u32,
        col.saturating_sub(1).min(u32::MAX as u64) as u32,
    )
}

/// clj-kondo's three levels. An unknown level is a warning — visible, but not
/// escalated into an error on our guess.
fn severity(level: Option<&str>) -> DiagnosticSeverity {
    match level {
        Some("error") => DiagnosticSeverity::ERROR,
        Some("info") => DiagnosticSeverity::INFORMATION,
        _ => DiagnosticSeverity::WARNING,
    }
}

/// Editors fade `UNNECESSARY` and strike through `DEPRECATED`. clj-kondo names
/// its dead-code linters `unused-*` uniformly, so the prefix covers every one
/// of them (including linters added after this was written).
fn tags(code: &str) -> Option<Vec<DiagnosticTag>> {
    if code.starts_with("unused-") {
        Some(vec![DiagnosticTag::UNNECESSARY])
    } else if code == "deprecated-var" {
        Some(vec![DiagnosticTag::DEPRECATED])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    fn has(pairs: &[(String, String)], macro_fqn: &str, target: &str) -> bool {
        pairs.iter().any(|(m, t)| m == macro_fqn && t == target)
    }

    #[test]
    fn parses_lint_as_pairs() {
        let edn = r#"{:linters {:foo {:level :off}}
                      :lint-as {defcomponent/defcomponent clojure.core/def
                                plumbing.core/for-map clojure.core/for}}"#;
        let pairs = parse_lint_as(edn);
        assert_eq!(pairs.len(), 2);
        assert!(has(&pairs, "defcomponent/defcomponent", "clojure.core/def"));
        assert!(has(&pairs, "plumbing.core/for-map", "clojure.core/for"));
    }

    #[test]
    fn missing_lint_as_yields_empty() {
        assert!(parse_lint_as(r#"{:linters {:foo {:level :off}}}"#).is_empty());
    }

    #[test]
    fn non_map_input_yields_empty() {
        assert!(parse_lint_as("123").is_empty());
        assert!(parse_lint_as("not edn (((").is_empty());
    }

    #[test]
    fn lint_as_value_not_a_map_yields_empty() {
        assert!(parse_lint_as(r#"{:lint-as :nope}"#).is_empty());
    }

    #[test]
    fn unqualified_symbols_are_kept_as_bare_names() {
        // Rare, but a bare symbol key/value should round-trip without a slash.
        let pairs = parse_lint_as(r#"{:lint-as {defthing def}}"#);
        assert!(has(&pairs, "defthing", "def"));
    }

    // --- JSON findings -> LSP diagnostics -------------------------------

    fn finding_diags(json: &str) -> Vec<Diagnostic> {
        parse_findings(json).expect("well-formed kondo output should parse")
    }

    #[test]
    fn parses_a_finding_into_a_diagnostic() {
        // The exact shape clj-kondo emits for an unresolved namespace:
        // `other/thing` starting at col 6, end-col 17 (1-based, exclusive).
        let json = r#"{"findings":[{"type":"unresolved-namespace","level":"warning",
                       "filename":"/p/src/a.clj","row":3,"col":6,"end-row":3,"end-col":17,
                       "langs":[],"message":"Unresolved namespace other."}],
                       "summary":{"error":0,"warning":1}}"#;
        let diags = finding_diags(json);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.range.start, Position::new(2, 5));
        assert_eq!(d.range.end, Position::new(2, 16));
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            d.code,
            Some(NumberOrString::String("unresolved-namespace".to_string()))
        );
        assert_eq!(d.source.as_deref(), Some("clj-kondo"));
        assert_eq!(d.message, "Unresolved namespace other.");
        assert_eq!(d.tags, None);
    }

    #[test]
    fn maps_every_level_to_a_severity() {
        let json = r#"{"findings":[
            {"type":"a","level":"error","row":1,"col":1,"message":"e"},
            {"type":"b","level":"warning","row":1,"col":1,"message":"w"},
            {"type":"c","level":"info","row":1,"col":1,"message":"i"}]}"#;
        let sev: Vec<_> = finding_diags(json).iter().map(|d| d.severity).collect();
        assert_eq!(
            sev,
            vec![
                Some(DiagnosticSeverity::ERROR),
                Some(DiagnosticSeverity::WARNING),
                Some(DiagnosticSeverity::INFORMATION),
            ]
        );
    }

    #[test]
    fn missing_end_position_yields_a_one_char_range() {
        // Bracket-mismatch `syntax` findings carry no end-row/end-col.
        let json = r#"{"findings":[{"type":"syntax","level":"error","row":11,"col":5,
                       "message":"Mismatched bracket"}]}"#;
        let diags = finding_diags(json);
        assert_eq!(diags[0].range.start, Position::new(10, 4));
        assert_eq!(diags[0].range.end, Position::new(10, 5));
    }

    #[test]
    fn unused_types_are_tagged_unnecessary() {
        let json = r#"{"findings":[
            {"type":"unused-namespace","level":"warning","row":2,"col":3,"message":"u"},
            {"type":"unused-binding","level":"warning","row":4,"col":1,"message":"u"}]}"#;
        for d in finding_diags(json) {
            assert_eq!(
                d.tags,
                Some(vec![DiagnosticTag::UNNECESSARY]),
                "{:?}",
                d.code
            );
        }
    }

    #[test]
    fn deprecated_var_is_tagged_deprecated() {
        let json = r#"{"findings":[{"type":"deprecated-var","level":"warning","row":2,
                       "col":3,"message":"d"}]}"#;
        assert_eq!(
            finding_diags(json)[0].tags,
            Some(vec![DiagnosticTag::DEPRECATED])
        );
    }

    #[test]
    fn empty_findings_parse_to_an_empty_diagnostic_set() {
        // A clean file is success with zero diagnostics — distinct from a
        // failed run, which must leave the native set alone.
        assert_eq!(
            parse_findings(r#"{"findings":[],"summary":{}}"#),
            Some(vec![])
        );
    }

    #[test]
    fn unparseable_output_yields_none() {
        assert_eq!(parse_findings(""), None);
        assert_eq!(parse_findings("Exception in thread \"main\""), None);
        assert_eq!(parse_findings("[1,2,3]"), None);
        assert_eq!(parse_findings(r#"{"summary":{}}"#), None);
    }

    #[test]
    fn malformed_findings_are_skipped_not_fatal() {
        // One good finding among junk still reaches the editor.
        let json = r#"{"findings":[
            {"level":"error","row":1,"col":1,"message":"no type"},
            {"type":"ok","level":"error","row":7,"col":2,"message":"good"}]}"#;
        let diags = finding_diags(json);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "good");
    }
}
