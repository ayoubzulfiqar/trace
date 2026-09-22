//! Phase 1: incremental filesystem scanning.
//!
//! The walk runs in parallel and honours `.gitignore`, `.ignore` and
//! `.traceignore` files (plus a built-in list of build/cache directories).
//! Change detection is two-tiered:
//!
//! 1. `(mtime, size)` — a file whose fingerprint is unchanged is not read.
//! 2. content hash — a file that was touched but not edited (checkout, `touch`,
//!    formatter no-op) is read and hashed, but not re-parsed.
//!
//! Scanning is split into [`compute_changes`] (read-only over the current
//! graph, parallel parse) and [`Changes::apply`] so a server can keep
//! answering queries from the old graph while a refresh is in flight.

use crate::structural::{
    extract_with_lang, IndexedFile, StructuralGraph, BUILD_MANIFESTS, BUILD_OUTPUT_DIRS,
    MAX_FILE_BYTES, SKIP_DIRS,
};
use crate::tree_sitter_detector::Lang;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, LazyLock};
use std::time::{Instant, UNIX_EPOCH};

/// A file discovered during the directory walk, ready to be scanned.
#[derive(Debug, Clone)]
pub struct ScannableFile {
    /// Forward-slash path relative to the root.
    pub rel: String,
    pub abs: PathBuf,
    pub lang: Lang,
    pub mtime_ns: i64,
    pub size: u64,
}

/// Everything the walk found.
#[derive(Debug, Default)]
pub struct WalkResult {
    pub files: Vec<ScannableFile>,
    /// Go modules: (directory relative to root, module path from `go.mod`).
    pub go_modules: Vec<(String, String)>,
    /// Supported files skipped for exceeding [`MAX_FILE_BYTES`].
    pub oversized: Vec<String>,
    /// Paths (relative, `""` = root) the walk could not read. Files beneath
    /// them are unknown, not deleted.
    pub unreadable: Vec<String>,
    /// The root was walked at all (false when it is missing).
    pub complete: bool,
}

enum Found {
    File(ScannableFile),
    GoMod(String, String),
    Oversized(String),
    Unreadable(String),
}

/// The path an `ignore` walk error is about.
fn error_path(err: &ignore::Error) -> Option<&Path> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        ignore::Error::Partial(errs) => errs.iter().find_map(error_path),
        _ => None,
    }
}

/// Skip a directory entry? Unambiguous tool/cache directories always;
/// build-output names only at the root or next to a build manifest.
fn skip_dir(entry: &ignore::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_some_and(|t| t.is_dir()) {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    if SKIP_DIRS.contains(&name.as_ref()) {
        return true;
    }
    if !BUILD_OUTPUT_DIRS.contains(&name.as_ref()) {
        return false;
    }
    entry.depth() == 1
        || entry
            .path()
            .parent()
            .is_some_and(|parent| BUILD_MANIFESTS.iter().any(|m| parent.join(m).is_file()))
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn rel_of(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    (!rel.is_empty()).then_some(rel)
}

/// Walk `root` and collect every supported source file.
pub fn collect_files(root: &Path) -> WalkResult {
    let mut result = WalkResult::default();
    if !root.is_dir() {
        return result;
    }
    result.complete = true;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .add_custom_ignore_filename(".traceignore")
        .filter_entry(|entry| !skip_dir(entry));

    let (tx, rx) = mpsc::channel::<Found>();
    builder.build_parallel().run(|| {
        let tx = tx.clone();
        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    let rel = error_path(&err)
                        .map(|p| rel_of(root, p).unwrap_or_default())
                        .unwrap_or_default();
                    let _ = tx.send(Found::Unreadable(rel));
                    return ignore::WalkState::Continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy();
            if name == "go.mod" {
                if let Some(module) = read_go_module(path) {
                    let dir = path
                        .parent()
                        .and_then(|d| rel_of(root, d))
                        .unwrap_or_default();
                    let _ = tx.send(Found::GoMod(dir, module));
                }
                return ignore::WalkState::Continue;
            }
            let Some(lang) = Lang::from_path(&name) else {
                return ignore::WalkState::Continue;
            };
            let (Some(rel), Ok(meta)) = (rel_of(root, path), entry.metadata()) else {
                return ignore::WalkState::Continue;
            };
            let found = if meta.len() > MAX_FILE_BYTES {
                Found::Oversized(rel)
            } else {
                Found::File(ScannableFile {
                    rel,
                    abs: path.to_path_buf(),
                    lang,
                    mtime_ns: mtime_ns(&meta),
                    size: meta.len(),
                })
            };
            let _ = tx.send(found);
            ignore::WalkState::Continue
        })
    });
    drop(tx);

    for found in rx {
        match found {
            Found::File(f) => result.files.push(f),
            Found::GoMod(dir, module) => result.go_modules.push((dir, module)),
            Found::Oversized(rel) => result.oversized.push(rel),
            Found::Unreadable(rel) => result.unreadable.push(rel),
        }
    }
    result.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    result.oversized.sort();
    // Longest directory first so nested modules win during resolution.
    result
        .go_modules
        .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    result
}

