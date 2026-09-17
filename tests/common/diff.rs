//! Reducing answers to what two of them owe each other, and reporting where
//! they differ. Shared by the soak and the compare gate.
//!
//! Both test binaries compile this module, and neither uses all of it, so
//! `dead_code` is off here rather than per item.
#![allow(dead_code)]

use std::collections::BTreeSet;

use serde_json::Value;

/// One place two answers disagree: the churned server against a fresh one in
/// the soak, clj-pulse against the clj-kondo oracle in the compare gate. The
/// two sides are named by the caller at print time.
pub struct Divergence {
    pub request: String,
    pub site: String,
    pub mine: String,
    pub theirs: String,
}

impl Divergence {
    pub fn print(&self, mine: &str, theirs: &str) {
        let width = mine.len().max(theirs.len()) + 1;
        println!("  {} at {}", self.request, self.site);
        println!("    {:width$} {}", format!("{mine}:"), self.mine);
        println!("    {:width$} {}", format!("{theirs}:"), self.theirs);
    }
}

/// A `Location[]` answer as a set of `uri@line:col-line:col`.
pub fn locations(result: &Value) -> BTreeSet<String> {
    result
        .as_array()
        .map(|items| items.iter().map(location_key).collect())
        .unwrap_or_default()
}

/// A `SymbolInformation[]` answer as a set: name plus where it is. Ranking
/// depends on occurrence counts, which a churned index counts in its own order,
/// so only the set is something two servers owe each other.
pub fn symbol_set(result: &Value) -> BTreeSet<String> {
    result
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    format!(
                        "{} {}",
                        item["name"].as_str().unwrap_or_default(),
                        location_key(&item["location"])
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn location_key(location: &Value) -> String {
    let range = &location["range"];
    format!(
        "{}@{}:{}-{}:{}",
        location["uri"].as_str().unwrap_or_default(),
        range["start"]["line"],
        range["start"]["character"],
        range["end"]["line"],
        range["end"]["character"]
    )
}

/// A JSON answer, short enough to read in a failure report.
pub fn brief(value: &Value) -> String {
    let text = value.to_string();
    if text.len() <= 300 {
        return text;
    }
    // On a character boundary, not on byte 300: a Clojure identifier or a path
    // can be non-ASCII, and slicing through one would panic in the very report
    // that exists to explain a failure.
    let mut cut = 300;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… ({} bytes)", &text[..cut], text.len())
}

/// What one side has that the other does not — the whole set is unreadable and
/// the difference is the finding.
pub fn brief_set(mine: &BTreeSet<String>, theirs: &BTreeSet<String>) -> String {
    let extra: Vec<&String> = mine.difference(theirs).take(5).collect();
    format!(
        "{} entries, {} not in the other: {:?}",
        mine.len(),
        mine.difference(theirs).count(),
        extra
    )
}
