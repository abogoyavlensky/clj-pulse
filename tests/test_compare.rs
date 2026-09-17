//! Differential correctness against clj-kondo's analysis: every position the
//! oracle knows the answer to becomes a probe, and clj-pulse's definition,
//! references and rename answers are judged against it. `bb check` runs the
//! whole pipeline on `simple_project`; `bb compare` runs it on a pinned real
//! corpus and reports every disagreement by language construct.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::oracle::{self, Expectation, Probe};
use common::setup_project;
use common::sites::Dialect;

/// `Some(root)` when the host has clj-kondo, printing why not otherwise: the
/// suite has to stay green on a box without it.
fn with_kondo(name: &str) -> Option<tempfile::TempDir> {
    match oracle::kondo_available() {
        Some(version) => {
            if !version.contains(oracle::PINNED_KONDO) {
                println!(
                    "{name}: clj-kondo {version} is not the pinned {}",
                    oracle::PINNED_KONDO
                );
            }
            Some(setup_project())
        }
        None => {
            println!("{name}: skipped, no clj-kondo on PATH");
            None
        }
    }
}

mod oracle_tests {
    use super::*;

    fn fixture_probes() -> Option<(tempfile::TempDir, Vec<Probe>)> {
        let tmp = with_kondo("oracle_tests")?;
        let analysis = oracle::run(tmp.path(), &["src"]).expect("clj-kondo runs");
        let probes = oracle::probes(&analysis, tmp.path());
        Some((tmp, probes))
    }

