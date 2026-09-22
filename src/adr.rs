//! Phase 3: Architecture Decision Record Graph.
//!
//! Captures immutable architecture decisions so agents can check *why* past
//! choices were made before rewriting logic. ADRs are stored as Markdown files
//! in `docs/decisions/` (human-editable, git-friendly) with an optional SQLite
//! cache for fast search.
//!
//! MCP tools: `search_decisions`, `record_decision`.

use crate::humanize::now_ms;
use crate::model::{Decision, DecisionStatus};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// The directory where ADR Markdown files live.
pub const DECISIONS_DIR: &str = "docs/decisions";

/// A searchable ADR record (mirrors the Markdown front-matter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdrRecord {
    pub id: String, // e.g. "0001"
    pub title: String,
    pub status: String, // Accepted | Superseded | Proposed | Deprecated
    pub date: String,   // YYYY-MM-DD
    pub context: String,
    pub decision: String,
    #[serde(default)]
    pub consequences: String,
    #[serde(default)]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub path: String, // relative path to the .md file
}

impl AdrRecord {
    pub fn new(title: &str, context: &str, decision: &str, consequences: &str) -> Self {
        AdrRecord {
            id: String::new(),
            title: title.to_string(),
            status: "Accepted".to_string(),
            date: current_date(),
            context: context.to_string(),
            decision: decision.to_string(),
            consequences: consequences.to_string(),
            supersedes: None,
            tags: Vec::new(),
            path: String::new(),
        }
    }
}

fn current_date() -> String {
    let now = now_ms();
    let secs = now / 1000;
    // Simple date formatting without chrono
    let days_since_epoch = secs / 86400;
    let day_of_year = (days_since_epoch % 365) as u32 + 1;
    let year = 1970 + (days_since_epoch / 365);
    format!(
        "{}-{:02}-{:02}",
        year,
        (day_of_year / 30).min(12),
        (day_of_year % 30).min(30)
    )
}

/// Find the next available ADR sequence number.
pub fn next_id(decisions_dir: &Path) -> String {
    let mut max: u32 = 0;
    if let Ok(entries) = fs::read_dir(decisions_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".md") {
                let stem = &name[..name.len() - 3];
                if let Some(num_str) = stem.split('-').next() {
                    if let Ok(n) = num_str.parse::<u32>() {
                        if n > max {
                            max = n;
                        }
                    }
                }
            }
        }
    }
    format!("{:04}", max + 1)
}

/// Record a new ADR as a Markdown file in `docs/decisions/`.
pub fn record_decision(
    root: &Path,
    title: &str,
    context: &str,
    decision: &str,
    consequences: &str,
) -> PathBuf {
    let decisions_dir = root.join(DECISIONS_DIR);
    fs::create_dir_all(&decisions_dir).unwrap();

    let id = next_id(&decisions_dir);
    let filename = format!("{}-{}.md", id, slugify(title));
    let path = decisions_dir.join(&filename);

    let md = format!(
        r#"---
id: {id}
title: "{title}"
status: Accepted
date: {date}
---

# {title}

## Context

{context}

## Decision

{decision}

## Consequences

{consequences}
"#,
        id = id,
        title = title,
        date = current_date(),
        context = context,
        decision = decision,
        consequences = consequences,
    );

    fs::write(&path, md).unwrap();
    path
}

/// Parse the front-matter of an ADR Markdown file into an `AdrRecord`.
pub fn parse_adr(path: &Path) -> Option<AdrRecord> {
    let text = fs::read_to_string(path).ok()?;
    let rel = path
        .strip_prefix(&std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()))
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");

    let mut record = AdrRecord::new("", "", "", "");
    record.path = rel.to_string();

    // Parse front matter
    // Simple front-matter parser
    let mut title = String::new();
    let mut id = String::new();
    let mut status = "Accepted".to_string();
    let mut date = String::new();
    let mut body_start = 0;

    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > 0 && lines[0].trim() == "---" {
        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                continue;
            }
            if line.trim() == "---" {
                body_start = i + 1;
                break;
            }
            if let Some(v) = line.strip_prefix("id: ") {
                id = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("title: ") {
                title = v.trim().trim_matches('"').to_string();
            } else if let Some(v) = line.strip_prefix("status: ") {
                status = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("date: ") {
                date = v.trim().to_string();
            }
        }
    }

    record.id = id;
    record.title = title;
    record.status = status;
    record.date = date;

    // Extract body sections
    let body = lines[body_start..].join("\n");
    record.context = extract_section(&body, "## Context");
    record.decision = extract_section(&body, "## Decision");
    record.consequences = extract_section(&body, "## Consequences");

    Some(record)
}

