//! Phase 3: Architecture Decision Record Graph.
//!
//! Captures architecture decisions so agents can check *why* past choices were
//! made before rewriting logic. ADRs are Markdown files (human-editable,
//! git-friendly). Records written by trace carry YAML front matter; existing
//! adr-tools / MADR style records (numbered `# N. Title` with a `## Status`
//! section) are read as well, from any of the conventional ADR directories.
//!
//! MCP tools: `search_decisions`, `record_decision`.

use crate::humanize::now_ms;
use crate::model::{Decision, DecisionStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where new ADRs go when the project has no ADR directory yet.
pub const DECISIONS_DIR: &str = "docs/decisions";

/// Conventional ADR directories, searched in order.
pub const ADR_DIRS: &[&str] = &[
    "docs/decisions",
    "docs/adr",
    "docs/adrs",
    "docs/architecture/decisions",
    "doc/adr",
    "doc/decisions",
    "doc/architecture/decisions",
    "adr",
    "decisions",
];

/// A searchable ADR record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdrRecord {
    pub id: String, // e.g. "0001"
    pub title: String,
    pub status: String, // Proposed | Accepted | Superseded | Deprecated | Rejected
    pub date: String,   // YYYY-MM-DD
    pub context: String,
    pub decision: String,
    #[serde(default)]
    pub consequences: String,
    #[serde(default)]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub path: String, // path to the .md file, relative to the project root
}

impl AdrRecord {
    pub fn new(title: &str, context: &str, decision: &str, consequences: &str) -> Self {
        AdrRecord {
            id: String::new(),
            title: title.to_string(),
            status: DecisionStatus::Accepted.to_string(),
            date: current_date(),
            context: context.to_string(),
            decision: decision.to_string(),
            consequences: consequences.to_string(),
            supersedes: None,
            superseded_by: None,
            tags: Vec::new(),
            path: String::new(),
        }
    }

    fn numeric_id(&self) -> Option<u32> {
        self.id.trim().parse().ok()
    }
}

/// Today's local date as `YYYY-MM-DD`.
pub fn current_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// The ADR directory for this project: the first conventional directory that
/// exists, or `docs/decisions`.
pub fn decisions_dir(root: &Path) -> PathBuf {
    ADR_DIRS
        .iter()
        .map(|d| root.join(d))
        .find(|p| p.is_dir())
        .unwrap_or_else(|| root.join(DECISIONS_DIR))
}

fn leading_number(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Find the next available ADR sequence number.
pub fn next_id(decisions_dir: &Path) -> String {
    let max = fs::read_dir(decisions_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".md")
                .then(|| leading_number(&name))
                .flatten()
        })
        .max()
        .unwrap_or(0);
    format!("{:04}", max + 1)
}

/// Normalise a user-supplied ADR reference (`3`, `0003`, `ADR-3`) to digits.
/// Accepts `3`, `0003`, `#3`, `ADR-3`, `adr 0003` and file stems like
/// `0003-use-postgres`; rejects words that merely contain digits (`s3`,
/// `oauth2`, `v2`).
fn id_number(reference: &str) -> Option<u32> {
    let mut r = reference.trim().trim_start_matches('#');
    if r.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("adr")) {
        r = r[3..].trim_start_matches(['-', '_', ' ', '#']);
    }
    let digits_len = r.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 {
        return None;
    }
    let rest = &r[digits_len..];
    if !(rest.is_empty() || rest.starts_with(['-', '_', '.', ' '])) {
        return None;
    }
    r[..digits_len].parse().ok()
}

/// Everything needed to record a decision.
#[derive(Debug, Clone, Default)]
pub struct NewDecision {
    pub title: String,
    pub context: String,
    pub decision: String,
    pub consequences: String,
    /// Defaults to Accepted.
    pub status: Option<String>,
    pub tags: Vec<String>,
    /// ID of the ADR this one replaces; it is marked Superseded.
    pub supersedes: Option<String>,
}

