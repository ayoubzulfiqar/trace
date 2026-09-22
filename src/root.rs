//! Project root discovery and path confinement.
//!
//! The root is resolved ONCE, before dispatch, and passed to every handler.
//! Every user-supplied path then goes through [`resolve_in_root`], which
//! guarantees it names a location inside the project (symlinks included).

use std::path::{Component, Path, PathBuf};

/// Explicit project configuration — always wins, even inside a larger repo.
const CONFIG_MARKERS: &[&str] = &["trace.toml", ".architectural-rules.json"];
/// Version-control roots — preferred over nested manifests so a monorepo is
/// indexed as one project.
const VCS_MARKERS: &[&str] = &[".git", ".hg", ".jj", ".svn"];
/// Build manifests — used when no VCS root exists.
const MANIFEST_MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];

/// Resolve the project root. An explicit path is used as-is (canonicalised);
/// otherwise walk upward from `cwd`: the nearest directory holding a trace
/// config file wins, then the nearest VCS root, then the nearest manifest.
pub fn resolve(explicit: Option<PathBuf>, cwd: &Path) -> Option<PathBuf> {
    if let Some(explicit) = explicit {
        return Some(canonicalize_path(&explicit));
    }
    find_root_from(cwd)
}

fn find_root_from(cwd: &Path) -> Option<PathBuf> {
    let mut first_manifest: Option<&Path> = None;
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if CONFIG_MARKERS.iter().any(|m| dir.join(m).exists()) {
            return Some(canonicalize_path(dir));
        }
        if VCS_MARKERS.iter().any(|m| dir.join(m).exists()) {
            return Some(canonicalize_path(dir));
        }
        if first_manifest.is_none() && MANIFEST_MARKERS.iter().any(|m| dir.join(m).exists()) {
            first_manifest = Some(dir);
        }
        current = dir.parent();
    }
    first_manifest.map(canonicalize_path)
}

fn canonicalize_path(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Is `root` too broad to index (filesystem root or the home directory)?
/// Agents launched without a working directory often start in `/` or `~`;
/// walking those would index the whole disk.
pub fn is_broad_root(root: &Path) -> bool {
    let root = canonicalize_path(root);
    if root.parent().is_none() {
        return true;
    }
    match dirs::home_dir() {
        Some(home) => canonicalize_path(&home) == root,
        None => false,
    }
}

/// A user-supplied path resolved inside the project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPath {
    /// Forward-slash path relative to the root (empty for the root itself).
    pub rel: String,
    /// Absolute path (symlinks resolved where the path exists).
    pub abs: PathBuf,
}

/// Resolve `input` (relative to `root`, or absolute) and confine it to the
/// root. Rejects `..` escapes and symlinks pointing outside. The target does
/// not need to exist (plans may name new files).
pub fn resolve_in_root(root: &Path, input: &str) -> Result<RepoPath, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("path is empty".to_string());
    }
    let root_real = canonicalize_path(root);
    let given = Path::new(input);
    let joined = if given.is_absolute() {
        given.to_path_buf()
    } else {
        root_real.join(given)
    };

    // Lexical normalisation first, so `..` can never climb above the root.
    let mut lexical = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                if !lexical.pop() {
                    return Err(format!("path '{input}' escapes the filesystem root"));
                }
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }

    // Resolve symlinks through the deepest existing ancestor.
    let mut probe = lexical.clone();
    let mut missing = Vec::new();
    let real = loop {
        if let Ok(real) = probe.canonicalize() {
            let mut real = real;
            for part in missing.iter().rev() {
                real.push(part);
            }
            break real;
        }
        match probe.file_name() {
            Some(name) => {
                missing.push(name.to_os_string());
                probe.pop();
            }
            None => break lexical.clone(),
        }
    };

    let rel = real.strip_prefix(&root_real).map_err(|_| {
        format!(
            "path '{input}' is outside the project root {}",
            root_real.display()
        )
    })?;
    let rel = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Ok(RepoPath { rel, abs: real })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn explicit_root_is_used_directly() {
        let tmp = TempDir::new().unwrap();
        let resolved = resolve(Some(tmp.path().to_path_buf()), Path::new("/")).unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn discovers_root_by_git_marker() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".git"), "").unwrap();
        assert_eq!(
            find_root_from(tmp.path()).unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn discovers_root_by_trace_toml() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("trace.toml"), "").unwrap();
        assert_eq!(
            find_root_from(tmp.path()).unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn walks_upward_to_find_marker() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("a/b/c")).unwrap();
        fs::write(tmp.path().join(".git"), "").unwrap();
        let resolved = find_root_from(&tmp.path().join("a/b/c")).unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn vcs_root_beats_nested_manifest() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/core/src")).unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join("crates/core/Cargo.toml"), "").unwrap();
        let resolved = find_root_from(&tmp.path().join("crates/core/src")).unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn trace_config_beats_vcs_root() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("svc")).unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join("svc/trace.toml"), "").unwrap();
        let resolved = find_root_from(&tmp.path().join("svc")).unwrap();
        assert_eq!(resolved, tmp.path().join("svc").canonicalize().unwrap());
    }

    #[test]
    fn filesystem_root_is_broad() {
        assert!(is_broad_root(Path::new("/")));
        let tmp = TempDir::new().unwrap();
        assert!(!is_broad_root(tmp.path()));
    }

    #[test]
    fn resolve_in_root_accepts_relative_absolute_and_new_files() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/lib.rs"), "").unwrap();
        let root = tmp.path();
        assert_eq!(
            resolve_in_root(root, "src/lib.rs").unwrap().rel,
            "src/lib.rs"
        );
        assert_eq!(
            resolve_in_root(root, "./src/../src/lib.rs").unwrap().rel,
            "src/lib.rs"
        );
        let abs = root.join("src/lib.rs");
        assert_eq!(
            resolve_in_root(root, abs.to_str().unwrap()).unwrap().rel,
            "src/lib.rs"
        );
        assert_eq!(
            resolve_in_root(root, "src/new/file.rs").unwrap().rel,
            "src/new/file.rs"
        );
    }

    #[test]
    fn resolve_in_root_rejects_escapes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        assert!(resolve_in_root(root, "../../../etc/passwd").is_err());
        assert!(resolve_in_root(root, "/etc/passwd").is_err());
        assert!(resolve_in_root(root, "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_in_root_rejects_symlink_escapes() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        assert!(resolve_in_root(tmp.path(), "link/secret").is_err());
    }
}