fn extract_section(body: &str, header: &str) -> String {
    let start = match body.find(header) {
        Some(s) => s + header.len(),
        None => return String::new(),
    };
    let rest = &body[start..];
    let next_header = rest.find("## ").unwrap_or(rest.len());
    rest[..next_header].trim().to_string()
}

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Search ADRs by keyword in title, context, decision, and consequences.
pub fn search_decisions(root: &Path, query: &str) -> Vec<AdrRecord> {
    let decisions_dir = root.join(DECISIONS_DIR);
    if !decisions_dir.is_dir() {
        return Vec::new();
    }
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    if let Ok(entries) = fs::read_dir(&decisions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(record) = parse_adr(&path) {
                let haystack = format!(
                    "{} {} {} {} {}",
                    record.title, record.context, record.decision, record.consequences, record.id
                )
                .to_lowercase();
                if query_lower.is_empty() || haystack.contains(&query_lower) {
                    results.push(record);
                }
            }
        }
    }
    results.sort_by(|a, b| a.id.cmp(&b.id));
    results
}

/// Convert parsed ADR Markdown into a model::Decision (for the fact store).
pub fn to_decision(record: &AdrRecord) -> Decision {
    let status: DecisionStatus = record.status.parse().unwrap_or(DecisionStatus::Retired);
    Decision {
        id: record.id.clone(),
        title: record.title.clone(),
        status,
        context: record.context.clone(),
        decision: record.decision.clone(),
        consequences: record.consequences.clone(),
        created_at_ms: now_ms(),
        author: "agent".to_string(),
        supersedes: record.supersedes.clone(),
        tags: record.tags.clone(),
        links: Vec::new(),
    }
}

/// List all decisions in the ADR directory, sorted by ID.
pub fn list_decisions(root: &Path) -> Vec<AdrRecord> {
    let decisions_dir = root.join(DECISIONS_DIR);
    if !decisions_dir.is_dir() {
        return Vec::new();
    }
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(&decisions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(record) = parse_adr(&path) {
                results.push(record);
            }
        }
    }
    results.sort_by(|a, b| a.id.cmp(&b.id));
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static ANR: AtomicU64 = AtomicU64::new(0);

    fn tmp_decisions_dir() -> PathBuf {
        let n = ANR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("trace_adr_{}_{}", std::process::id(), n));
        let decisions = dir.join(DECISIONS_DIR);
        fs::create_dir_all(&decisions).unwrap();
        dir
    }

    #[test]
    fn record_and_search_decision() {
        let dir = tmp_decisions_dir();
        let path = record_decision(
            &dir,
            "Use SQLite for Persistence",
            "We need embedded storage.",
            "Adopt rusqlite for all data.",
            "Single dependency, no external server.",
        );
        assert!(path.exists());

        let results = search_decisions(&dir, "SQLite");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Use SQLite for Persistence");
        assert_eq!(results[0].status, "Accepted");

        let results_by_context = search_decisions(&dir, "embedded");
        assert_eq!(results_by_context.len(), 1);
    }

    #[test]
    fn next_id_increments() {
        let dir = tmp_decisions_dir();
        record_decision(&dir, "First", "c1", "d1", "con1");
        record_decision(&dir, "Second", "c2", "d2", "con2");
        let decisions = list_decisions(&dir);
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].id, "0001");
        assert_eq!(decisions[1].id, "0002");
    }

    #[test]
    fn search_empty_query_returns_all() {
        let dir = tmp_decisions_dir();
        record_decision(&dir, "First", "c1", "d1", "con1");
        record_decision(&dir, "Second", "c2", "d2", "con2");
        let results = search_decisions(&dir, "");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_nonexistent_dir_returns_empty() {
        let dir = std::env::temp_dir().join(format!("trace_nonexistent_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let results = search_decisions(&dir, "anything");
        assert!(results.is_empty());
    }

    #[test]
    fn to_decision_conversion() {
        let dir = tmp_decisions_dir();
        let _ = record_decision(&dir, "Test ADR", "ctx", "dec", "con");
        let results = search_decisions(&dir, "Test");
        assert_eq!(results.len(), 1);
        let decision = to_decision(&results[0]);
        assert_eq!(decision.title, "Test ADR");
        assert_eq!(decision.status, DecisionStatus::Accepted);
    }

    #[test]
    fn slugify_produces_valid_filename() {
        let dir = tmp_decisions_dir();
        let path = record_decision(&dir, "Use Redis Cache!", "c", "d", "con");
        assert!(path.exists());
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(filename.contains("use-redis-cache"));
    }
}