/// YAML double-quoted scalar (JSON strings are valid YAML).
fn yaml_str(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn slugify(s: &str) -> String {
    let mut slug = String::new();
    let mut dash = false;
    for c in s.to_lowercase().chars() {
        if c.is_alphanumeric() {
            slug.push(c);
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
        if slug.len() >= 60 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "decision".to_string()
    } else {
        slug
    }
}

fn rel_to_root(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Record a new ADR as a Markdown file. Allocation of the sequence number is
/// race-free across processes (`create_new` + retry).
pub fn record(root: &Path, new: &NewDecision) -> Result<AdrRecord, String> {
    let title = single_line(&new.title);
    if title.is_empty() {
        return Err("title must not be empty".into());
    }
    if new.decision.trim().is_empty() {
        return Err("decision must not be empty".into());
    }
    let status: DecisionStatus = match &new.status {
        Some(s) if !s.trim().is_empty() => s.parse()?,
        _ => DecisionStatus::Accepted,
    };
    let superseded = match &new.supersedes {
        Some(reference) if !reference.trim().is_empty() => Some(
            find_decision(root, reference)
                .ok_or_else(|| format!("cannot supersede {reference:?}: no such decision"))?,
        ),
        _ => None,
    };
    let tags: Vec<String> = new
        .tags
        .iter()
        .map(|t| single_line(t))
        .filter(|t| !t.is_empty())
        .collect();

    let dir = decisions_dir(root);
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let date = current_date();
    let slug = slugify(&title);
    // Number allocation must be exclusive across threads and processes:
    // `create_new` alone cannot stop two different titles taking one number.
    let _allocation = allocation_lock(root)?;

    for _attempt in 0..100 {
        let id = next_id(&dir);
        let path = dir.join(format!("{id}-{slug}.md"));
        let mut front = format!(
            "---\nid: {}\ntitle: {}\nstatus: {status}\ndate: {date}\n",
            yaml_str(&id),
            yaml_str(&title)
        );
        if !tags.is_empty() {
            let list: Vec<String> = tags.iter().map(|t| yaml_str(t)).collect();
            front.push_str(&format!("tags: [{}]\n", list.join(", ")));
        }
        if let Some(old) = &superseded {
            front.push_str(&format!("supersedes: {}\n", yaml_str(&old.id)));
        }
        front.push_str("---\n");
        let consequences = if new.consequences.trim().is_empty() {
            "_Not recorded._"
        } else {
            new.consequences.trim()
        };
        let context = if new.context.trim().is_empty() {
            "_Not recorded._"
        } else {
            new.context.trim()
        };
        let body = format!(
            "\n# {title}\n\n## Context\n\n{context}\n\n## Decision\n\n{}\n\n## Consequences\n\n{consequences}\n",
            new.decision.trim()
        );
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        let mut file = match file {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("writing {}: {e}", path.display())),
        };
        file.write_all(front.as_bytes())
            .and_then(|_| file.write_all(body.as_bytes()))
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        drop(file);

        if let Some(old) = &superseded {
            mark_superseded(&root.join(&old.path), &id, &title)
                .map_err(|e| format!("recorded {id} but failed to update {}: {e}", old.path))?;
        }
        return parse_adr_in(root, &path)
            .ok_or_else(|| format!("wrote {} but could not read it back", path.display()));
    }
    Err("could not allocate an ADR number after 100 attempts".into())
}

/// Exclusive lock (held until dropped) serialising ADR number allocation.
/// Lives in the project's gitignored `.trace/` state directory.
fn allocation_lock(root: &Path) -> Result<fs::File, String> {
    let state = root.join(".trace");
    fs::create_dir_all(&state).map_err(|e| format!("creating {}: {e}", state.display()))?;
    let gitignore = state.join(".gitignore");
    if !gitignore.exists() {
        let _ = fs::write(
            &gitignore,
            "# trace runtime state (index cache, history)\n*\n",
        );
    }
    let path = state.join("adr.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    file.lock()
        .map_err(|e| format!("locking {}: {e}", path.display()))?;
    Ok(file)
}

/// Compatibility wrapper: record an Accepted decision and return its path.
pub fn record_decision(
    root: &Path,
    title: &str,
    context: &str,
    decision: &str,
    consequences: &str,
) -> Result<PathBuf, String> {
    let record = record(
        root,
        &NewDecision {
            title: title.into(),
            context: context.into(),
            decision: decision.into(),
            consequences: consequences.into(),
            ..Default::default()
        },
    )?;
    Ok(root.join(record.path))
}

/// Mark an existing ADR as superseded by `new_id`, preserving the rest of the
/// file. Works for front-matter records and adr-tools `## Status` sections.
fn mark_superseded(path: &Path, new_id: &str, new_title: &str) -> std::io::Result<()> {
    let text = fs::read_to_string(path)?;
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let has_front = lines.first().map(|l| l.trim() == "---").unwrap_or(false);
    let front_end = if has_front {
        lines
            .iter()
            .skip(1)
            .position(|l| l.trim() == "---")
            .map(|i| i + 1)
    } else {
        None
    };
    if let Some(end) = front_end {
        // `lines[end]` is the closing `---`; only the block between is edited.
        let mut front: Vec<String> = lines[1..end]
            .iter()
            .filter(|l| !l.trim_start().starts_with("superseded_by:"))
            .cloned()
            .collect();
        let mut saw_status = false;
        for line in front.iter_mut() {
            if line.trim_start().starts_with("status:") {
                *line = "status: Superseded".to_string();
                saw_status = true;
            }
        }
        if !saw_status {
            front.push("status: Superseded".to_string());
        }
        front.push(format!("superseded_by: {}", yaml_str(new_id)));
        let mut rebuilt = vec!["---".to_string()];
        rebuilt.extend(front);
        rebuilt.extend(lines[end..].iter().cloned());
        lines = rebuilt;
    } else if let Some(idx) = lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case("## status"))
    {
        // Replace the first non-empty line of the section.
        let target = lines
            .iter()
            .enumerate()
            .skip(idx + 1)
            .take_while(|(_, l)| !l.starts_with('#'))
            .find(|(_, l)| !l.trim().is_empty())
            .map(|(i, _)| i);
        let status = format!("Superseded by {new_id} ({new_title})");
        match target {
            Some(i) => lines[i] = status,
            None => {
                lines.insert(idx + 1, String::new());
                lines.insert(idx + 2, status);
            }
        }
    } else {
        lines.insert(
            0,
            format!(
                "---\nstatus: Superseded\nsuperseded_by: {}\n---",
                yaml_str(new_id)
            ),
        );
    }
    let mut out = lines.join("\n");
    out.push('\n');
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, path)
}

// ── Parsing ────────────────────────────────────────────────────────────────────

#[derive(Default, Deserialize)]
struct FrontMatter {
    #[serde(default)]
    id: Option<serde_yaml::Value>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    date: Option<serde_yaml::Value>,
    #[serde(default)]
    tags: Option<serde_yaml::Value>,
    #[serde(default)]
    supersedes: Option<serde_yaml::Value>,
    #[serde(default)]
    superseded_by: Option<serde_yaml::Value>,
}

fn yaml_scalar(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_list(value: &serde_yaml::Value) -> Vec<String> {
    match value {
        serde_yaml::Value::Sequence(items) => items.iter().filter_map(yaml_scalar).collect(),
        serde_yaml::Value::String(s) => s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Lenient `key: value` parse for front matter serde_yaml rejects.
fn parse_front_matter_lines(lines: &[&str]) -> FrontMatter {
    let mut fm = FrontMatter::default();
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        let v = serde_yaml::Value::String(value.clone());
        match key.trim() {
            "id" => fm.id = Some(v),
            "title" => fm.title = Some(value),
            "status" => fm.status = Some(value),
            "date" => fm.date = Some(v),
            "tags" => {
                fm.tags = Some(serde_yaml::Value::String(
                    value.trim_matches(['[', ']']).to_string(),
                ))
            }
            "supersedes" => fm.supersedes = Some(v),
            "superseded_by" => fm.superseded_by = Some(v),
            _ => {}
        }
    }
    fm
}

/// Body of a `## Heading` section: everything up to the next heading of
/// level 1 or 2. `prefixes` are matched case-insensitively against the
/// heading text (MADR uses "Context and Problem Statement", etc.).
/// Markdown headings outside fenced code blocks: (line, level, lowercase text).
fn headings(lines: &[&str]) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        if let Some(marker) = ["```", "~~~"].into_iter().find(|m| t.starts_with(m)) {
            match fence {
                Some(open) if open == marker => fence = None,
                None => fence = Some(marker),
                _ => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        let level = t.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&level) && t[level..].starts_with(' ') {
            let text = t[level..]
                .trim()
                .trim_end_matches('#')
                .trim()
                .to_lowercase();
            out.push((i, level, text));
        }
    }
    out
}

/// Headings that start a section of their own (ours, adr-tools, MADR).
fn is_section_heading(text: &str) -> bool {
    text == "status"
        || text.starts_with("context")
        || text.starts_with("decision")
        || text.contains("consequences")
        || text.starts_with("considered options")
        || text.starts_with("pros and cons")
        || matches!(text, "links" | "more information" | "confirmation")
}

/// Body of the first level-2/3 section whose heading matches one of `names`
/// (exact matches, in preference order, before prefix matches). The body ends
/// at the next heading of the same or higher level, or at the next heading
/// that starts another known section (MADR nests `### Consequences` under
/// `## Decision Outcome`). Headings inside code fences are ignored.
fn extract_section(lines: &[&str], names: &[&str]) -> String {
    let heads = headings(lines);
    let eligible = |h: &(usize, usize, String)| (2..=3).contains(&h.1);
    let pick = names
        .iter()
        .find_map(|n| heads.iter().position(|h| eligible(h) && h.2 == *n))
        .or_else(|| {
            names.iter().find_map(|n| {
                heads
                    .iter()
                    .position(|h| eligible(h) && h.2.starts_with(n) && !h.2.contains("driver"))
            })
        });
    let Some(idx) = pick else {
        return String::new();
    };
    let (start, level, _) = &heads[idx];
    let end = heads[idx + 1..]
        .iter()
        .find(|(_, lvl, text)| lvl <= level || is_section_heading(text))
        .map(|(i, _, _)| *i)
        .unwrap_or(lines.len());
    lines[start + 1..end].join("\n").trim().to_string()
}

/// Parse an ADR file; `path` is made relative to `root` in the record.
pub fn parse_adr_in(root: &Path, path: &Path) -> Option<AdrRecord> {
    let text = fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let file_name = path.file_name()?.to_string_lossy().to_string();

    let (front, body_start) = if lines.first().map(|l| l.trim()) == Some("---") {
        match lines.iter().skip(1).position(|l| l.trim() == "---") {
            Some(end) => {
                let raw = &lines[1..=end];
                let fm = serde_yaml::from_str::<FrontMatter>(&raw.join("\n"))
                    .unwrap_or_else(|_| parse_front_matter_lines(raw));
                (fm, end + 2)
            }
            None => (FrontMatter::default(), 0),
        }
    } else {
        (FrontMatter::default(), 0)
    };
    let body = &lines[body_start.min(lines.len())..];

    let heading = body
        .iter()
        .find_map(|l| l.trim().strip_prefix("# "))
        .map(str::trim)
        .unwrap_or("");
    // adr-tools: "# 1. Record architecture decisions"
    let heading_title = match heading.split_once(". ") {
        Some((num, rest)) if num.chars().all(|c| c.is_ascii_digit()) => rest.trim(),
        _ => heading,
    };
    let title = front
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| heading_title.to_string());

    let id = front
        .id
        .as_ref()
        .and_then(yaml_scalar)
        .or_else(|| leading_number(&file_name).map(|n| format!("{n:04}")))
        .unwrap_or_default();

    let inline_field = |key: &str| {
        body.iter().find_map(|l| {
            let t = l.trim().trim_start_matches(['*', '-']).trim();
            let (k, v) = t.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(key)
                .then(|| v.trim().to_string())
        })
    };
    let status_raw = front
        .status
        .clone()
        .or_else(|| {
            let section = extract_section(body, &["status"]);
            section
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
        })
        .or_else(|| inline_field("status"))
        .unwrap_or_else(|| "Accepted".to_string());
    let status = status_raw
        .parse::<DecisionStatus>()
        .map(|s| s.to_string())
        .unwrap_or(status_raw.clone());
    let superseded_by = front
        .superseded_by
        .as_ref()
        .and_then(yaml_scalar)
        .or_else(|| {
            status_raw
                .to_lowercase()
                .starts_with("superseded by")
                .then(|| {
                    status_raw
                        .chars()
                        .skip("superseded by".len())
                        .skip_while(|c| !c.is_ascii_digit())
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                })
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<u32>().ok())
                .map(|n| format!("{n:04}"))
        });
    let date = front
        .date
        .as_ref()
        .and_then(yaml_scalar)
        .or_else(|| inline_field("date"))
        .unwrap_or_default();

    Some(AdrRecord {
        id,
        title,
        status,
        date,
        context: extract_section(
            body,
            &[
                "context",
                "context and problem statement",
                "problem statement",
                "background",
            ],
        ),
        decision: extract_section(body, &["decision", "decision outcome", "decisions"]),
        consequences: extract_section(
            body,
            &[
                "consequences",
                "positive consequences",
                "negative consequences",
            ],
        ),
        supersedes: front.supersedes.as_ref().and_then(yaml_scalar),
        superseded_by,
        tags: front.tags.as_ref().map(yaml_list).unwrap_or_default(),
        path: rel_to_root(root, path),
    })
}

