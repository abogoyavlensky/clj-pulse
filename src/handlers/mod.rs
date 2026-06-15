pub mod code_action;
pub mod completion;
pub mod definition;
pub mod hover;
pub mod references;
pub mod signature;
pub mod symbols;

use crate::index::{CoreSymbol, DefKind, Index, Symbol};

#[derive(Debug, Clone)]
pub enum ResolvedSymbol {
    Project(Symbol),
    Core(CoreSymbol),
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
            }
        }

        if let Some(sym) = index.lookup_in_ns(current_ns, word) {
            return Some(ResolvedSymbol::Project(sym));
        }

        if let Some(core) = index.core_symbols.iter().find(|c| c.name == word) {
            return Some(ResolvedSymbol::Core(core.clone()));
        }

        if let Some(sym) = resolve_factory(index, current_ns, word) {
            return Some(ResolvedSymbol::Project(sym));
        }
    }

    None
}

/// The record/type name a constructor function refers to: `map->DB` and `->DB`
/// both target `DB`. `None` for non-factory names (and the bare `->`/`map->`).
fn factory_target_name(name: &str) -> Option<&str> {
    let target = name
        .strip_prefix("map->")
        .or_else(|| name.strip_prefix("->"))?;
    (!target.is_empty()).then_some(target)
}

/// Resolves an auto-generated record/type constructor (`->X` / `map->X`) to the
/// `defrecord`/`deftype` `X` it builds, so navigation/hover land on the type.
/// Gated on the target's kind so a plain fn named `->foo` is never hijacked.
fn resolve_factory(index: &Index, ns: &str, name: &str) -> Option<Symbol> {
    let target = factory_target_name(name)?;
    let sym = index.lookup_in_ns(ns, target)?;
    matches!(sym.kind, DefKind::Defrecord | DefKind::Deftype).then_some(sym)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{NsMeta, SymbolSource};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tower_lsp::lsp_types::Range;

    #[test]
    fn factory_target_name_strips_known_prefixes() {
        assert_eq!(factory_target_name("map->DB"), Some("DB"));
        assert_eq!(factory_target_name("->DB"), Some("DB"));
        assert_eq!(factory_target_name("plain"), None);
        assert_eq!(factory_target_name("->"), None);
        assert_eq!(factory_target_name("map->"), None);
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
}
