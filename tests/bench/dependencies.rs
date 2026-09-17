//! Benchmark-only ground truth, read from the prepared classpath before timing.
//! This deliberately does not use either server's extractor or analysis output.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use tower_lsp::lsp_types::Url;
use tree_sitter::{Node, Parser};

#[derive(Debug, Serialize)]
pub struct Target {
    pub artifact: PathBuf,
    /// Relative entry for a JAR, absolute file for a source directory.
    pub source: String,
    pub symbol: String,
    pub line: u32,
    pub character: u32,
}

impl Target {
    pub fn accepts(&self, result: &Value) -> bool {
        let matches = |loc: &Value| {
            let Some(uri) = loc["uri"].as_str().or_else(|| loc["targetUri"].as_str()) else {
                return false;
            };
            let range = loc.get("targetSelectionRange").or_else(|| loc.get("range"));
            let Some(range) = range else { return false };
            let point = |key: &str| -> Option<(u64, u64)> {
                Some((
                    range[key]["line"].as_u64()?,
                    range[key]["character"].as_u64()?,
                ))
            };
            let (Some(start), Some(end)) = (point("start"), point("end")) else {
                return false;
            };
            let expected = (self.line as u64, self.character as u64);
            self.matches_uri(uri) && start <= expected && expected < end
        };
        match result {
            Value::Array(items) => items.iter().any(matches),
            other => matches(other),
        }
    }

    fn matches_uri(&self, uri: &str) -> bool {
        if self.artifact.is_dir() {
            return Url::parse(uri).ok().and_then(|u| u.to_file_path().ok())
                == Some(PathBuf::from(&self.source));
        }
        let decoded = percent_encoding::percent_decode_str(uri).decode_utf8_lossy();
        let pair = decoded
            .strip_prefix("jar:")
            .and_then(|s| s.split_once("!/"))
            .or_else(|| {
                decoded
                    .strip_prefix("zipfile:")
                    .and_then(|s| s.split_once("::"))
            });
        let Some((archive, entry)) = pair else {
            return false;
        };
        // jar:file:///path and zipfile:///path::entry name the same archive.
        let archive = archive.strip_prefix("file://").unwrap_or(archive);
        let archive = if archive.starts_with("///") {
            &archive[2..]
        } else {
            archive
        };
        entry == self.source && Path::new(archive) == self.artifact
    }
}

#[derive(Debug, Serialize)]
pub struct Exclusion {
    pub artifact: PathBuf,
    pub reason: String,
}

#[derive(Default, Debug, Serialize)]
pub struct Inventory {
    pub entries: usize,
    pub source_dependencies: usize,
    pub targets: Vec<Target>,
    pub excluded: Vec<Exclusion>,
    pub errors: Vec<Exclusion>,
}

