use std::path::{Path, PathBuf};

pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        if dir.join("deps.edn").exists() || dir.join("project.clj").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn source_paths(root: &Path) -> Vec<PathBuf> {
    let deps_edn = root.join("deps.edn");
    if let Ok(contents) = std::fs::read_to_string(&deps_edn) {
        if let Some(paths) = parse_paths_from_deps_edn(&contents) {
            return paths.into_iter().map(|p| root.join(p)).collect();
        }
    }

    vec![root.join("src"), root.join("test")]
}

fn parse_paths_from_deps_edn(contents: &str) -> Option<Vec<String>> {
    let paths_idx = find_top_level_paths(contents)?;
    let after_paths = &contents[paths_idx + ":paths".len()..];
    let bracket_start = after_paths.find('[')?;
    let bracket_end = after_paths[bracket_start..].find(']')?;
    let inside = &after_paths[bracket_start + 1..bracket_start + bracket_end];

    let paths: Vec<String> = inside
        .split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, s)| s.to_string())
        .collect();

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// Finds `:paths` at the top level of the deps.edn map. A plain substring
/// search would match `:paths` nested inside `:aliases` (e.g. a tools.build
/// `:build` alias), so track nesting depth and skip strings and comments.
fn find_top_level_paths(contents: &str) -> Option<usize> {
    let bytes = contents.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'"' => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b';' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => depth -= 1,
            b':' if depth == 1 && contents[i..].starts_with(":paths") => {
                let next = bytes.get(i + ":paths".len());
                let at_boundary = next
                    .map(|c| c.is_ascii_whitespace() || *c == b'[')
                    .unwrap_or(true);
                if at_boundary {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_top_level_paths() {
        let edn = r#"{:paths ["src" "resources"]
                      :deps {org.clojure/clojure {:mvn/version "1.11.1"}}}"#;
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string(), "resources".to_string()])
        );
    }

    #[test]
    fn test_alias_paths_are_ignored() {
        // tools.build convention: :paths inside the :build alias must not be
        // mistaken for the project's source paths.
        let edn = r#"{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}
                      :aliases {:build {:paths ["build"]
                                        :deps {io.github.clojure/tools.build {:mvn/version "0.9.6"}}}}}"#;
        assert_eq!(parse_paths_from_deps_edn(edn), None);
    }

    #[test]
    fn test_top_level_paths_after_aliases() {
        let edn = r#"{:aliases {:build {:paths ["build"]}}
                      :paths ["src" "lib"]}"#;
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string(), "lib".to_string()])
        );
    }

    #[test]
    fn test_paths_in_comment_ignored() {
        let edn = ";; :paths [\"nope\"]\n{:paths [\"src\"]}";
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string()])
        );
    }

    #[test]
    fn test_paths_in_string_ignored() {
        let edn = r#"{:description ":paths [\"nope\"]"
                      :paths ["src"]}"#;
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string()])
        );
    }

    #[test]
    fn test_no_paths_returns_none() {
        let edn = r#"{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}}"#;
        assert_eq!(parse_paths_from_deps_edn(edn), None);
    }

    #[test]
    fn test_extra_paths_not_matched() {
        let edn = r#"{:extra-paths ["dev"]}"#;
        assert_eq!(parse_paths_from_deps_edn(edn), None);
    }
}
