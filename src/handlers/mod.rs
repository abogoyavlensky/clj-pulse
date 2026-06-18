pub mod code_action;
pub mod completion;
pub mod definition;
pub mod hover;
pub mod letgo_builtins;
mod letgo_native_names;
pub mod references;
pub mod signature;
pub mod symbols;

use crate::index::{CoreSymbol, DefKind, Index, Symbol};

#[derive(Debug, Clone)]
pub enum ResolvedSymbol {
    Project(Symbol),
    Core(CoreSymbol),
    /// A let-go special form (compiler intrinsic) — hover-only, never navigable.
    SpecialForm(&'static letgo_builtins::SpecialForm),
    /// A let-go native core fn (Go `ns.Def`) — hover-only; doc/arglists borrowed
    /// from the clojure.core table.
    LetgoNative(CoreSymbol),
}

pub fn resolve_symbol(index: &Index, word: &str, current_ns: &str) -> Option<ResolvedSymbol> {
    let ns_meta = index.ns_meta(current_ns);

    if let Some((alias, name)) = word.split_once('/') {
        // Qualified symbol: alias/name
        let full_ns = ns_meta
            .as_ref()
            .and_then(|m| m.aliases.get(alias))
            .map(|s| s.as_str())
            .unwrap_or(alias);

        if let Some(sym) = index.lookup_in_ns(full_ns, name) {
            return Some(ResolvedSymbol::Project(sym));
        }

        if let Some(sym) = resolve_factory(index, full_ns, name) {
            return Some(ResolvedSymbol::Project(sym));
        }
    } else {
        // Bare symbol: check refers, then current ns, then core
        if let Some(meta) = &ns_meta {
            if let Some(fqn) = meta.refers.get(word) {
                if let Some(sym) = index.lookup(fqn) {
                    return Some(ResolvedSymbol::Project(sym));
                }
                // A referred record constructor (`:refer [->DB map->DB]`): the
                // ctor fqn is not indexed, but its record is — resolve it in the
                // referred namespace.
                if let Some((refer_ns, _)) = fqn.rsplit_once('/') {
                    if let Some(sym) = resolve_factory(index, refer_ns, word) {
                        return Some(ResolvedSymbol::Project(sym));
                    }
                }
            }
        }

        if let Some(sym) = index.lookup_in_ns(current_ns, word) {
            return Some(ResolvedSymbol::Project(sym));
        }

        // A locally generated record/type constructor shadows a clojure.core
        // symbol of the same name, so resolve it before the core fallback.
        if let Some(sym) = resolve_factory(index, current_ns, word) {
            return Some(ResolvedSymbol::Project(sym));
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
            if let Some(sf) = letgo_builtins::special_form(word) {
                return Some(ResolvedSymbol::SpecialForm(sf));
            }
            // Native core fns (Go `ns.Def`, e.g. `count`/`str`) also have no
            // `.lg` source; borrow the clojure.core entry for hover/completion.
            if letgo_builtins::is_native(word) {
                if let Some(core) = index.core_symbols.iter().find(|c| c.name == word) {
                    return Some(ResolvedSymbol::LetgoNative(core.clone()));
                }
            }
            return None;
        }

        if let Some(core) = index.core_symbols.iter().find(|c| c.name == word) {
            return Some(ResolvedSymbol::Core(core.clone()));
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

/// Resolves an auto-generated record/type constructor to the `defrecord`/
/// `deftype` it builds, so navigation/hover land on the type. Gated on kind so
/// a plain fn named `->foo` is never hijacked; `map->X` is records-only, since
/// `deftype` generates `->X` but no map constructor.
fn resolve_factory(index: &Index, ns: &str, name: &str) -> Option<Symbol> {
    let (target, is_map_ctor) = factory_target(name)?;
    let sym = index.lookup_in_ns(ns, target)?;
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
            match resolve_symbol(&index, factory, "my.ns") {
                Some(ResolvedSymbol::Project(s)) => assert_eq!(s.name, "DB"),
                other => panic!("{} did not resolve to DB: {:?}", factory, other),
            }
        }
    }

    #[test]
    fn resolve_factory_ignores_non_record_targets() {
        // A plain fn named `foo` must not be reachable via `->foo`.
        let index = index_with(vec![sym("foo", "my.ns", DefKind::Defn)]);
        assert!(resolve_symbol(&index, "->foo", "my.ns").is_none());
    }

    #[test]
    fn map_constructor_is_record_only() {
        // deftype generates `->T` but no `map->T`.
        let index = index_with(vec![sym("T", "my.ns", DefKind::Deftype)]);
        assert!(matches!(
            resolve_symbol(&index, "->T", "my.ns"),
            Some(ResolvedSymbol::Project(_))
        ));
        assert!(resolve_symbol(&index, "map->T", "my.ns").is_none());
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
        match resolve_symbol(&index, "->Foo", "my.ns") {
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
            },
            vec![],
            vec![],
        );

        for factory in ["->DB", "map->DB"] {
            match resolve_symbol(&index, factory, "app") {
                Some(ResolvedSymbol::Project(s)) => assert_eq!(s.name, "DB"),
                other => panic!("referred {} did not resolve: {:?}", factory, other),
            }
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

        match resolve_symbol(&index, "map", "app") {
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

        match resolve_symbol(&index, "map", "app") {
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

        assert!(resolve_symbol(&index, "map", "app").is_none());
    }

    #[test]
    fn letgo_special_form_resolves_for_hover() {
        let index = index_with(vec![]);
        index.mark_letgo_core();
        match resolve_symbol(&index, "if", "app") {
            Some(ResolvedSymbol::SpecialForm(sf)) => assert_eq!(sf.name, "if"),
            other => panic!("expected SpecialForm(if): {:?}", other),
        }
    }

    #[test]
    fn without_letgo_marker_special_form_is_not_resolved() {
        // No marker → `if` is neither a project nor a static-core symbol, so it
        // stays unresolved (behavior unchanged for Clojure projects).
        let index = index_with(vec![]);
        assert!(resolve_symbol(&index, "if", "app").is_none());
    }

    #[test]
    fn letgo_native_resolves_with_borrowed_core_entry() {
        // `count` is a native (no `.lg` source); with the marker set it resolves
        // to LetgoNative carrying the clojure.core entry for hover text.
        let mut index = index_with(vec![]);
        index.core_symbols = vec![core_entry("count")];
        index.mark_letgo_core();
        match resolve_symbol(&index, "count", "app") {
            Some(ResolvedSymbol::LetgoNative(c)) => assert_eq!(c.name, "count"),
            other => panic!("expected LetgoNative(count): {:?}", other),
        }
    }
}
