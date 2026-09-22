//! Phase 1: Incremental filesystem scanning + hashing.
//!
//! Walks the project tree, extracts structural facts from each source file,
//! and stores them in a `StructuralGraph`. Only files whose hash changed
//! are re-parsed (incremental), so large repos stay responsive.

use crate::structural::{extract_file, ScannableFile, StructuralGraph};

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

pub mod incremental {
    //! Hash-based incremental detection.
    use super::*;

    /// (mtime_ms, size) pair stored for quick "has this file changed?" checks.
    pub fn file_fingerprint(path: &Path) -> Option<(i64, u64)> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta.modified().ok()?;
        let mtime_ms = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as i64;
        Some((mtime_ms, meta.len()))
    }

    /// Fingerprint of a directory = concatenation of child fingerprints.
    /// Used to detect any change anywhere under a subtree without walking
    /// every file every time.
    pub fn dir_fingerprint(path: &Path) -> Option<(i64, u64)> {
        let entries = std::fs::read_dir(path).ok()?;
        let mut max_ts: i64 = 0;
        let mut total: u64 = 0;
        for e in entries {
            if let Ok(e) = e {
                let child = e.path();
                if child.is_file() {
                    if let Some((ts, len)) = file_fingerprint(&child) {
                        if ts > max_ts {
                            max_ts = ts;
                        }
                        total += len;
                    }
                }
            }
        }
        if max_ts == 0 && total == 0 {
            None
        } else {
            Some((max_ts, total))
        }
    }

    /// Returns `Some(())` when the file content appears stale (changed on disk
    /// relative to the stored fingerprint), or `None` when the fingerprint
    /// is missing.
    pub fn needs_reparse(path: &Path, stored: Option<(i64, u64)>) -> bool {
        match file_fingerprint(path) {
            Some(current) => match stored {
                Some(prev) => current != prev,
                None => true,
            },
            None => false, // file no longer exists
        }
    }
}

/// Walk a directory tree and collect all scannable files.
pub fn collect_files(root: &Path) -> Vec<ScannableFile> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    walk_dir_collect(root, root, &mut out);
    out.sort_by(|a, b| a.rel.path().cmp(b.rel.path()));
    out
}

fn walk_dir_collect(root: &Path, current: &Path, out: &mut Vec<ScannableFile>) {
    let entries = match std::fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if crate::structural::SKIP_DIRS
                .iter()
                .any(|d| *d == name.as_ref())
            {
                continue;
            }
            walk_dir_collect(root, &path, out);
        } else if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_lowercase);
            let Some(ext) = ext else { continue };
            if crate::structural::SUPPORTED_EXTS.iter().any(|e| e == &ext) {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                let rel_str = rel
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                if let Some(fingerprint) = incremental::file_fingerprint(&path) {
                    out.push(ScannableFile {
                        rel: crate::model::RelPath(rel_str),
                        abs: path.to_string_lossy().to_string(),
                        ext,
                        fingerprint,
                    });
                }
            }
        }
    }
}

