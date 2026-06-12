pub mod completion;
pub mod definition;
pub mod hover;
pub mod references;
pub mod signature;
pub mod symbols;

use crate::index::{CoreSymbol, Index, Symbol};

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
    }

    None
}
