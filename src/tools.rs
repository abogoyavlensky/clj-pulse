//! Where the tools clj-pulse spawns are looked for.
//!
//! An editor started from a desktop launcher (the macOS Dock, a Linux app
//! menu) inherits a PATH without the directories a login shell adds, so a
//! `clj-kondo` or `clojure` CLI installed through Homebrew or mise works in a
//! terminal and is invisible here. Every child process clj-pulse spawns gets
//! PATH plus a short list of well-known install directories, appended *after*
//! the user's own entries so an explicit PATH always wins.
//!
//! `CLJ_PULSE_TOOL_DIRS` (a `PATH`-style list) replaces the built-in list —
//! the e2e suite's way to make discovery deterministic on any machine.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Overrides [`well_known_dirs`] when set: the directories to search after
/// PATH, separated like PATH. Empty means "PATH only".
pub const TOOL_DIRS_ENV: &str = "CLJ_PULSE_TOOL_DIRS";

/// The install directories searched after PATH, existing ones only, in
/// priority order. mise shims come first: a shim picks the tool version the
/// current directory's mise config pins, which is what a project wants.
pub fn well_known_dirs() -> Vec<PathBuf> {
    let candidates = match std::env::var_os(TOOL_DIRS_ENV) {
        Some(list) => std::env::split_paths(&list).collect(),
        None => candidate_dirs(
            home_dir().as_deref(),
            std::env::var_os("MISE_DATA_DIR")
                .map(PathBuf::from)
                .as_deref(),
        ),
    };
    candidates.into_iter().filter(|d| d.is_dir()).collect()
}

/// The built-in candidate list, before the existence filter: mise shims,
/// Homebrew on Apple Silicon, Intel Macs and Linux, then the per-user bin
/// directories cargo and pipx-style installers use.
fn candidate_dirs(home: Option<&Path>, mise_data_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match mise_data_dir {
        Some(data) => dirs.push(data.join("shims")),
        None => {
            if let Some(home) = home {
                dirs.push(home.join(".local/share/mise/shims"));
            }
        }
    }
    if cfg!(unix) {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin"));
    }
    if let Some(home) = home {
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join("bin"));
    }
    dirs
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// PATH as every child process sees it: the inherited value, then the
/// well-known directories it does not already list.
pub fn augmented_path() -> OsString {
    augment(std::env::var_os("PATH").as_deref(), &well_known_dirs())
}

fn augment(path: Option<&OsStr>, extra: &[PathBuf]) -> OsString {
    let mut entries: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    for dir in extra {
        if !entries.iter().any(|e| e == dir) {
            entries.push(dir.clone());
        }
    }
    std::env::join_paths(entries).unwrap_or_default()
}

/// Gives `cmd` the augmented PATH. Applied to every spawn, including the
/// classpath shell, so `clojure -Spath` finds a Homebrew or mise `clojure`.
pub fn apply_env(cmd: &mut tokio::process::Command) {
    cmd.env("PATH", augmented_path());
}

/// Where a program resolves, as an absolute path — `None` when no directory
/// holds an executable of that name. A name with a path separator is the
/// user's explicit choice and is taken as given, relative to `base` when it
/// is relative; a bare name is searched over the augmented PATH, where a
/// relative entry is also read against `base`. Absolute, because the child
/// may run from another directory than the one the name was written for.
pub fn resolve(program: &str, base: &Path) -> Option<PathBuf> {
    if program.contains('/') || program.contains(std::path::MAIN_SEPARATOR) {
        return Some(base.join(program));
    }
    let path = augmented_path();
    std::env::split_paths(&path)
        .map(|dir| base.join(dir))
        .flat_map(|dir| candidates_in(&dir, program))
        .find(|p| is_executable(p))
}

fn candidates_in(dir: &Path, program: &str) -> Vec<PathBuf> {
    let mut out = vec![dir.join(program)];
    if cfg!(windows) {
        out.push(dir.join(format!("{program}.exe")));
        out.push(dir.join(format!("{program}.cmd")));
        out.push(dir.join(format!("{program}.bat")));
    }
    out
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The search, in words, for a "not found" message: how many PATH entries
/// and which well-known directories were tried.
pub fn describe_search() -> String {
    let entries = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).count())
        .unwrap_or(0);
    let extra = well_known_dirs();
    if extra.is_empty() {
        format!("PATH ({entries} entries)")
    } else {
        let listed: Vec<String> = extra.iter().map(|d| d.display().to_string()).collect();
        format!("PATH ({entries} entries) and {}", listed.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_put_mise_shims_first_and_honor_mise_data_dir() {
        let dirs = candidate_dirs(Some(Path::new("/h")), None);
        assert_eq!(dirs[0], PathBuf::from("/h/.local/share/mise/shims"));
        assert!(dirs.contains(&PathBuf::from("/h/.cargo/bin")));
        let dirs = candidate_dirs(Some(Path::new("/h")), Some(Path::new("/data/mise")));
        assert_eq!(dirs[0], PathBuf::from("/data/mise/shims"));
        assert!(!dirs.iter().any(|d| d.ends_with(".local/share/mise/shims")));
    }

    #[test]
    fn augment_appends_after_the_users_path_without_duplicates() {
        let extra = vec![
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/bin"),
        ];
        let path = augment(Some(OsStr::new("/usr/bin:/bin")), &extra);
        let entries: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
                PathBuf::from("/opt/homebrew/bin")
            ],
            "the user's entries first, then the new ones, and no repeat of /usr/bin"
        );
        // No PATH at all still yields the well-known dirs.
        let path = augment(None, &extra);
        assert_eq!(std::env::split_paths(&path).count(), 2);
    }

    #[test]
    fn explicit_paths_resolve_against_the_base() {
        let base = Path::new("/ws");
        assert_eq!(
            resolve("/nowhere/clj-kondo", base),
            Some(PathBuf::from("/nowhere/clj-kondo"))
        );
        // A workspace-relative path stays valid when the child runs from a
        // subdirectory: it is anchored to the workspace, not to the cwd.
        assert_eq!(
            resolve("./bin/clj-kondo", base),
            Some(PathBuf::from("/ws/./bin/clj-kondo"))
        );
    }

    #[test]
    fn bare_names_resolve_only_to_executables() {
        let base = std::env::current_dir().unwrap();
        assert!(resolve("clj-pulse-tool-that-does-not-exist", &base).is_none());
        // `sh` is on every Unix PATH; the resolved file must be executable
        // and absolute.
        #[cfg(unix)]
        assert!(resolve("sh", &base).is_some_and(|p| p.is_absolute() && is_executable(&p)));
    }
}
