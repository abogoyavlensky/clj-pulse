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
    let paths_idx = contents.find(":paths")?;
    let after_paths = &contents[paths_idx + 6..];
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