fn read_go_module(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("module")?;
        let module = rest.trim().trim_matches('"');
        (!module.is_empty() && rest.starts_with(char::is_whitespace)).then(|| module.to_string())
    })
}

/// Fast 64-bit content hash (word-at-a-time multiply-rotate with a final
/// avalanche). Change detection only — not cryptographic.
pub fn content_hash(bytes: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h: u64 = 0xCBF2_9CE4_8422_2325 ^ (bytes.len() as u64).wrapping_mul(K);
    let (words, rem) = bytes.as_chunks::<8>();
    for word in words {
        h = (h.rotate_left(23) ^ u64::from_le_bytes(*word)).wrapping_mul(K);
    }
    let mut tail = [0u8; 8];
    tail[..rem.len()].copy_from_slice(rem);
    h = (h.rotate_left(23) ^ u64::from_le_bytes(tail)).wrapping_mul(K);
    // splitmix64 finaliser
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^ (h >> 31)
}

/// Statistics for one scan/refresh.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ScanStats {
    /// Supported source files seen by the walk.
    pub files_total: usize,
    /// Files parsed (new or edited).
    pub reparsed: usize,
    /// Files skipped because their fingerprint was unchanged.
    pub skipped: usize,
    /// Files whose fingerprint changed but whose content did not.
    pub touched: usize,
    /// Files dropped from the index (deleted, now ignored, binary, oversized).
    pub removed: usize,
    /// Files skipped for exceeding the size limit.
    pub oversized: usize,
    /// Files that could not be read.
    pub errors: usize,
    pub elapsed_ms: i64,
}

/// The delta between the graph and the filesystem.
#[derive(Debug, Default)]
pub struct Changes {
    /// New or re-parsed files.
    pub upserts: Vec<(String, IndexedFile)>,
    /// Files whose content is unchanged but whose fingerprint moved.
    pub touched: Vec<(String, i64, u64)>,
    pub removed: Vec<String>,
    pub go_modules: Vec<(String, String)>,
    pub stats: ScanStats,
}

impl Changes {
    /// Did the file set or any file's facts change?
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.touched.is_empty() && self.removed.is_empty()
    }

    /// Apply the delta to `graph` (consumes it: no facts are cloned).
    pub fn apply(self, graph: &mut StructuralGraph) {
        for rel in &self.removed {
            graph.files.remove(rel);
        }
        for (rel, mtime_ns, size) in self.touched {
            if let Some(entry) = graph.files.get_mut(&rel) {
                entry.mtime_ns = mtime_ns;
                entry.size = size;
            }
        }
        for (rel, entry) in self.upserts {
            graph.files.insert(rel, entry);
        }
        graph.go_modules = self.go_modules;
    }
}

/// Dedicated pool with generous stacks: syntax trees of generated code can
/// be deep, and the extractor recurses.
static SCAN_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .stack_size(16 * 1024 * 1024)
        .thread_name(|i| format!("trace-scan-{i}"))
        .build()
        .expect("failed to build scan thread pool")
});

