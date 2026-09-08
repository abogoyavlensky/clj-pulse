use std::path::Path;

use clj_pulse::index::extractor;
use clj_pulse::index::scanner;

#[test]
fn test_indexes_all_files_in_project() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index =
        scanner::build_index(root, &paths, &clj_pulse::index::ExtractConfig::default()).unwrap();

    assert!(index.lookup("simple.core/add").is_some());
    assert!(index.lookup("simple.core/multiply").is_some());
    assert!(index.lookup("simple.core/VERSION").is_some());
    assert!(index.lookup("simple.utils/greet").is_some());
    assert!(index.lookup("simple.utils/add-and-double").is_some());
}

#[test]
fn test_index_contains_ns_metadata() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index =
        scanner::build_index(root, &paths, &clj_pulse::index::ExtractConfig::default()).unwrap();

    let meta = index.ns_meta("simple.utils").unwrap();
    assert_eq!(
        meta.aliases.get("core").map(|s| s.as_str()),
        Some("simple.core")
    );
}

#[test]
fn test_remove_file_cleans_up_all_symbols() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index =
        scanner::build_index(root, &paths, &clj_pulse::index::ExtractConfig::default()).unwrap();

    assert!(index.lookup("simple.utils/greet").is_some());

    let utils_path = root.join("src/utils.clj");
    index.remove_file(&utils_path);

    assert!(index.lookup("simple.utils/greet").is_none());
    assert!(index.ns_meta("simple.utils").is_none());
}

#[test]
fn test_insert_file_updates_index() {
    let root = Path::new("tests/fixtures/simple_project");
    let paths = vec![root.join("src")];
    let index =
        scanner::build_index(root, &paths, &clj_pulse::index::ExtractConfig::default()).unwrap();

    let new_source = r#"
        (ns simple.utils (:require [simple.core :as core]))
        (defn new-fn [x] x)
    "#;
    let fake_path = root.join("src/utils.clj");
    index.remove_file(&fake_path);
    let (meta, syms) = extractor::extract(new_source, &fake_path).unwrap();
    index.insert_file(meta, syms, vec![]);

    assert!(index.lookup("simple.utils/new-fn").is_some());
    assert!(index.lookup("simple.utils/greet").is_none());
}

#[test]
fn test_clear_libs_allows_dir_lib_reinsert() {
    use clj_pulse::index::{Index, SymbolSource};
    use std::path::PathBuf;

    let index = Index::new();
    let (meta, mut syms) = extractor::extract(
        "(ns dirlib.core)\n(defn go [x] x)",
        &PathBuf::from("/libs/dirlib/core.clj"),
    )
    .unwrap();
    for s in &mut syms {
        s.source = SymbolSource::Dir(PathBuf::from("/libs"));
    }
    index.insert_lib_file(meta.clone(), syms.clone());
    assert!(index.lookup("dirlib.core/go").is_some());

    // Classpath change: clear and re-insert the same dir lib
    index.clear_libs();
    assert!(index.lookup("dirlib.core/go").is_none());
    index.insert_lib_file(meta, syms);
    assert!(
        index.lookup("dirlib.core/go").is_some(),
        "dir-lib namespace must be reinsertable after clear_libs"
    );
}

#[test]
fn test_insert_edn_file_occurrences_and_sentinel_isolation() {
    use clj_pulse::index::{Index, Occurrence};
    use std::path::PathBuf;
    use tower_lsp::lsp_types::Range;

    let index = Index::new();

    // A no-`ns` Clojure file has an empty-string namespace; its symbol must not
    // be disturbed by EDN files (which use a NUL sentinel ns, not "").
    let clj = PathBuf::from("/proj/src/nons.clj");
    let (meta, syms) = extractor::extract("(def x 1)", &clj).unwrap();
    assert_eq!(meta.name, "", "no-ns file should have empty namespace");
    index.insert_file(meta, syms, vec![]);
    assert!(index.lookup("x").is_some());

    // Insert an EDN config file's occurrences.
    let edn = PathBuf::from("/proj/resources/config.edn");
    let occ = Occurrence {
        fqn: ":readx.db/db".to_string(),
        name_range: Range::default(),
    };
    index.insert_edn_file(edn.clone(), vec![occ]);
    assert!(
        index.is_project_path(&edn),
        "EDN file should be an editable project path"
    );

    // Removing the EDN file clears it without panicking and without clobbering
    // the empty-ns Clojure file's symbol.
    index.remove_file(&edn);
    assert!(!index.is_project_path(&edn));
    assert!(
        index.lookup("x").is_some(),
        "removing an EDN file must not disturb a no-ns clj file's symbols"
    );
}

#[test]
fn test_keyword_counts_track_insert_remove_merge() {
    use clj_pulse::index::{Index, NsMeta, Occurrence};
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use tower_lsp::lsp_types::Range;

    let file = PathBuf::from("/proj/src/a.clj");
    let meta = || NsMeta {
        name: "a".to_string(),
        file: file.clone(),
        aliases: HashMap::new(),
        refers: HashMap::new(),
        requires: vec![],
        imports: HashMap::new(),
        refer_all: vec![],
        as_aliases: vec![],
        core_excludes: vec![],
    };
    let occ = |fqn: &str| Occurrence {
        fqn: fqn.to_string(),
        name_range: Range::default(),
    };
    let count_of = |index: &Index, fqn: &str| {
        index
            .keyword_counts()
            .into_iter()
            .find(|(k, _)| k == fqn)
            .map(|(_, n)| n)
            .unwrap_or(0)
    };

    let index = Index::new();
    index.insert_file(meta(), vec![], vec![occ(":id"), occ(":id"), occ(":x/y")]);
    assert_eq!(count_of(&index, ":id"), 2);
    assert_eq!(count_of(&index, ":x/y"), 1);
    // Var occurrences are not keywords and never counted.
    index.insert_file(
        meta(),
        vec![],
        vec![occ(":id"), occ(":id"), occ(":x/y"), occ("a/f")],
    );
    assert_eq!(count_of(&index, "a/f"), 0, "var fqns must not be counted");

    // Re-indexing the same file replaces its occurrences: counts must not double.
    assert_eq!(
        count_of(&index, ":id"),
        2,
        "re-index must not double counts"
    );

    index.remove_file(&file);
    assert!(
        index.keyword_counts().is_empty(),
        "counts after remove: {:?}",
        index.keyword_counts()
    );

    // A merge that re-scans the same file with fewer keywords replaces, not adds.
    index.insert_file(meta(), vec![], vec![occ(":id"), occ(":id")]);
    let new_index = Index::new();
    new_index.insert_file(meta(), vec![], vec![occ(":id")]);
    index.merge_project_from(new_index, &HashSet::new());
    assert_eq!(count_of(&index, ":id"), 1, "merge must replace, not add");

    // EDN config files contribute their keywords too, once each.
    let edn = PathBuf::from("/proj/resources/config.edn");
    index.insert_edn_file(edn.clone(), vec![occ(":x/y"), occ(":x/y")]);
    assert_eq!(count_of(&index, ":x/y"), 2);
    index.insert_edn_file(edn, vec![occ(":x/y")]);
    assert_eq!(count_of(&index, ":x/y"), 1, "re-insert must replace");
}
