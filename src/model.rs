//! Shared data model — paths, decisions and execution-memory records.
//!
//! Structural facts (symbols, imports, call edges, routes) live in
//! `structural.rs` and are re-exported here for convenience.

use serde::{Deserialize, Serialize};

// ── RelPath (a path relative to the project root) ──────────────────────────────

/// A file path relative to the project root, forward-slash normalised.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default, Ord, PartialOrd)]
pub struct RelPath(pub String);

impl RelPath {
    pub fn new(path: &str) -> Self {
        RelPath(normalize_rel(path))
    }

    pub fn path(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Normalise a relative path: forward slashes, no `.`/empty segments, no
/// leading `./`. `..` segments are preserved — escaping the root is checked
/// separately by `root::resolve_in_root`.
pub fn normalize_rel(path: &str) -> String {
    let unified = path.trim().replace('\\', "/");
    unified
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect::<Vec<_>>()
        .join("/")
}

// ── Structural symbol model (re-exported from structural.rs) ───────────────────

pub use crate::structural::{
    CallEdge, FileFacts, Import, IndexedFile, ObservationSource, Route, StructuralGraph, Symbol,
    SymbolKind, STRUCTURAL_EXTRACTOR_VERSION,
};

// ── ADR model (Phase 3) ────────────────────────────────────────────────────────

/// A declared architectural decision — the WHY behind a shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    /// Lifecycle state.
    #[serde(default)]
    pub status: DecisionStatus,
    pub context: String,
    pub decision: String,
    pub consequences: String,
    /// When this decision was first recorded.
    #[serde(default)]
    pub created_at_ms: i64,
    /// Who/what proposed this decision.
    #[serde(default)]
    pub author: String,
    /// ID of a decision this one supersedes.
    #[serde(default)]
    pub supersedes: Option<String>,
    /// Tags for categorization/search.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Related file paths or concept names.
    #[serde(default)]
    pub links: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    Proposed,
    #[default]
    Accepted,
    Superseded,
    Deprecated,
    Rejected,
    Retired,
}

impl DecisionStatus {
    pub const ALL: &'static [DecisionStatus] = &[
        DecisionStatus::Proposed,
        DecisionStatus::Accepted,
        DecisionStatus::Superseded,
        DecisionStatus::Deprecated,
        DecisionStatus::Rejected,
        DecisionStatus::Retired,
    ];
}

impl std::str::FromStr for DecisionStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Only the leading word counts: "Superseded by [3. Use X](...)".
        let word = value
            .trim()
            .split(|c: char| !c.is_alphabetic())
            .find(|w| !w.is_empty())
            .unwrap_or("")
            .to_ascii_lowercase();
        match word.as_str() {
            "proposed" | "draft" | "pending" => Ok(Self::Proposed),
            "accepted" | "active" | "approved" | "adopted" => Ok(Self::Accepted),
            "superseded" | "replaced" => Ok(Self::Superseded),
            "deprecated" | "obsolete" => Ok(Self::Deprecated),
            "rejected" | "declined" => Ok(Self::Rejected),
            "retired" => Ok(Self::Retired),
            _ => Err(format!(
                "unknown decision status {value:?} (expected proposed, accepted, superseded, deprecated, rejected or retired)"
            )),
        }
    }
}

impl std::fmt::Display for DecisionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DecisionStatus::Proposed => "Proposed",
            DecisionStatus::Accepted => "Accepted",
            DecisionStatus::Superseded => "Superseded",
            DecisionStatus::Deprecated => "Deprecated",
            DecisionStatus::Rejected => "Rejected",
            DecisionStatus::Retired => "Retired",
        })
    }
}

// ── Session / execution memory (Phase 4) ───────────────────────────────────────

/// A session record in the execution-memory log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    pub session_id: String,
    pub timestamp_ms: i64,
    pub agent_name: String,
    pub summary: String,
}

/// A touched-file record in the execution-memory log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TouchedFileRecord {
    pub session_id: String,
    pub file_path: String,
    pub change_reason: String,
}

/// An event in the execution-memory log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalEvent {
    pub session_id: String,
    pub timestamp_ms: i64,
    pub agent_name: String,
    pub event_type: String,
    pub detail: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rel_cleans_separators_and_dots() {
        assert_eq!(normalize_rel("./src//lib.rs"), "src/lib.rs");
        assert_eq!(normalize_rel("src\\a\\b.rs"), "src/a/b.rs");
        assert_eq!(normalize_rel(" src/./x.ts "), "src/x.ts");
        assert_eq!(normalize_rel("../outside"), "../outside");
    }

    #[test]
    fn decision_status_parses_leading_word_and_aliases() {
        assert_eq!("accepted".parse(), Ok(DecisionStatus::Accepted));
        assert_eq!("Proposed".parse(), Ok(DecisionStatus::Proposed));
        assert_eq!(
            "Superseded by [3. Use Postgres](0003-use-postgres.md)".parse(),
            Ok(DecisionStatus::Superseded)
        );
        assert_eq!("deprecated".parse(), Ok(DecisionStatus::Deprecated));
        assert!("banana".parse::<DecisionStatus>().is_err());
    }
}