impl Inventory {
    pub fn discover(root: &Path, entries: Vec<PathBuf>) -> Self {
        let mut out = Self::default();
        let mut seen_paths = BTreeSet::new();
        let mut libraries = Vec::new();
        for entry in entries {
            let path = match entry.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    out.errors.push(Exclusion {
                        artifact: entry,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            if !seen_paths.insert(path.clone()) {
                continue;
            }
            out.entries += 1;
            let files = match sources(&path) {
                Ok(files) => files,
                Err(e) => {
                    out.errors.push(Exclusion {
                        artifact: path,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            libraries.push((path, files));
        }
        // Clojure tries foo.clj across the whole classpath before foo.cljc.
        // Within one extension, the first classpath entry owns the resource.
        let mut owners = BTreeMap::new();
        for (index, (_, files)) in libraries.iter().enumerate() {
            for (name, _) in files {
                let (base, ext) = name.rsplit_once('.').unwrap();
                let rank = (usize::from(ext != "clj"), index);
                owners
                    .entry(base.to_string())
                    .and_modify(|old| {
                        if rank < *old {
                            *old = rank;
                        }
                    })
                    .or_insert(rank);
            }
        }
        for (index, (path, files)) in libraries.into_iter().enumerate() {
            let project = path.is_dir() && path.starts_with(root);
            if !project && !files.is_empty() {
                out.source_dependencies += 1;
            }
            let mut target = None;
            for (name, source) in &files {
                let (base, ext) = name.rsplit_once('.').unwrap();
                let owner = owners.get(base) == Some(&(usize::from(ext != "clj"), index));
                if project || !owner || target.is_some() {
                    continue;
                }
                if let Some((ns, var, line, character)) = public_definition(source) {
                    let relative = format!(
                        "{}.{}",
                        ns.replace('-', "_").replace('.', "/"),
                        Path::new(name).extension().unwrap().to_string_lossy()
                    );
                    if relative != *name {
                        continue;
                    }
                    target = Some(Target {
                        artifact: path.clone(),
                        source: if path.is_dir() {
                            path.join(name).to_string_lossy().into_owned()
                        } else {
                            name.clone()
                        },
                        symbol: format!("{ns}/{var}"),
                        line,
                        character,
                    });
                }
            }
            if let Some(target) = target {
                out.targets.push(target);
            } else {
                let reason = if project {
                    "project source root"
                } else if files.is_empty() {
                    "no .clj or .cljc sources"
                } else {
                    "no unshadowed ordinary public definition"
                };
                out.excluded.push(Exclusion {
                    artifact: path,
                    reason: reason.into(),
                });
            }
        }
        out
    }

    /// Unsaved buffer: it never modifies the corpus or survives a run.
    pub fn buffer(&self) -> String {
        let mut text = String::from("(ns clj-pulse-benchmark-dependencies\n  (:require\n");
        for target in &self.targets {
            let ns = target.symbol.split_once('/').unwrap().0;
            text.push_str(&format!("    [{ns}]\n"));
        }
        text.push_str("  ))\n");
        for target in &self.targets {
            text.push_str(&format!("{}\n", target.symbol));
        }
        text
    }

    pub fn probe_line(&self, index: usize) -> u32 {
        (self.targets.len() + 3 + index) as u32
    }
}

/// Lexical resource order makes selection repeatable. Read errors are reported
/// as discovery failures, rather than silently reducing the coverage denominator.
fn sources(path: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let is_source = |p: &str| p.ends_with(".clj") || p.ends_with(".cljc");
    let mut files = Vec::new();
    if path.is_dir() {
        for entry in ignore::WalkBuilder::new(path)
            .hidden(false)
            .git_ignore(false)
            .git_exclude(false)
            .git_global(false)
            .build()
        {
            let entry = entry?;
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let name = entry
                    .path()
                    .strip_prefix(path)?
                    .to_string_lossy()
                    .replace('\\', "/");
                if is_source(&name) {
                    files.push((name, std::fs::read_to_string(entry.path())?));
                }
            }
        }
    } else if path.extension().is_some_and(|e| e == "jar") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if is_source(entry.name()) {
                let name = entry.name().to_string();
                let mut source = String::new();
                entry.read_to_string(&mut source)?;
                files.push((name, source));
            }
        }
    } else {
        anyhow::bail!("not a source directory or JAR");
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk())
        .filter(|n| n.kind() != "comment")
        .collect()
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

/// Conservative syntax subset: top-level ns plus unqualified def/defn/defmacro.
/// Metadata on names is excluded, so private/type-hinted/reader-specific forms
/// cannot accidentally become ambiguous ground truth. Never descend into quotes,
/// comments, discards, reader conditionals, or macro bodies.
fn public_definition(source: &str) -> Option<(String, String, u32, u32)> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_clojure::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let forms = children(tree.root_node());
    let mut namespace = None;
    for form in forms {
        if form.kind() != "list_lit" || form.has_error() {
            continue;
        }
        let kids = children(form);
        let (Some(head), Some(name)) = (kids.first(), kids.get(1)) else {
            continue;
        };
        if head.kind() != "sym_lit" || name.kind() != "sym_lit" {
            continue;
        }
        let head = text(*head, source);
        // Namespace metadata is harmless; symbol field excludes the metadata.
        if head == "ns" {
            namespace = name
                .child_by_field_name("name")
                .map(|n| text(n, source).to_string());
            continue;
        }
        if !matches!(head, "def" | "defn" | "defmacro") {
            continue;
        }
        let name_text = text(*name, source);
        if name_text.contains(['^', '/', '\n', ' ']) || name_text.is_empty() {
            continue;
        }
        // An attr map can make even an unannotated defn private. Avoid all
        // attr-map definitions, not just the common spelling of :private.
        let after_name = kids.iter().skip(2).find(|n| n.kind() != "str_lit");
        if head != "def" && after_name.is_some_and(|n| n.kind() == "map_lit") {
            continue;
        }
        let ns = namespace.as_ref()?;
        let point = name.start_position();
        let col = source[name.start_byte() - point.column..name.start_byte()]
            .encode_utf16()
            .count();
        return Some((
            ns.clone(),
            name_text.to_string(),
            point.row as u32,
            col as u32,
        ));
    }
    None
}

#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    pub symbol: String,
    pub ready_ms: Option<u64>,
    pub attempts: usize,
    pub last_failure: Option<String>,
}

impl Outcome {
    pub fn new(symbol: String) -> Self {
        Self {
            symbol,
            ready_ms: None,
            attempts: 0,
            last_failure: None,
        }
    }
    pub fn observe(&mut self, response: &Value, accepted: bool, elapsed_ms: u64) {
        if self.ready_ms.is_some() {
            return;
        }
        if accepted {
            self.ready_ms = Some(elapsed_ms);
            self.last_failure = None;
        } else {
            self.last_failure = Some(if let Some(error) = response.get("error") {
                format!("JSON-RPC error: {error}")
            } else {
                format!("no matching definition location: {}", response["result"])
            });
        }
    }
}

pub fn ready_ms(outcomes: &[Outcome]) -> Option<u64> {
    if outcomes.is_empty() {
        return None;
    }
    outcomes
        .iter()
        .map(|o| o.ready_ms)
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn jar(path: &Path, files: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, source) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(source.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn independent_selection_ignores_nondefinitions_and_private_forms() {
        let source = "(ns ^{:doc \"hi\"} sample.core)\n#_(def skipped 1)\n'(def quoted 1)\n(comment (def hidden 1))\n(defn- secret [] 1)\n(def ^:private private 1)\n(defn attr {:private true} [] 1)\n(defn public [] 1)";
        let (ns, var, line, col) = public_definition(source).unwrap();
        assert_eq!(
            (ns.as_str(), var.as_str(), line, col),
            ("sample.core", "public", 7, 6)
        );
        assert!(public_definition("(ns sample) #?(:clj (def conditional 1))").is_none());
    }

    #[test]
    fn inventory_accounts_for_sources_shadowing_and_failures() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let local = temp.path().join("local");
        std::fs::create_dir_all(root.join("src/sample")).unwrap();
        std::fs::create_dir_all(local.join("local")).unwrap();
        std::fs::write(
            root.join("src/sample/core.clj"),
            "(ns sample.core) (def project 1)",
        )
        .unwrap();
        std::fs::write(
            local.join("local/core.cljc"),
            "(ns local.core) (def local 1)",
        )
        .unwrap();
        let library = temp.path().join("library.jar");
        jar(
            &library,
            &[
                ("sample/core.clj", "(ns sample.core) (def shadowed 1)"),
                ("other/core.clj", "(ns other.core) (def library 1)"),
            ],
        );
        let java = temp.path().join("java.jar");
        jar(&java, &[("Thing.class", "bytes")]);
        let empty = temp.path().join("empty.jar");
        jar(&empty, &[("empty.clj", "(ns empty) (defn- private [] 1)")]);
        let missing = temp.path().join("missing.jar");
        let inv = Inventory::discover(
            &root,
            vec![
                root.join("src"),
                library.clone(),
                library,
                local,
                java,
                empty,
                missing,
            ],
        );
        assert_eq!(inv.entries, 5);
        assert_eq!(inv.source_dependencies, 3);
        assert_eq!(
            inv.targets
                .iter()
                .map(|t| t.symbol.as_str())
                .collect::<Vec<_>>(),
            vec!["other.core/library", "local.core/local"]
        );
        assert_eq!(inv.excluded.len(), 3);
        assert_eq!(inv.errors.len(), 1);
        for (i, target) in inv.targets.iter().enumerate() {
            assert_eq!(
                inv.buffer().lines().nth(inv.probe_line(i) as usize),
                Some(target.symbol.as_str())
            );
        }
    }

    #[test]
    fn clj_wins_over_earlier_cljc_and_same_extension_uses_classpath_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let portable = temp.path().join("portable.jar");
        let jvm = temp.path().join("jvm.jar");
        let shadowed = temp.path().join("shadowed.jar");
        jar(&portable, &[("foo.cljc", "(ns foo) (def portable 1)")]);
        jar(&jvm, &[("foo.clj", "(ns foo) (def jvm 1)")]);
        jar(&shadowed, &[("foo.clj", "(ns foo) (def later 1)")]);
        let inv = Inventory::discover(&root, vec![portable, jvm.clone(), shadowed]);
        assert_eq!(inv.targets.len(), 1);
        assert_eq!(inv.targets[0].artifact, jvm);
        assert_eq!(inv.targets[0].symbol, "foo/jvm");
        assert_eq!(inv.source_dependencies, 3);
    }

    #[test]
    fn locations_require_the_exact_artifact_entry_and_definition() {
        let t = Target {
            artifact: PathBuf::from("/some lib/a.jar"),
            source: "foo/core.clj".into(),
            symbol: "foo.core/a".into(),
            line: 4,
            character: 6,
        };
        let loc = |uri: &str, line| {
            json!({"uri":uri,"range":{"start":{"line":line,"character":6},
            "end":{"line":line,"character":7}}})
        };
        assert!(t.accepts(&loc("jar:file:///some%20lib/a.jar!/foo/core.clj", 4)));
        assert!(t.accepts(&json!([loc(
            "zipfile:///some%20lib/a.jar::foo/core.clj",
            4
        )])));
        assert!(!t.accepts(&loc("jar:file:///some%20lib/wrong.jar!/foo/core.clj", 4)));
        assert!(!t.accepts(&loc("jar:file:///some%20lib/a.jar!/foo/core.clj", 5)));
        assert!(!t.accepts(&Value::Null));
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("foo.clj");
        let t = Target {
            artifact: temp.path().into(),
            source: file.to_string_lossy().into(),
            ..t
        };
        let uri = Url::from_file_path(&file).unwrap();
        let link = json!([{"targetUri":uri,"targetSelectionRange":loc("",4)["range"]}]);
        assert!(t.accepts(&link));
    }

    #[test]
    fn partial_and_empty_coverage_have_no_readiness_time() {
        assert_eq!(ready_ms(&[]), None);
        let mut outcomes = vec![Outcome::new("a/x".into()), Outcome::new("b/y".into())];
        outcomes[0].observe(&json!({"result":{}}), true, 12);
        outcomes[1].observe(&json!({"error":{"code":-1}}), false, 14);
        assert_eq!(ready_ms(&outcomes), None);
        assert!(outcomes[1]
            .last_failure
            .as_ref()
            .unwrap()
            .contains("JSON-RPC error"));
        outcomes[1].last_failure = Some("startup deadline exceeded".into());
        assert_eq!(ready_ms(&outcomes), None);
        outcomes[1].observe(&json!({"result":{}}), true, 20);
        assert_eq!(ready_ms(&outcomes), Some(20));
    }
}
