pub mod builtins;
pub mod clojuredocs;
pub mod code_action;
pub mod completion;
pub mod definition;
pub mod highlight;
pub mod hover;
pub mod ignored_forms;
pub mod indent;
pub mod java;
mod letgo_native_names;
pub mod matching;
pub mod references;
pub mod selection;
pub mod signature;
pub mod symbols;

use crate::index::{core_ns, CoreSymbol, DefKind, Dialect, Index, Symbol};

#[derive(Debug, Clone)]
pub enum ResolvedSymbol {
    Project(Symbol),
    Core(CoreSymbol),
    /// A special form (compiler intrinsic, Clojure or let-go) — hover-only,
    /// never navigable.
    SpecialForm(&'static builtins::SpecialForm),
    /// A let-go native core fn (Go `ns.Def`) — hover-only; doc/arglists borrowed
    /// from the clojure.core table.
    LetgoNative(CoreSymbol),
}

/// Resolves the symbol `word` names when read inside `current_ns`: a qualified
/// name through its `:as` alias, a bare name through `:refer`, then the current
/// namespace's own defs, then the `:refer :all` / `(:use ns)` namespaces, and
/// finally the builtins (clojure.core, or let-go's `core` in a let-go project).
/// `dialect` is the asking file's: a namespace split across `.clj` and `.cljs`
/// answers with the metadata and definitions of that half, and a library with
/// a ClojureScript copy with that copy.
pub fn resolve_symbol(
    index: &Index,
    word: &str,
    current_ns: &str,
    dialect: Dialect,
) -> Option<ResolvedSymbol> {
    let ns_meta = index.ns_meta_for(current_ns, dialect);
    let lookup_in_ns = |ns: &str, name: &str| lookup_in_ns_for(index, ns, name, dialect);

    if let Some((alias, name)) = word.split_once('/') {
        // Qualified symbol: alias/name
        let full_ns = ns_meta
            .as_ref()
            .and_then(|m| m.aliases.get(alias))
            .map(|s| s.as_str())
            .unwrap_or(alias);

        if let Some(sym) = lookup_in_ns(full_ns, name) {
            return Some(ResolvedSymbol::Project(sym));
        }

        if let Some(sym) = resolve_factory(index, full_ns, name, dialect) {
            return Some(ResolvedSymbol::Project(sym));
        }
    } else {
        // Bare symbol: check refers, then current ns, then core
        if let Some(meta) = &ns_meta {
            if let Some(fqn) = meta.refers.get(word) {
                if let Some(sym) = index.lookup_for(fqn, dialect) {
                    return Some(ResolvedSymbol::Project(sym));
                }
                // A referred record constructor (`:refer [->DB map->DB]`): the
                // ctor fqn is not indexed, but its record is — resolve it in the
                // referred namespace.
                if let Some((refer_ns, _)) = fqn.rsplit_once('/') {
                    if let Some(sym) = resolve_factory(index, refer_ns, word, dialect) {
                        return Some(ResolvedSymbol::Project(sym));
                    }
                }
                // A core name referred under another name
                // (`:refer-clojure :rename {map cmap}`) has no indexed var
                // unless the clojure JAR is on the classpath — fall back to
                // the curated core entry so hover still describes it.
                let core_name = fqn
                    .strip_prefix("clojure.core/")
                    .or_else(|| fqn.strip_prefix("cljs.core/"));
                if let Some(core_name) = core_name {
                    if let Some(core) = index.core_symbols.iter().find(|c| c.name == core_name) {
                        return Some(ResolvedSymbol::Core(core.clone()));
                    }
                }
            }
        }

        if let Some(sym) = lookup_in_ns(current_ns, word) {
            return Some(ResolvedSymbol::Project(sym));
        }

        // A locally generated record/type constructor shadows a clojure.core
        // symbol of the same name, so resolve it before the core fallback.
        if let Some(sym) = resolve_factory(index, current_ns, word, dialect) {
            return Some(ResolvedSymbol::Project(sym));
        }

        // `[ns :refer :all]` / `(:use ns)` make every public var of those
        // namespaces a bare name here — that is how `is` and `testing` navigate
        // in a test file that pulled clojure.test in wholesale. Tried after the
        // current namespace, which shadows them just as it does in Clojure.
        if let Some(meta) = &ns_meta {
            for ns in &meta.refer_all {
                // Private vars are indexed for jar navigation but are not
                // referred, so a bare name never names one.
                if let Some(sym) = lookup_in_ns(ns, word).filter(|s| s.kind != DefKind::DefnPrivate)
                {
                    return Some(ResolvedSymbol::Project(sym));
                }
                // A record/type constructor is referred like any other public
                // var, but is generated rather than indexed.
                if let Some(sym) = resolve_factory(index, ns, word, dialect) {
                    return Some(ResolvedSymbol::Project(sym));
                }
            }
        }

        // In a let-go project, bare names are auto-referred from let-go's
        // built-in `core` (indexed from `.lg` source), not the static
        // clojure.core list — which would mis-navigate to a clojure JAR absent
        // from a let-go classpath. Resolve there and never fall through: a name
        // missing from `core` (a go-only primitive) simply doesn't navigate.
        if index.letgo_core() {
            if let Some(sym) = index.lookup_in_ns("core", word) {
                return Some(ResolvedSymbol::Project(sym));
            }
            // Compiler special forms (`if`, `try`, …) have no `.lg` source;
            // surface a description for hover but never navigate.
            if let Some(sf) = builtins::special_form(word, true) {
                return Some(ResolvedSymbol::SpecialForm(sf));
            }
            // Native core vars/fns (Go `ns.Def`, e.g. `count`,
            // `*command-line-args*`) also have no `.lg` source. Use the
            // version's harvested set when available, else the static list;
            // borrow the clojure.core entry for hover when present, otherwise
            // show a bare native.
            let native = index
                .letgo_native_contains(word)
                .unwrap_or_else(|| builtins::is_native(word));
            if native {
                let core = index
                    .core_symbols
                    .iter()
                    .find(|c| c.name == word)
                    .cloned()
                    .unwrap_or_else(|| CoreSymbol {
                        name: word.to_string(),
                        params: String::new(),
                        doc: String::new(),
                    });
                return Some(ResolvedSymbol::LetgoNative(core));
            }
            return None;
        }

        // Clojure compiler special forms (`if`, `do`, `new`, …) have no
        // clojure.core var and no source; surface a description for hover but
        // never navigate. Checked after project lookups (a project var named
        // `new` still wins) and before the static clojure.core list.
        if let Some(sf) = builtins::special_form(word, false) {
            return Some(ResolvedSymbol::SpecialForm(sf));
        }

        // `(:refer-clojure :exclude [update])` unmaps the core var here, so a
        // bare `update` is this file's own — never core's.
        let excluded = ns_meta
            .as_ref()
            .is_some_and(|m| m.core_excludes.iter().any(|e| e == word));
        if !excluded {
            if let Some(core) = index.core_symbols.iter().find(|c| c.name == word) {
                return Some(ResolvedSymbol::Core(core.clone()));
            }
            // A `.cljs` file's core is `cljs.core`, which defines far more than
            // the static list names: its protocols (`IMeta`), their methods
            // (`-nth`), `not-native`, … Asked after the static list, so a name
            // both cores share keeps its curated hover.
            if dialect == Dialect::Cljs {
                if let Some(sym) = lookup_in_ns(core_ns(dialect), word) {
                    return Some(ResolvedSymbol::Project(sym));
                }
            }
        }
    }

    None
}

/// The type a constructor function builds, plus whether it is the map
/// constructor: `map->DB` → `("DB", true)`, `->DB` → `("DB", false)`. `None`
/// for non-factory names (and the bare `->`/`map->`).
fn factory_target(name: &str) -> Option<(&str, bool)> {
    if let Some(t) = name.strip_prefix("map->") {
        (!t.is_empty()).then_some((t, true))
    } else if let Some(t) = name.strip_prefix("->") {
        (!t.is_empty()).then_some((t, false))
    } else {
        None
    }
}

/// [`Index::lookup_in_ns`] by the rule of [`Index::lookup_for`].
fn lookup_in_ns_for(index: &Index, ns: &str, name: &str, dialect: Dialect) -> Option<Symbol> {
    index.lookup_for(&format!("{}/{}", ns, name), dialect)
}

/// Resolves an auto-generated record/type constructor to the `defrecord`/
/// `deftype` it builds, so navigation/hover land on the type. Gated on kind so
/// a plain fn named `->foo` is never hijacked; `map->X` is records-only, since
/// `deftype` generates `->X` but no map constructor.
fn resolve_factory(index: &Index, ns: &str, name: &str, dialect: Dialect) -> Option<Symbol> {
    let (target, is_map_ctor) = factory_target(name)?;
    let sym = lookup_in_ns_for(index, ns, target, dialect)?;
    let ok = match sym.kind {
        DefKind::Defrecord => true,
        DefKind::Deftype => !is_map_ctor,
        _ => false,
    };
    ok.then_some(sym)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{NsMeta, SymbolSource};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tower_lsp::lsp_types::Range;

    #[test]
    fn factory_target_strips_prefixes_and_flags_map_ctor() {
        assert_eq!(factory_target("map->DB"), Some(("DB", true)));
        assert_eq!(factory_target("->DB"), Some(("DB", false)));
        assert_eq!(factory_target("plain"), None);
        assert_eq!(factory_target("->"), None);
        assert_eq!(factory_target("map->"), None);
    }

    fn sym(name: &str, ns: &str, kind: DefKind) -> Symbol {
        Symbol {
            name: name.to_string(),
            fqn: format!("{}/{}", ns, name),
            ns: ns.to_string(),
            kind,
            params: vec![],
            doc: None,
            file: PathBuf::from("a.clj"),
            source: SymbolSource::Project,
            range: Range::default(),
            name_range: Range::default(),
            private: false,
        }
    }

    #[test]
    fn core_exclude_hides_the_core_var_and_rename_finds_it() {
        // `(:refer-clojure :exclude [update] :rename {map cmap})`.
        let index = Index::new_with_core();
        let mut meta = NsMeta {
            name: "my.ns".to_string(),
            file: PathBuf::from("a.clj"),
            aliases: HashMap::new(),
            refers: HashMap::new(),
            requires: vec![],
            imports: HashMap::new(),
            refer_all: vec![],
            as_aliases: vec![],
            core_excludes: vec!["update".to_string(), "map".to_string()],
        };
        meta.refers
            .insert("cmap".to_string(), "clojure.core/map".to_string());
        index.insert_file(meta, vec![], vec![]);

        assert!(
            resolve_symbol(&index, "update", "my.ns", Dialect::Clj).is_none(),
            "an excluded core name must not resolve to clojure.core"
        );
        match resolve_symbol(&index, "cmap", "my.ns", Dialect::Clj) {
            Some(ResolvedSymbol::Core(core)) => assert_eq!(core.name, "map"),
            other => panic!("cmap should resolve to core map, got {:?}", other.is_some()),
        }
    }

    fn index_with(symbols: Vec<Symbol>) -> Index {
        let index = Index::new();
        index.insert_file(
            NsMeta {
                name: "my.ns".to_string(),
                file: PathBuf::from("a.clj"),
                aliases: HashMap::new(),
                refers: HashMap::new(),
                requires: vec![],
                imports: HashMap::new(),
                refer_all: vec![],
                as_aliases: vec![],
                core_excludes: vec![],
            },
            symbols,
            vec![],
        );
        index
    }

    #[test]
    fn resolve_symbol_navigates_factory_to_record() {
        let index = index_with(vec![sym("DB", "my.ns", DefKind::Defrecord)]);
        for factory in ["map->DB", "->DB"] {
            match resolve_symbol(&index, factory, "my.ns", Dialect::Clj) {
                Some(ResolvedSymbol::Project(s)) => assert_eq!(s.name, "DB"),
                other => panic!("{} did not resolve to DB: {:?}", factory, other),
            }
        }
    }

    #[test]
    fn resolve_factory_ignores_non_record_targets() {
        // A plain fn named `foo` must not be reachable via `->foo`.
        let index = index_with(vec![sym("foo", "my.ns", DefKind::Defn)]);
        assert!(resolve_symbol(&index, "->foo", "my.ns", Dialect::Clj).is_none());
    }

    #[test]
    fn map_constructor_is_record_only() {
        // deftype generates `->T` but no `map->T`.
        let index = index_with(vec![sym("T", "my.ns", DefKind::Deftype)]);
        assert!(matches!(
            resolve_symbol(&index, "->T", "my.ns", Dialect::Clj),
            Some(ResolvedSymbol::Project(_))
        ));
        assert!(resolve_symbol(&index, "map->T", "my.ns", Dialect::Clj).is_none());
    }

    #[test]
    fn local_constructor_shadows_core() {
        // A local record generating a ctor that collides with clojure.core
        // (e.g. `->Eduction`) must resolve to the local record, not core.
        let mut index = index_with(vec![sym("Foo", "my.ns", DefKind::Defrecord)]);
        index.core_symbols = vec![CoreSymbol {
            name: "->Foo".to_string(),
            params: String::new(),
            doc: String::new(),
        }];
        match resolve_symbol(&index, "->Foo", "my.ns", Dialect::Clj) {
            Some(ResolvedSymbol::Project(s)) => assert_eq!(s.name, "Foo"),
            other => panic!("local ctor did not shadow core: {:?}", other),
        }
    }

    #[test]
    fn resolve_symbol_navigates_referred_factory() {
        let index = Index::new();
        // The record lives in `recs`.
        index.insert_file(
            NsMeta {
                name: "recs".to_string(),
                file: PathBuf::from("recs.clj"),
                aliases: HashMap::new(),
                refers: HashMap::new(),
                requires: vec![],
                imports: HashMap::new(),
                refer_all: vec![],
                as_aliases: vec![],
                core_excludes: vec![],
            },
            vec![sym("DB", "recs", DefKind::Defrecord)],
            vec![],
        );
        // `app` refers the (un-indexed) constructors.
        let mut refers = HashMap::new();
        refers.insert("->DB".to_string(), "recs/->DB".to_string());
        refers.insert("map->DB".to_string(), "recs/map->DB".to_string());
        index.insert_file(
            NsMeta {
                name: "app".to_string(),
                file: PathBuf::from("app.clj"),
                aliases: HashMap::new(),
                refers,
                requires: vec![],
                imports: HashMap::new(),
                refer_all: vec![],
                as_aliases: vec![],
                core_excludes: vec![],
            },
            vec![],
            vec![],
        );

        for factory in ["->DB", "map->DB"] {
            match resolve_symbol(&index, factory, "app", Dialect::Clj) {
                Some(ResolvedSymbol::Project(s)) => assert_eq!(s.name, "DB"),
                other => panic!("referred {} did not resolve: {:?}", factory, other),
            }
        }
    }

    #[test]
    fn resolve_symbol_falls_back_to_refer_all_namespaces() {
        let index = Index::new();
        // `lib` holds a public fn, a private one, and a record.
        index.insert_file(
            NsMeta {
                name: "lib".to_string(),
                file: PathBuf::from("lib.clj"),
                aliases: HashMap::new(),
                refers: HashMap::new(),
                requires: vec![],
                imports: HashMap::new(),
                refer_all: vec![],
                as_aliases: vec![],
                core_excludes: vec![],
            },
            vec![
                sym("public-fn", "lib", DefKind::Defn),
                sym("secret", "lib", DefKind::DefnPrivate),
                sym("DB", "lib", DefKind::Defrecord),
            ],
            vec![],
        );
        // `app` pulls `lib` in wholesale and defines a name that collides.
        index.insert_file(
            NsMeta {
                name: "app".to_string(),
                file: PathBuf::from("app.clj"),
                aliases: HashMap::new(),
                refers: HashMap::new(),
                requires: vec!["lib".to_string()],
                imports: HashMap::new(),
                refer_all: vec!["lib".to_string()],
                as_aliases: vec![],
                core_excludes: vec![],
            },
            vec![sym("public-fn", "app", DefKind::Defn)],
            vec![],
        );

        // A bare public name resolves into the refer-all namespace, and so does
        // its generated record constructor.
        for (word, ns) in [("DB", "lib"), ("->DB", "lib"), ("map->DB", "lib")] {
            match resolve_symbol(&index, word, "app", Dialect::Clj) {
                Some(ResolvedSymbol::Project(s)) => assert_eq!(s.ns, ns, "{}", word),
                other => panic!("{} did not resolve: {:?}", word, other),
            }
        }
        // Private vars are not referred, so a bare name never reaches one.
        assert!(resolve_symbol(&index, "secret", "app", Dialect::Clj).is_none());
        // The current namespace shadows a refer-all name, as it does in Clojure.
        match resolve_symbol(&index, "public-fn", "app", Dialect::Clj) {
            Some(ResolvedSymbol::Project(s)) => assert_eq!(s.fqn, "app/public-fn"),
            other => panic!("current ns did not shadow refer-all: {:?}", other),
        }
    }

    fn core_entry(name: &str) -> CoreSymbol {
        CoreSymbol {
            name: name.to_string(),
            params: String::new(),
            doc: String::new(),
        }
    }

    #[test]
    fn letgo_core_is_the_bare_word_builtin() {
        // let-go project: `core/map` indexed from .lg source, marker set. Bare
        // `map` must resolve to that, not the static clojure.core builtin even
        // when the static list also carries `map`.
        let mut index = index_with(vec![sym("map", "core", DefKind::Defn)]);
        index.core_symbols = vec![core_entry("map")];
        index.mark_letgo_core();

        match resolve_symbol(&index, "map", "app", Dialect::Clj) {
            Some(ResolvedSymbol::Project(s)) => assert_eq!(s.fqn, "core/map"),
            other => panic!("bare map did not resolve to let-go core/map: {:?}", other),
        }
    }

    #[test]
    fn without_letgo_marker_bare_word_uses_static_core() {
        // No marker → unchanged behavior: bare `map` falls through to the
        // static clojure.core list.
        let mut index = index_with(vec![]);
        index.core_symbols = vec![core_entry("map")];

        match resolve_symbol(&index, "map", "app", Dialect::Clj) {
            Some(ResolvedSymbol::Core(c)) => assert_eq!(c.name, "map"),
            other => panic!("expected static Core(map): {:?}", other),
        }
    }

    #[test]
    fn letgo_marker_skips_static_core_for_missing_builtin() {
        // Marker set but `core/map` not indexed (e.g. a go-only primitive):
        // resolution must NOT fall back to the static clojure.core list, which
        // would mis-navigate to an absent clojure JAR.
        let mut index = index_with(vec![]);
        index.core_symbols = vec![core_entry("map")];
        index.mark_letgo_core();

        assert!(resolve_symbol(&index, "map", "app", Dialect::Clj).is_none());
    }

    #[test]
    fn letgo_special_form_resolves_for_hover() {
        let index = index_with(vec![]);
        index.mark_letgo_core();
        match resolve_symbol(&index, "if", "app", Dialect::Clj) {
            Some(ResolvedSymbol::SpecialForm(sf)) => assert_eq!(sf.name, "if"),
            other => panic!("expected SpecialForm(if): {:?}", other),
        }
    }

    #[test]
    fn letgo_native_resolves_with_borrowed_core_entry() {
        // `count` is a native (no `.lg` source); with the marker set it resolves
        // to LetgoNative carrying the clojure.core entry for hover text.
        let mut index = index_with(vec![]);
        index.core_symbols = vec![core_entry("count")];
        index.mark_letgo_core();
        match resolve_symbol(&index, "count", "app", Dialect::Clj) {
            Some(ResolvedSymbol::LetgoNative(c)) => assert_eq!(c.name, "count"),
            other => panic!("expected LetgoNative(count): {:?}", other),
        }
    }

    #[test]
    fn clojure_special_form_resolves_for_hover() {
        // Clojure project (no let-go marker): `if` resolves to a special form
        // (hover-only), while a clojure.core fn still resolves to Core.
        let mut index = index_with(vec![]);
        index.core_symbols = vec![core_entry("count")];
        match resolve_symbol(&index, "if", "app", Dialect::Clj) {
            Some(ResolvedSymbol::SpecialForm(sf)) => assert_eq!(sf.name, "if"),
            other => panic!("expected SpecialForm(if): {:?}", other),
        }
        match resolve_symbol(&index, "count", "app", Dialect::Clj) {
            Some(ResolvedSymbol::Core(c)) => assert_eq!(c.name, "count"),
            other => panic!("expected Core(count): {:?}", other),
        }
    }

    #[test]
    fn resolve_symbol_reads_the_asking_halfs_ns_form() {
        // `app.shared` split across `.clj` and `.cljs`, each half aliasing `u`
        // to its own platform's utilities.
        let index = Index::new();
        for (ns, file) in [("util.jvm", "util/jvm.clj"), ("util.js", "util/js.cljs")] {
            let mut helper = sym("helper", ns, DefKind::Defn);
            helper.file = PathBuf::from(file);
            let meta = NsMeta {
                name: ns.to_string(),
                file: PathBuf::from(file),
                aliases: HashMap::new(),
                refers: HashMap::new(),
                requires: vec![],
                imports: HashMap::new(),
                refer_all: vec![],
                as_aliases: vec![],
                core_excludes: vec![],
            };
            index.insert_file(meta, vec![helper], vec![]);
        }
        for (file, target) in [
            ("app/shared.clj", "util.jvm"),
            ("app/shared.cljs", "util.js"),
        ] {
            let meta = NsMeta {
                name: "app.shared".to_string(),
                file: PathBuf::from(file),
                aliases: HashMap::from([("u".to_string(), target.to_string())]),
                refers: HashMap::new(),
                requires: vec![target.to_string()],
                imports: HashMap::new(),
                refer_all: vec![],
                as_aliases: vec![],
                core_excludes: vec![],
            };
            index.insert_file(meta, vec![], vec![]);
        }
        for (dialect, ns) in [(Dialect::Clj, "util.jvm"), (Dialect::Cljs, "util.js")] {
            match resolve_symbol(&index, "u/helper", "app.shared", dialect) {
                Some(ResolvedSymbol::Project(s)) => assert_eq!(s.ns, ns, "{dialect:?}"),
                other => panic!("{dialect:?}: u/helper did not resolve: {other:?}"),
            }
        }
    }

    #[test]
    fn cljs_core_only_names_resolve_in_a_cljs_file() {
        // `cljs.core` indexed from the ClojureScript JAR: `not-native` is not in
        // the static clojure.core list, `map` is.
        let mut index = index_with(vec![]);
        index.core_symbols = vec![core_entry("map")];
        let file = "/m2/cljs.jar!/cljs/core.cljs";
        let lib = |name: &str| {
            let mut s = sym(name, "cljs.core", DefKind::Def);
            s.file = PathBuf::from(file);
            s.source = SymbolSource::Jar(PathBuf::from("/m2/cljs.jar"));
            s
        };
        let meta = NsMeta {
            name: "cljs.core".to_string(),
            file: PathBuf::from(file),
            aliases: HashMap::new(),
            refers: HashMap::new(),
            requires: vec![],
            imports: HashMap::new(),
            refer_all: vec![],
            as_aliases: vec![],
            core_excludes: vec![],
        };
        index.insert_lib_file(meta, vec![lib("not-native"), lib("map")]);

        match resolve_symbol(&index, "not-native", "my.ns", Dialect::Cljs) {
            Some(ResolvedSymbol::Project(s)) => assert_eq!(s.fqn, "cljs.core/not-native"),
            other => panic!("not-native did not resolve: {other:?}"),
        }
        // A `.clj` file has no `cljs.core`.
        assert!(resolve_symbol(&index, "not-native", "my.ns", Dialect::Clj).is_none());
        // A shared name keeps the static entry.
        assert!(matches!(
            resolve_symbol(&index, "map", "my.ns", Dialect::Cljs),
            Some(ResolvedSymbol::Core(_))
        ));
    }

    #[test]
    fn project_var_shadows_clojure_special_form() {
        // A project var named `new` wins over the `new` special form.
        let index = index_with(vec![sym("new", "my.ns", DefKind::Defn)]);
        match resolve_symbol(&index, "new", "my.ns", Dialect::Clj) {
            Some(ResolvedSymbol::Project(s)) => assert_eq!(s.name, "new"),
            other => panic!("project `new` should win: {:?}", other),
        }
    }
}
