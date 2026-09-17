//! The landing rules in `tests/common/sites.rs`: what counts as a definition
//! that arrived where it should. The bench, the soak and the compare gate all
//! judge through them, so a rule that slips lets three gates pass on a wrong
//! answer at once.

mod common;

use std::path::{Path, PathBuf};

use serde_json::json;

use common::sites::{answers, landing, Dialect, Expect, Landing};

fn at(uri: &str) -> serde_json::Value {
    json!([{ "uri": uri, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }])
}

#[test]
fn file_landing_is_exact_not_a_suffix() {
    let expect = Expect::File(PathBuf::from("/corpus/src/clojure/string.clj"));
    let asking = Path::new("/corpus/src/a.clj");
    assert_eq!(
        landing(
            &at("file:///corpus/src/clojure/string.clj"),
            &expect,
            asking
        ),
        Landing::Landed
    );
    // clj-kondo has a `src/clj_kondo/impl/types/clojure/string.clj`; a suffix
    // match would credit it as the definition of `clojure.string`.
    assert_eq!(
        landing(
            &at("file:///corpus/src/clj_kondo/impl/types/clojure/string.clj"),
            &expect,
            asking
        ),
        Landing::Miss
    );
    assert_eq!(landing(&json!(null), &expect, asking), Landing::Miss);
}

#[test]
fn archive_landing_from_clj_wants_clj_or_cljc() {
    let expect = Expect::Archive("clojure/string".to_string());
    let asking = Path::new("/corpus/src/a.clj");
    let jar = |ext: &str| at(&format!("jar:file:///m2/clojure.jar!/clojure/string.{ext}"));
    assert_eq!(landing(&jar("clj"), &expect, asking), Landing::Landed);
    assert_eq!(landing(&jar("cljc"), &expect, asking), Landing::Landed);
    assert_eq!(
        landing(&jar("cljs"), &expect, asking),
        Landing::WrongDialect
    );
    // clojure-lsp's scheme is a landing too.
    assert_eq!(
        landing(
            &at("zipfile:///m2/clojure.jar::clojure/string.clj"),
            &expect,
            asking
        ),
        Landing::Landed
    );
    // A different entry is a miss, whatever the dialect.
    assert_eq!(
        landing(
            &at("jar:file:///m2/clojure.jar!/clojure/set.clj"),
            &expect,
            asking
        ),
        Landing::Miss
    );
    // `answers` is the bench's lenient view: anything but a miss.
    assert!(answers(&jar("cljs"), &expect, asking));
    assert!(!answers(&jar("edn"), &expect, asking));
}

#[test]
fn archive_landing_from_cljc_accepts_every_dialect() {
    let expect = Expect::Archive("clojure/string".to_string());
    let asking = Path::new("/corpus/src/a.cljc");
    for ext in ["clj", "cljc", "cljs"] {
        let uri = format!("jar:file:///m2/clojure.jar!/clojure/string.{ext}");
        assert_eq!(
            landing(&at(&uri), &expect, asking),
            Landing::Landed,
            "{ext}"
        );
    }
    let cljs = Path::new("/corpus/src/a.cljs");
    assert_eq!(
        landing(
            &at("jar:file:///m2/clojure.jar!/clojure/string.clj"),
            &expect,
            cljs
        ),
        Landing::WrongDialect
    );
}

#[test]
fn dialect_reads_the_extension() {
    assert_eq!(Dialect::of(Path::new("a.clj")), Dialect::Clj);
    assert_eq!(Dialect::of(Path::new("a.cljs")), Dialect::Cljs);
    assert_eq!(Dialect::of(Path::new("a.cljc")), Dialect::Cljc);
    // Anything else — `.edn`, no extension — is treated as `.cljc`: it has no
    // dialect of its own, so no library file is the wrong one for it.
    assert_eq!(Dialect::of(Path::new("config.edn")), Dialect::Cljc);
    assert!(Dialect::Clj.accepts("cljc"));
    assert!(!Dialect::Clj.accepts("cljs"));
    assert!(Dialect::Cljs.accepts("cljc"));
    assert!(!Dialect::Cljs.accepts("clj"));
    assert!(Dialect::Cljc.accepts("cljs"));
}