enum Outcome {
    Parsed(String, IndexedFile),
    Touched(String, i64, u64),
    Unreadable,
    /// Binary content or grew past the size limit since the walk.
    Rejected(String),
}

/// Files modified this recently are "racily clean" (a same-size edit within
/// the filesystem's timestamp granularity would keep the fingerprint), so
/// their mtime is recorded as unknown and they are re-hashed next refresh.
const RACY_WINDOW_NS: i64 = 2_000_000_000;

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn process(
    file: &ScannableFile,
    prior: Option<&IndexedFile>,
    force: bool,
    racy_after: i64,
) -> Outcome {
    let mtime_ns = if file.mtime_ns >= racy_after {
        0
    } else {
        file.mtime_ns
    };
    let bytes = match std::fs::read(&file.abs) {
        Ok(b) => b,
        Err(_) => return Outcome::Unreadable,
    };
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Outcome::Rejected(file.rel.clone());
    }
    let size = bytes.len() as u64;
    let hash = content_hash(&bytes);
    if !force {
        if let Some(prior) = prior {
            if prior.hash == hash {
                return Outcome::Touched(file.rel.clone(), mtime_ns, size);
            }
        }
    }
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Outcome::Rejected(file.rel.clone());
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    let facts = extract_with_lang(&file.rel, file.lang, &text);
    Outcome::Parsed(
        file.rel.clone(),
        IndexedFile {
            mtime_ns,
            size,
            hash,
            facts,
        },
    )
}

/// Compare the filesystem against `graph` and compute what changed. With
/// `force`, every file is re-parsed.
pub fn compute_changes(graph: &StructuralGraph, root: &Path, force: bool) -> Changes {
    let start = Instant::now();
    let racy_after = now_ns() - RACY_WINDOW_NS;
    let walk = collect_files(root);
    let mut changes = Changes {
        go_modules: walk.go_modules,
        ..Default::default()
    };
    changes.stats.files_total = walk.files.len();
    changes.stats.oversized = walk.oversized.len();

    let candidates: Vec<&ScannableFile> = walk
        .files
        .iter()
        .filter(|f| {
            force
                || graph
                    .files
                    .get(&f.rel)
                    .is_none_or(|e| e.mtime_ns != f.mtime_ns || e.size != f.size)
        })
        .collect();
    changes.stats.skipped = walk.files.len() - candidates.len();

    let outcomes: Vec<Outcome> = SCAN_POOL.install(|| {
        candidates
            .par_iter()
            .map(|f| process(f, graph.files.get(&f.rel), force, racy_after))
            .collect()
    });

    for outcome in outcomes {
        match outcome {
            Outcome::Parsed(rel, entry) => {
                changes.stats.reparsed += 1;
                changes.upserts.push((rel, entry));
            }
            Outcome::Touched(rel, mtime_ns, size) => {
                changes.stats.touched += 1;
                changes.touched.push((rel, mtime_ns, size));
            }
            Outcome::Unreadable => changes.stats.errors += 1,
            Outcome::Rejected(rel) => {
                if graph.files.contains_key(&rel) {
                    changes.removed.push(rel);
                }
            }
        }
    }

    // Files are only "deleted" when the walk could have seen them: a missing
    // root or an unreadable directory must not purge the index.
    let seen: HashSet<&str> = walk.files.iter().map(|f| f.rel.as_str()).collect();
    let unreadable = |rel: &str| {
        walk.unreadable
            .iter()
            .any(|dir| crate::structural::path_has_prefix(rel, dir))
    };
    let mut removed: HashSet<String> = changes.removed.drain(..).collect();
    removed.extend(
        walk.oversized
            .iter()
            .filter(|r| graph.files.contains_key(*r))
            .cloned(),
    );
    if walk.complete {
        removed.extend(
            graph
                .files
                .keys()
                .filter(|rel| !seen.contains(rel.as_str()) && !unreadable(rel))
                .cloned(),
        );
    }
    changes.removed = removed.into_iter().collect();
    changes.removed.sort();
    changes.stats.removed = changes.removed.len();
    changes.stats.elapsed_ms = start.elapsed().as_millis() as i64;
    changes
}