/// Parse an ADR with its path relative to the current directory.
pub fn parse_adr(path: &Path) -> Option<AdrRecord> {
    let cwd = std::env::current_dir().unwrap_or_default();
    parse_adr_in(&cwd, path)
}

/// Every ADR in every conventional directory, ordered by number.
pub fn list_decisions(root: &Path) -> Vec<AdrRecord> {
    let mut seen = HashSet::new();
    let mut records = Vec::new();
    for dir in ADR_DIRS {
        let dir = root.join(dir);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if matches!(name.as_str(), "readme.md" | "index.md" | "template.md") {
                continue;
            }
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !seen.insert(canonical) {
                continue;
            }
            if let Some(record) = parse_adr_in(root, &path) {
                records.push(record);
            }
        }
    }
    records.sort_by(|a, b| match (a.numeric_id(), b.numeric_id()) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.path.cmp(&b.path)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.id.cmp(&b.id).then_with(|| a.path.cmp(&b.path)),
    });
    records
}

/// Look up one decision by reference (`3`, `0003`, `ADR-3`).
pub fn find_decision(root: &Path, reference: &str) -> Option<AdrRecord> {
    let wanted = id_number(reference)?;
    list_decisions(root)
        .into_iter()
        .find(|r| r.numeric_id() == Some(wanted))
}