/// Full scan of the repository — re-extracts structure from every scannable
/// file. Returns the populated graph plus timing stats.
pub fn scan_repo(root: &Path) -> (StructuralGraph, ScanStats) {
    let start = Instant::now();
    let files = collect_files(root);
    let mut graph = StructuralGraph::new();
    let mut reparsed = 0usize;
    let skipped = 0usize;
    let mut errors = 0usize;

    for f in &files {
        let text = match std::fs::read_to_string(&f.abs) {
            Ok(t) => t,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let (symbols, imports, routes) = extract_file(&f.rel.0, &f.ext, &text);
        graph.merge_file(&f.rel, &symbols, &imports, &routes);
        reparsed += 1;
    }

    let elapsed_ms = start.elapsed().as_millis() as i64;
    (
        graph,
        ScanStats {
            files_total: files.len(),
            reparsed,
            skipped,
            errors,
            elapsed_ms,
        },
    )
}

/// Incrementally re-scan, only re-parsing files whose fingerprint changed
/// since `prior_fingerprints`.
pub fn scan_incremental(
    root: &Path,
    prior_fingerprints: &HashMap<String, (i64, u64)>,
) -> (StructuralGraph, ScanStats, HashMap<String, (i64, u64)>) {
    let start = Instant::now();
    let files = collect_files(root);
    let mut graph = StructuralGraph::new();
    let mut reparsed = 0usize;
    let mut skipped = 0usize;
    let mut errors = 0usize;
    let mut new_fingerprints = HashMap::new();

    for f in &files {
        new_fingerprints.insert(f.rel.0.clone(), f.fingerprint);
        let needs = incremental::needs_reparse(
            Path::new(&f.abs),
            prior_fingerprints.get(&f.rel.0).copied(),
        );
        if !needs {
            skipped += 1;
            continue;
        }
        let text = match std::fs::read_to_string(&f.abs) {
            Ok(t) => t,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let (symbols, imports, routes) = extract_file(&f.rel.0, &f.ext, &text);
        graph.merge_file(&f.rel, &symbols, &imports, &routes);
        reparsed += 1;
    }

    let elapsed_ms = start.elapsed().as_millis() as i64;
    (
        graph,
        ScanStats {
            files_total: files.len(),
            reparsed,
            skipped,
            errors,
            elapsed_ms,
        },
        new_fingerprints,
    )
}

/// Lightweight stats returned after a scan.
#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub files_total: usize,
    pub reparsed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub elapsed_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RelPath;
    use std::io::Write;
    use std::path::PathBuf;

    static ANR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmp_tree(structure: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trace_test_{}_{}",
            std::process::id(),
            ANR.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, content) in structure {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        dir
    }

    #[test]
    fn collect_files_finds_rust_and_ts() {
        let root = tmp_tree(&[
            ("src/lib.rs", "pub fn ok() {}"),
            ("src/main.ts", "export const x = 1;"),
            ("README.md", "# readme"),
            ("target/release/x", "binary"),
        ]);
        let files = collect_files(&root);
        let rels: Vec<&str> = files.iter().map(|f| f.rel.0.as_str()).collect();
        assert!(rels.contains(&"src/lib.rs"));
        assert!(rels.contains(&"src/main.ts"));
        assert!(!rels.iter().any(|r| r.ends_with(".md")));
        assert!(!rels.iter().any(|r| r.contains("target")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn incremental_fingerprint_detects_change() {
        let root = tmp_tree(&[("src/lib.rs", "pub fn ok() {}")]);
        let path = root.join("src/lib.rs");
        let fp1 = incremental::file_fingerprint(&path).unwrap();
        std::fs::write(&path, "pub fn changed() {}").unwrap();
        let fp2 = incremental::file_fingerprint(&path).unwrap();
        assert!(incremental::needs_reparse(&path, Some(fp1)));
        assert!(!incremental::needs_reparse(&path, Some(fp2)));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_repo_finds_symbols() {
        let root = tmp_tree(&[
            ("src/lib.rs", "pub struct User;\npub fn process() {}"),
            ("src/app.ts", "export class Service {}"),
        ]);
        let (graph, stats) = scan_repo(&root);
        assert_eq!(stats.files_total, 2);
        assert_eq!(stats.errors, 0);
        let symbol_names: Vec<String> = graph.symbols.values().map(|s| s.name.clone()).collect();
        assert!(symbol_names.contains(&"User".to_string()));
        assert!(symbol_names.contains(&"Service".to_string()));
        assert!(symbol_names.contains(&"process".to_string()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_incremental_skips_unchanged() {
        let root = tmp_tree(&[("src/lib.rs", "pub fn ok() {}")]);
        let path = root.join("src/lib.rs");
        let fp = incremental::file_fingerprint(&path).unwrap();
        let mut prior = HashMap::new();
        prior.insert("src/lib.rs".to_string(), fp);
        let (_graph, stats, new_fp) = scan_incremental(&root, &prior);
        assert_eq!(stats.reparsed, 0);
        assert_eq!(stats.skipped, 1);
        assert_eq!(new_fp.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