/// Full scan of the repository into a fresh graph.
pub fn scan_repo(root: &Path) -> (StructuralGraph, ScanStats) {
    let mut graph = StructuralGraph::new();
    let changes = compute_changes(&graph, root, true);
    let stats = changes.stats.clone();
    changes.apply(&mut graph);
    (graph, stats)
}

/// Bring `graph` up to date with the filesystem, re-parsing only what changed.
pub fn scan_incremental(graph: &mut StructuralGraph, root: &Path) -> ScanStats {
    let changes = compute_changes(graph, root, false);
    let stats = changes.stats.clone();
    changes.apply(graph);
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_tree(structure: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (rel, content) in structure {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        dir
    }

    /// Set a file's mtime `secs_ago` seconds in the past (deterministic, and
    /// outside the racy window).
    fn set_age(path: &Path, secs_ago: u64) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        file.set_modified(t).unwrap();
    }

    #[test]
    fn collect_files_finds_sources_and_skips_build_dirs() {
        let root = tmp_tree(&[
            ("src/lib.rs", "pub fn ok() {}"),
            ("src/main.ts", "export const x = 1;"),
            ("src/util.mts", "export const y = 1;"),
            ("README.md", "# readme"),
            ("target/release/x.rs", "fn x() {}"),
            ("node_modules/pkg/index.js", "x()"),
            (".hidden/secret.py", "x = 1"),
        ]);
        let walk = collect_files(root.path());
        let rels: Vec<&str> = walk.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["src/lib.rs", "src/main.ts", "src/util.mts"]);
    }

    #[test]
    fn collect_files_honours_gitignore_and_traceignore() {
        let root = tmp_tree(&[
            (".gitignore", "generated/\n"),
            (".traceignore", "legacy/**\n"),
            ("generated/api.ts", "x()"),
            ("legacy/old.py", "x = 1"),
            ("app/main.py", "x = 1"),
        ]);
        let walk = collect_files(root.path());
        let rels: Vec<&str> = walk.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["app/main.py"]);
    }

    #[test]
    fn collect_files_reads_go_modules_and_flags_oversized() {
        let big = "x".repeat(MAX_FILE_BYTES as usize + 1);
        let root = tmp_tree(&[
            ("svc/go.mod", "module example.com/svc\n\ngo 1.22\n"),
            ("svc/main.go", "package main"),
            ("huge.js", big.as_str()),
        ]);
        let walk = collect_files(root.path());
        assert_eq!(
            walk.go_modules,
            vec![("svc".to_string(), "example.com/svc".to_string())]
        );
        assert_eq!(walk.oversized, vec!["huge.js".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_do_not_hang_the_walk() {
        let root = tmp_tree(&[("src/lib.rs", "pub fn ok() {}")]);
        std::os::unix::fs::symlink(root.path(), root.path().join("src/loop")).unwrap();
        let walk = collect_files(root.path());
        assert_eq!(walk.files.len(), 1);
    }

    #[test]
    fn scan_repo_finds_symbols_and_call_edges() {
        let root = tmp_tree(&[
            (
                "src/lib.rs",
                "pub struct User;\npub fn process() { helper(); }\nfn helper() {}",
            ),
            (
                "src/app.ts",
                "export class Service { run() { process(); } }",
            ),
        ]);
        let (graph, stats) = scan_repo(root.path());
        assert_eq!(stats.files_total, 2);
        assert_eq!(stats.reparsed, 2);
        assert_eq!(stats.errors, 0);
        let names: Vec<&str> = graph.symbols().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"User"));
        assert!(names.contains(&"Service"));
        assert!(names.contains(&"process"));
        let callers: Vec<&str> = graph
            .find_callers("process")
            .iter()
            .map(|(e, _)| e.caller.as_str())
            .collect();
        assert_eq!(callers, vec!["Service.run"]);
    }

    #[test]
    fn incremental_scan_skips_touches_reparses_and_removes() {
        let root = tmp_tree(&[
            ("a.rs", "pub fn a() {}"),
            ("b.rs", "pub fn b() {}"),
            ("c.rs", "pub fn c() {}"),
        ]);
        for f in ["a.rs", "b.rs", "c.rs"] {
            set_age(&root.path().join(f), 60);
        }
        let (mut graph, _) = scan_repo(root.path());

        let stats = scan_incremental(&mut graph, root.path());
        assert_eq!(
            (stats.skipped, stats.reparsed, stats.touched, stats.removed),
            (3, 0, 0, 0)
        );

        set_age(&root.path().join("a.rs"), 30);
        std::fs::write(root.path().join("b.rs"), "pub fn b2() {}").unwrap();
        set_age(&root.path().join("b.rs"), 30);
        std::fs::remove_file(root.path().join("c.rs")).unwrap();
        let stats = scan_incremental(&mut graph, root.path());
        assert_eq!(stats.touched, 1, "{stats:?}");
        assert_eq!(stats.reparsed, 1, "{stats:?}");
        assert_eq!(stats.removed, 1, "{stats:?}");
        let names: Vec<&str> = graph.symbols().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b2"]);
    }

    #[test]
    fn freshly_modified_files_are_rechecked() {
        let root = tmp_tree(&[("a.rs", "pub fn a() {}")]);
        let (mut graph, _) = scan_repo(root.path());
        assert_eq!(
            graph.files["a.rs"].mtime_ns, 0,
            "racily-clean mtime is not trusted"
        );
        // Same size, same (coarse) mtime: only a re-hash notices the edit.
        std::fs::write(root.path().join("a.rs"), "pub fn b() {}").unwrap();
        let stats = scan_incremental(&mut graph, root.path());
        assert_eq!(stats.reparsed, 1, "{stats:?}");
        assert_eq!(graph.symbols().next().unwrap().name, "b");
    }

    #[test]
    fn build_output_names_are_only_skipped_in_context() {
        let root = tmp_tree(&[
            ("package.json", "{}"),
            ("build/bundle.js", "x()"),
            ("web/package.json", "{}"),
            ("web/dist/app.js", "x()"),
            (
                "src/main/java/com/acme/build/Builder.java",
                "class Builder {}",
            ),
            ("lib/out/report.py", "x = 1"),
        ]);
        let walk = collect_files(root.path());
        let rels: Vec<&str> = walk.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec![
                "lib/out/report.py",
                "src/main/java/com/acme/build/Builder.java"
            ]
        );
    }

    #[test]
    fn missing_root_does_not_purge_the_index() {
        let root = tmp_tree(&[("a.rs", "pub fn a() {}")]);
        let (graph, _) = scan_repo(root.path());
        let changes = compute_changes(&graph, &root.path().join("nope"), false);
        assert!(changes.removed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directories_do_not_purge_their_files() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp_tree(&[("locked/a.rs", "pub fn a() {}"), ("b.rs", "pub fn b() {}")]);
        let (graph, _) = scan_repo(root.path());
        let locked = root.path().join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = std::fs::read_dir(&locked).is_ok(); // running as root
        let changes = compute_changes(&graph, root.path(), false);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !readable_anyway {
            assert!(changes.removed.is_empty(), "{:?}", changes.removed);
        }
    }

    #[test]
    fn content_hash_is_stable_and_discriminating() {
        assert_eq!(content_hash(b"hello world"), content_hash(b"hello world"));
        assert_ne!(content_hash(b"hello world"), content_hash(b"hello worle"));
        assert_ne!(content_hash(b""), content_hash(b"\0"));
        assert_ne!(content_hash(b"12345678"), content_hash(b"123456789"));
    }

    #[test]
    fn binary_files_are_not_indexed() {
        let root = tmp_tree(&[("blob.js", "x\0y")]);
        let (graph, _) = scan_repo(root.path());
        assert!(graph.files.is_empty());
    }
}