// ── Search ─────────────────────────────────────────────────────────────────────

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Search options for [`search`].
#[derive(Debug, Clone, Default)]
pub struct SearchOptions<'a> {
    pub query: &'a str,
    /// Keep only this status (case-insensitive, aliases accepted).
    pub status: Option<&'a str>,
    /// 0 = unlimited.
    pub limit: usize,
}

/// Relevance-ranked search over title, tags, id, and body sections. Every
/// query term must appear somewhere (prefix matches count at half weight);
/// if nothing matches all terms, documents matching any term are returned.
pub fn search(root: &Path, options: &SearchOptions) -> Vec<(AdrRecord, f32)> {
    let wanted_status = options
        .status
        .and_then(|s| s.parse::<DecisionStatus>().ok())
        .map(|s| s.to_string());
    let records: Vec<AdrRecord> = list_decisions(root)
        .into_iter()
        .filter(|r| wanted_status.as_ref().is_none_or(|s| &r.status == s))
        .collect();

    let terms = tokenize(options.query);
    if terms.is_empty() {
        let mut all: Vec<(AdrRecord, f32)> = records.into_iter().map(|r| (r, 0.0)).collect();
        if options.limit > 0 {
            all.truncate(options.limit);
        }
        return all;
    }
    let phrase = options.query.trim().to_lowercase();
    let wanted_id = id_number(options.query);

    let score_record = |r: &AdrRecord, require_all: bool| -> Option<f32> {
        let fields: [(Vec<String>, f32); 5] = [
            (tokenize(&r.title), 5.0),
            (r.tags.iter().flat_map(|t| tokenize(t)).collect(), 4.0),
            (tokenize(&r.decision), 2.0),
            (tokenize(&r.context), 1.0),
            (tokenize(&r.consequences), 1.0),
        ];
        let mut score = 0.0;
        let mut matched_terms = 0;
        for term in &terms {
            let mut term_score = 0.0;
            for (tokens, weight) in &fields {
                let exact = tokens.iter().filter(|t| *t == term).count();
                let prefix = tokens
                    .iter()
                    .filter(|t| t.len() > term.len() && t.starts_with(term.as_str()))
                    .count();
                if exact + prefix > 0 {
                    let tf = (exact as f32 + 0.5 * prefix as f32).min(5.0);
                    term_score += weight * (1.0 + tf.ln_1p());
                }
            }
            if term_score > 0.0 {
                matched_terms += 1;
                score += term_score;
            }
        }
        if let Some(n) = wanted_id {
            if r.numeric_id() == Some(n) {
                score += 50.0;
                matched_terms = terms.len();
            }
        }
        if matched_terms == 0 || (require_all && matched_terms < terms.len()) {
            return None;
        }
        if terms.len() > 1 {
            if r.title.to_lowercase().contains(&phrase) {
                score += 8.0;
            } else if format!("{} {}", r.decision, r.context)
                .to_lowercase()
                .contains(&phrase)
            {
                score += 3.0;
            }
        }
        // Prefer live decisions over replaced ones.
        if matches!(
            r.status.as_str(),
            "Superseded" | "Deprecated" | "Rejected" | "Retired"
        ) {
            score *= 0.7;
        }
        Some(score)
    };

    let mut hits: Vec<(AdrRecord, f32)> = records
        .iter()
        .filter_map(|r| score_record(r, true).map(|s| (r.clone(), s)))
        .collect();
    if hits.is_empty() {
        hits = records
            .iter()
            .filter_map(|r| score_record(r, false).map(|s| (r.clone(), s)))
            .collect();
    }
    hits.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.numeric_id().cmp(&b.0.numeric_id()))
    });
    if options.limit > 0 {
        hits.truncate(options.limit);
    }
    hits
}

