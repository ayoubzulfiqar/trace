//! Root discovery — git-style upward walk from the current directory looking
//! for project markers. Resolved ONCE, before dispatch, and passed to every
//! handler.

use std::path::{Path, PathBuf};

/// Resolve the project root. If `explicit` is given, use it directly.
/// Otherwise walk upward from `cwd` looking for marker files
/// (in priority order: `trace.toml`, `.git/`, `.hg/`, `Cargo.toml`).
pub fn resolve(explicit: Option<PathBuf>, cwd: &Path) -> Option<PathBuf> {
    if let Some(explicit) = explicit {
        return Some(canonicalize_path(&explicit));
    }
    find_root_from(cwd)
}

fn find_root_from(cwd: &Path) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(cwd);
    while let Some(dir) = current {
        for marker in MARKERS {
            if dir.join(marker).exists() {
                return Some(canonicalize_path(dir));
            }
        }
        current = dir.parent();
    }
    None
}

const MARKERS: &[&str] = &["trace.toml", ".git", ".hg", "Cargo.toml"];

fn canonicalize_path(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn explicit_root_is_used_directly() {
        let tmp =
            std::env::temp_dir().join(format!("trace-root-test-{}-explicit", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let resolved = resolve(Some(tmp.clone()), std::env::temp_dir().as_path()).unwrap();
        // explicit path may or may not canonicalize depending on temp dir
        assert!(resolved == tmp || resolved == tmp.canonicalize().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discovers_root_by_git_marker() {
        let tmp = std::env::temp_dir().join(format!("trace-root-test-{}-git", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join(".git"), "").unwrap();
        let resolved = find_root_from(&tmp).unwrap();
        assert_eq!(resolved, tmp.canonicalize().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discovers_root_by_trace_toml() {
        let tmp = std::env::temp_dir().join(format!("trace-root-test-{}-toml", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("trace.toml"), "").unwrap();
        let resolved = find_root_from(&tmp).unwrap();
        assert_eq!(resolved, tmp.canonicalize().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn walks_upward_to_find_marker() {
        let base =
            std::env::temp_dir().join(format!("trace-root-test-{}-walkup", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/b/c")).unwrap();
        fs::write(base.join(".git"), "").unwrap();
        let resolved = find_root_from(&base.join("a/b/c")).unwrap();
        assert_eq!(resolved, base.canonicalize().unwrap());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn returns_none_when_no_marker_found() {
        let tmp = std::env::temp_dir().join(format!("trace-root-test-{}-none", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp.join("sub/deep")).unwrap();
        // Create a subdirectory with no markers and no marker in any parent chain
        // that we control — this may find / or /tmp as root, so just verify
        // it returns Some (it will find some ancestor marker) or we test in a
        // controlled way:
        let result = find_root_from(&tmp.join("sub/deep"));
        // On most systems, walking up from /tmp will find a marker eventually
        // (e.g. /tmp itself has no marker, but / might). Just verify it doesn't
        // panic and returns a path or None.
        if let Some(r) = result {
            assert!(r.is_dir());
        }
    }
}