    fn at<'a>(probes: &'a [Probe], file: &str, line: u32, character: u32) -> Vec<&'a Probe> {
        probes
            .iter()
            .filter(|p| p.file.ends_with(file) && p.line == line && p.character == character)
            .collect()
    }

    #[test]
    fn run_parses_every_analysis_section() {
        let Some(tmp) = with_kondo("run_parses_every_analysis_section") else {
            return;
        };
        let analysis = oracle::run(tmp.path(), &["src"]).expect("clj-kondo runs");
        assert!(!analysis.var_definitions.is_empty());
        assert!(!analysis.var_usages.is_empty());
        assert!(!analysis.locals.is_empty());
        assert!(!analysis.local_usages.is_empty());
        assert!(!analysis.keywords.is_empty());
        assert!(!analysis.namespace_definitions.is_empty());
        // Filenames come back absolute, whatever kondo printed.
        assert!(analysis
            .var_definitions
            .iter()
            .all(|d| Path::new(&d.filename).is_absolute()));
    }

    #[test]
    fn var_usage_expects_the_definition_name_row() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // utils.clj:7 `(* 2 (core/add x y))` — kondo's `name-col` is 9, the
        // `c` of `core/add`; the probe moves to `add` at column 14 (1-based).
        let found = at(&probes, "src/utils.clj", 6, 13);
        assert_eq!(found.len(), 1, "{found:?}");
        let probe = found[0];
        assert_eq!(probe.token, "core/add");
        assert_eq!(probe.bucket, "var-usage/project/aliased");
        match &probe.expect {
            Expectation::Definition { file, line } => {
                assert!(file.ends_with("src/core.clj"));
                assert_eq!(*line, 4, "`(defn add` is line 5");
            }
            other => panic!("expected Definition, got {other:?}"),
        }
    }

    #[test]
    fn core_usage_expects_a_library_definition_in_the_asking_dialect() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // core.clj:5 `(defn add` — `defn` at column 2.
        let found = at(&probes, "src/core.clj", 4, 1);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].bucket, "var-usage/core/macro");
        match &found[0].expect {
            Expectation::LibraryDefinition { ns, dialect } => {
                assert_eq!(ns, "clojure.core");
                assert_eq!(*dialect, Dialect::Clj);
            }
            other => panic!("expected LibraryDefinition, got {other:?}"),
        }
    }

    #[test]
    fn destructured_local_refuses_rename_and_keys_keyword_too() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // kw_destructure.clj:6 `(defn read-local [{::kw/keys [local]}]` — `local` at column 31.
        let found = at(&probes, "src/kw_destructure.clj", 5, 30);
        let local_rename = found.iter().find(|p| {
            p.bucket == "local/destructured" && matches!(p.expect, Expectation::RenameRefused)
        });
        assert!(local_rename.is_some(), "{found:?}");
        let keys_rename = found
            .iter()
            .find(|p| p.bucket == "keyword/keys" && matches!(p.expect, Expectation::RenameRefused));
        assert!(keys_rename.is_some(), "{found:?}");
        // The local still gets a references probe; the keyword from that
        // position does not, since the cursor is on the local binding.
        assert!(found
            .iter()
            .any(|p| p.bucket == "local/destructured"
                && matches!(p.expect, Expectation::References(_))));
        assert!(!found
            .iter()
            .any(|p| p.bucket.starts_with("keyword/")
                && matches!(p.expect, Expectation::References(_))));
    }

    #[test]
    fn plain_local_definition_and_sites() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // locals.clj:5 `        scaled (* base 2)]` — usage of `base` at column 19.
        let found = at(&probes, "src/locals.clj", 4, 18);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].bucket, "local/plain");
        match &found[0].expect {
            Expectation::Definition { file, line } => {
                assert!(file.ends_with("src/locals.clj"));
                assert_eq!(*line, 3);
            }
            other => panic!("expected Definition, got {other:?}"),
        }
        // locals.clj:4 `  (let [base   (inc n)` — the binding at column 9.
        let binding = at(&probes, "src/locals.clj", 3, 8);
        let refs = binding
            .iter()
            .find_map(|p| match &p.expect {
                Expectation::References(sites) => Some(sites),
                _ => None,
            })
            .expect("references probe on the binding");
        assert_eq!(refs.exact.len(), 3, "binding + two usages: {refs:?}");
        assert!(binding
            .iter()
            .any(|p| matches!(&p.expect, Expectation::RenameSites(s) if s.exact.len() == 3)));
    }

    #[test]
    fn declare_resolves_to_the_real_def_and_stays_a_site() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // ns_options.clj:21 `  (cmap inc (defined-later m)))` — `defined-later` at column 14.
        let usage = at(&probes, "src/ns_options.clj", 20, 13);
        assert_eq!(usage.len(), 1, "{usage:?}");
        match &usage[0].expect {
            Expectation::Definition { line, .. } => {
                assert_eq!(*line, 10, "`(defn defined-later` is line 11")
            }
            other => panic!("expected Definition, got {other:?}"),
        }
        // The declare on line 9 has no probe of its own …
        assert!(at(&probes, "src/ns_options.clj", 8, 9).is_empty());
        // … but the def's sites include it.
        let def = at(&probes, "src/ns_options.clj", 10, 6);
        assert_eq!(def.len(), 2, "references + rename: {def:?}");
        for probe in def {
            assert_eq!(probe.bucket, "var-def/defn");
            let sites = match &probe.expect {
                Expectation::References(s) | Expectation::RenameSites(s) => s,
                other => panic!("{other:?}"),
            };
            let lines: BTreeSet<u32> = sites.exact.iter().map(|(_, l, _)| *l).collect();
            assert_eq!(lines, BTreeSet::from([8, 10, 20]), "{sites:?}");
        }
        // A var with only a declare resolves to the declare.
        let only = at(&probes, "src/ns_options.clj", 11, 3);
        assert_eq!(only.len(), 1);
        match &only[0].expect {
            Expectation::Definition { line, .. } => assert_eq!(*line, 6),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn two_usages_on_one_line_count_twice() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // twice.clj:5 `(defn twin [x] x)` — the def at column 7.
        let def = at(&probes, "src/twice.clj", 4, 6);
        let sites = def
            .iter()
            .find_map(|p| match &p.expect {
                Expectation::References(s) => Some(s),
                _ => None,
            })
            .expect("references probe");
        let usage_line = sites
            .per_line
            .iter()
            .find(|((_, line), _)| *line == 7)
            .map(|(_, n)| *n);
        assert_eq!(usage_line, Some(2), "{sites:?}");
        assert_eq!(sites.exact.len(), 3);
    }

    #[test]
    fn keyword_groups_refuse_rename_when_destructured_or_foreign() {
        let Some((_tmp, probes)) = fixture_probes() else {
            return;
        };
        // keywords.clj:7 `   ::local true})` — plain `::local` at column 4, but
        // kw_destructure.clj reads the key through `::kw/keys`, so the whole
        // group refuses.
        let local = at(&probes, "src/keywords.clj", 6, 3);
        assert!(local
            .iter()
            .any(|p| p.bucket == "keyword/qualified"
                && matches!(p.expect, Expectation::RenameRefused)));
        let refs = local
            .iter()
            .find_map(|p| match &p.expect {
                Expectation::References(s) => Some(s),
                _ => None,
            })
            .expect("references probe");
        assert_eq!(refs.exact.len(), 3, "{refs:?}");
        // alias_sites.clj:11 `(def lit :c/site)` — `c` is no namespace this
        // corpus defines; the cursor is on `site`.
        let foreign = at(&probes, "src/alias_sites.clj", 10, 12);
        assert!(
            foreign
                .iter()
                .any(|p| matches!(p.expect, Expectation::RenameRefused)),
            "{foreign:?}"
        );
        // keywords.clj:13 `(assoc m :id (::c/thing m)))` — one site, renamable;
        // the cursor sits on `thing`, past the alias.
        let thing = at(&probes, "src/keywords.clj", 12, 20);
        assert!(
            thing.iter().any(|p| p.bucket == "keyword/alias"
                && matches!(&p.expect, Expectation::RenameSites(s) if s.exact.len() == 1)),
            "{thing:?}"
        );
    }

    #[test]
    fn unjudged_counts_uncovered_tokens_only() {
        let Some(tmp) = with_kondo("unjudged_counts_uncovered_tokens_only") else {
            return;
        };
        let analysis = oracle::run(tmp.path(), &["src"]).expect("clj-kondo runs");
        let file = tmp.path().join("src/consumer.clj");
        let text = std::fs::read_to_string(&file).unwrap();
        let covered = oracle::covered(&analysis, &file);
        let unjudged = oracle::unjudged(&text, &covered);
        // `helpers/greet` resolves to no namespace, and kondo reports nothing
        // for the `ns` head itself: two tokens no oracle entry vouches for.
        assert_eq!(unjudged, 2);
        assert!(oracle::unjudged(&text, &BTreeSet::new()) > unjudged);
    }
}