/// Compatibility wrapper: ranked search without scores or limits.
pub fn search_decisions(root: &Path, query: &str) -> Vec<AdrRecord> {
    search(
        root,
        &SearchOptions {
            query,
            ..Default::default()
        },
    )
    .into_iter()
    .map(|(r, _)| r)
    .collect()
}

/// Convert parsed ADR Markdown into a model::Decision (for the fact store).
pub fn to_decision(record: &AdrRecord) -> Decision {
    let status: DecisionStatus = record.status.parse().unwrap_or_default();
    let created_at_ms = chrono::NaiveDate::parse_from_str(&record.date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or_else(now_ms);
    Decision {
        id: record.id.clone(),
        title: record.title.clone(),
        status,
        context: record.context.clone(),
        decision: record.decision.clone(),
        consequences: record.consequences.clone(),
        created_at_ms,
        author: "agent".to_string(),
        supersedes: record.supersedes.clone(),
        tags: record.tags.clone(),
        links: vec![record.path.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn new(title: &str, decision: &str) -> NewDecision {
        NewDecision {
            title: title.into(),
            context: format!("context for {title}"),
            decision: decision.into(),
            consequences: "some consequences".into(),
            ..Default::default()
        }
    }

    #[test]
    fn record_and_search_decision() {
        let dir = TempDir::new().unwrap();
        let path = record_decision(
            dir.path(),
            "Use SQLite for Persistence",
            "We need embedded storage.",
            "Adopt rusqlite for all data.",
            "Single dependency, no external server.",
        )
        .unwrap();
        assert!(path.exists());
        assert!(path.starts_with(dir.path().join(DECISIONS_DIR)));

        let results = search_decisions(dir.path(), "SQLite");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Use SQLite for Persistence");
        assert_eq!(results[0].status, "Accepted");
        assert_eq!(
            results[0].path,
            "docs/decisions/0001-use-sqlite-for-persistence.md"
        );
        assert_eq!(search_decisions(dir.path(), "embedded").len(), 1);
        assert_eq!(
            search_decisions(dir.path(), "persist").len(),
            1,
            "prefix match"
        );
    }

    #[test]
    fn dates_are_real_calendar_dates() {
        let date = current_date();
        let parsed = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d");
        assert!(parsed.is_ok(), "bad date {date}");
    }

    #[test]
    fn titles_with_quotes_and_newlines_round_trip() {
        let dir = TempDir::new().unwrap();
        let rec = record(
            dir.path(),
            &new("Use \"quoted\" names: yes\nsecond line", "Do it"),
        )
        .unwrap();
        assert_eq!(rec.title, "Use \"quoted\" names: yes second line");
        let listed = list_decisions(dir.path());
        assert_eq!(listed[0].title, rec.title);
    }

    #[test]
    fn validation_errors() {
        let dir = TempDir::new().unwrap();
        assert!(record(dir.path(), &new("  ", "x")).is_err());
        assert!(record(dir.path(), &new("T", " ")).is_err());
        let mut bad_status = new("T", "x");
        bad_status.status = Some("maybe".into());
        assert!(record(dir.path(), &bad_status).is_err());
        let mut bad_ref = new("T", "x");
        bad_ref.supersedes = Some("42".into());
        assert!(record(dir.path(), &bad_ref).is_err());
    }

    #[test]
    fn ids_increment_and_sort_numerically() {
        let dir = TempDir::new().unwrap();
        for i in 0..11 {
            record(dir.path(), &new(&format!("Decision {i}"), "x")).unwrap();
        }
        let ids: Vec<String> = list_decisions(dir.path())
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids.first().unwrap(), "0001");
        assert_eq!(ids.last().unwrap(), "0011");
        assert_eq!(ids.len(), 11);
    }

    #[test]
    fn concurrent_recording_never_collides() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    record(&root, &new(&format!("Same title {}", i % 2), "x"))
                        .unwrap()
                        .id
                })
            })
            .collect();
        let mut ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 8);
        assert_eq!(list_decisions(&root).len(), 8);
    }

    #[test]
    fn supersede_updates_the_old_record() {
        let dir = TempDir::new().unwrap();
        record(dir.path(), &new("Use MySQL", "MySQL everywhere")).unwrap();
        let mut replacement = new("Use Postgres", "Postgres everywhere");
        replacement.supersedes = Some("1".into());
        replacement.tags = vec!["storage".into(), "db".into()];
        let rec = record(dir.path(), &replacement).unwrap();
        assert_eq!(rec.supersedes.as_deref(), Some("0001"));
        assert_eq!(rec.tags, vec!["storage", "db"]);
        let old = find_decision(dir.path(), "0001").unwrap();
        assert_eq!(old.status, "Superseded");
        assert_eq!(old.superseded_by.as_deref(), Some("0002"));
        assert!(old.decision.contains("MySQL everywhere"), "body preserved");
    }

    #[test]
    fn ranked_search_prefers_title_and_filters_status() {
        let dir = TempDir::new().unwrap();
        record(dir.path(), &new("Adopt event sourcing", "Store events")).unwrap();
        record(
            dir.path(),
            &new("Logging format", "Mention event sourcing once in passing"),
        )
        .unwrap();
        let hits = search(
            dir.path(),
            &SearchOptions {
                query: "event sourcing",
                ..Default::default()
            },
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0.title, "Adopt event sourcing");
        assert!(hits[0].1 > hits[1].1);

        let by_id = search(
            dir.path(),
            &SearchOptions {
                query: "2",
                ..Default::default()
            },
        );
        assert_eq!(by_id[0].0.id, "0002");

        let none = search(
            dir.path(),
            &SearchOptions {
                query: "event",
                status: Some("superseded"),
                ..Default::default()
            },
        );
        assert!(none.is_empty());
        let any_term = search(
            dir.path(),
            &SearchOptions {
                query: "logging kubernetes",
                ..Default::default()
            },
        );
        assert_eq!(
            any_term.len(),
            1,
            "falls back to OR when no record has every term"
        );
    }

    #[test]
    fn reads_adr_tools_format_from_doc_adr() {
        let dir = TempDir::new().unwrap();
        let adr_dir = dir.path().join("doc/adr");
        fs::create_dir_all(&adr_dir).unwrap();
        fs::write(
            adr_dir.join("0001-record-architecture-decisions.md"),
            "# 1. Record architecture decisions\n\nDate: 2016-02-12\n\n## Status\n\nSuperseded by [2. Use MADR](0002-use-madr.md)\n\n## Context\n\nWe need to record decisions.\n\n### Detail\n\nnested heading stays in section\n\n## Decision\n\nWe will use ADRs.\n\n## Consequences\n\nSee Nygard.\n",
        )
        .unwrap();
        fs::write(adr_dir.join("README.md"), "# ADRs\n").unwrap();
        let records = list_decisions(dir.path());
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.id, "0001");
        assert_eq!(r.title, "Record architecture decisions");
        assert_eq!(r.date, "2016-02-12");
        assert_eq!(r.status, "Superseded");
        assert_eq!(r.superseded_by.as_deref(), Some("0002"));
        assert!(r.context.contains("nested heading stays"));
        assert_eq!(r.decision, "We will use ADRs.");
        assert_eq!(
            decisions_dir(dir.path()),
            adr_dir,
            "new records go to the existing directory"
        );
    }

    #[test]
    fn references_are_parsed_strictly() {
        assert_eq!(id_number("3"), Some(3));
        assert_eq!(id_number("0003"), Some(3));
        assert_eq!(id_number("#3"), Some(3));
        assert_eq!(id_number("ADR-3"), Some(3));
        assert_eq!(id_number("adr 0012"), Some(12));
        assert_eq!(id_number("0001-run-migrations-on-deploy-v2"), Some(1));
        assert_eq!(id_number("0001-use-mysql.md"), Some(1));
        assert_eq!(id_number("s3"), None);
        assert_eq!(id_number("oauth2"), None);
        assert_eq!(id_number("v2"), None);
        assert_eq!(id_number(""), None);
    }

    #[test]
    fn supersede_by_file_stem_targets_the_right_record() {
        let dir = TempDir::new().unwrap();
        for i in 0..12 {
            record(dir.path(), &new(&format!("Decision {i}"), "x")).unwrap();
        }
        let mut replacement = new("Replacement", "y");
        replacement.supersedes = Some("0001-decision-0-v2".into());
        record(dir.path(), &replacement).unwrap();
        assert_eq!(find_decision(dir.path(), "1").unwrap().status, "Superseded");
        assert_eq!(find_decision(dir.path(), "12").unwrap().status, "Accepted");
    }

    #[test]
    fn digits_inside_words_do_not_trigger_id_lookup() {
        let dir = TempDir::new().unwrap();
        record(dir.path(), &new("Logging format", "json lines")).unwrap();
        record(dir.path(), &new("Filler", "x")).unwrap();
        record(dir.path(), &new("Store uploads in S3", "use s3 buckets")).unwrap();
        let hits = search(
            dir.path(),
            &SearchOptions {
                query: "s3",
                ..Default::default()
            },
        );
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].0.title, "Store uploads in S3");
    }

    #[test]
    fn madr_sections_and_code_fences() {
        let dir = TempDir::new().unwrap();
        let adr_dir = dir.path().join("docs/adr");
        fs::create_dir_all(&adr_dir).unwrap();
        fs::write(
            adr_dir.join("0001-use-postgres.md"),
            "# Use Postgres\n\n## Context and Problem Statement\n\nWe need a database.\n\n## Decision Drivers\n\n* cost\n\n## Considered Options\n\n* MySQL\n\n## Decision Outcome\n\nChosen option: Postgres.\n\n```sh\n# apply schema\npsql -f schema.sql\n```\n\n### Consequences\n\n* Good, because JSONB.\n",
        )
        .unwrap();
        let r = &list_decisions(dir.path())[0];
        assert_eq!(r.context, "We need a database.");
        assert!(
            r.decision.starts_with("Chosen option: Postgres."),
            "{:?}",
            r.decision
        );
        assert!(
            r.decision.contains("psql -f schema.sql"),
            "code fence kept: {:?}",
            r.decision
        );
        assert!(!r.decision.contains("JSONB"));
        assert_eq!(r.consequences, "* Good, because JSONB.");
    }

    #[test]
    fn search_nonexistent_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        assert!(search_decisions(&dir.path().join("missing"), "anything").is_empty());
    }

    #[test]
    fn to_decision_conversion() {
        let dir = TempDir::new().unwrap();
        record(dir.path(), &new("Test ADR", "dec")).unwrap();
        let decision = to_decision(&search_decisions(dir.path(), "Test")[0]);
        assert_eq!(decision.title, "Test ADR");
        assert_eq!(decision.status, DecisionStatus::Accepted);
        assert!(decision.created_at_ms > 0);
    }

    #[test]
    fn slugify_produces_valid_filename() {
        assert_eq!(slugify("Use Redis Cache!"), "use-redis-cache");
        assert_eq!(slugify("!!!"), "decision");
        assert_eq!(slugify("a  --  b"), "a-b");
        assert!(slugify(&"x".repeat(200)).len() <= 60);
    }
}
