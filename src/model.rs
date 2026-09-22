//! The fact model — what trace is allowed to know.
//!
//! Every answer carries its evidence, and every piece of evidence is labelled
//! with its strength:
//!
//!   DECLARED — read from a schema declaration. The project itself asserts this.
//!   USED     — observed in source as a real access. The code demonstrably
//!              touches it.
//!   NAMED    — name resemblance only. The weakest tier, and it says so.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Evidence tiers — structured domain only (code, schema, infra).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Declared,
    Used,
    Named,
}

/// One piece of evidence — a fact with its strength label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub tier: Tier,
    /// Human-readable statement of the fact, always naming its source file.
    pub what: String,
}

// ── RelPath (a path relative to the project root) ──────────────────────────────

/// A file path relative to the project root, forward-slash normalised.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default, Ord, PartialOrd)]
pub struct RelPath(pub String);

impl RelPath {
    pub fn path(&self) -> &str { &self.0 }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
}

// ── Structural symbol model (Phase 1 output, re-exported from structural.rs) ─

pub use crate::structural::{
    CallEdge, Import, ObservationSource, Route, STRUCTURAL_EXTRACTOR_VERSION,
    StructuralFileFacts, StructuralGraph, Symbol, SymbolKind,
};

// ── Index (Phase 1 + Phase 4) ──────────────────────────────────────────────────

/// Everything the scan learned about one repository — the schema/concept layer
/// plus decisions (Phase 3) and execution state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    pub root: String,
    pub files_scanned: usize,
    /// Per-file extraction cache: the incremental engine.
    #[serde(default)]
    pub file_facts: BTreeMap<String, FileFacts>,
    /// archietect.toml [aliases]: concept term → the concept that implements it.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// Archietect.toml [[decision]] entries — these are the ADRs (Phase 3).
    #[serde(default)]
    pub decisions: Vec<Decision>,
    /// Paths excluded from scanning.
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub extractor_version: u32,
    /// Signature of the concept set — if a rescan changes it, all usage is
    /// invalid.
    #[serde(default)]
    pub concepts_sig: String,
}

/// What one file contributed, cached against (size, mtime, extractor version).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub size: u64,
    pub mtime_ms: i64,
    /// Declaration fragments this file asserts.
    #[serde(default)]
    pub decls: Vec<DeclFragment>,
    /// (concept, access-kind) usage hits observed in this file.
    #[serde(default)]
    pub usage: Vec<(String, String)>,
    #[serde(default)]
    pub decl_kinds: Vec<String>,
}

/// One file's assertion about one concept.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeclFragment {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub relations: Vec<String>,
    pub table: Option<String>,
}

// ── ADR model (Phase 3) ────────────────────────────────────────────────────────

/// A declared architectural decision — the WHY behind a shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    /// Lifecycle state: accepted / superseded / rejected / retired.
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    #[default]
    Accepted,
    Superseded,
    Rejected,
    Retired,
}

impl std::str::FromStr for DecisionStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "accepted" | "active" => Ok(Self::Accepted),
            "superseded" => Ok(Self::Superseded),
            "rejected" => Ok(Self::Rejected),
            "retired" => Ok(Self::Retired),
            other => Err(format!("unknown decision status {other:?}")),
        }
    }
}

impl std::fmt::Display for DecisionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecisionStatus::Accepted => write!(f, "Accepted"),
            DecisionStatus::Superseded => write!(f, "Superseded"),
            DecisionStatus::Rejected => write!(f, "Rejected"),
            DecisionStatus::Retired => write!(f, "Retired"),
        }
    }
}

// ── Constraint rule model (Phase 2) ────────────────────────────────────────────

/// A single architectural invariant rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    /// Glob pattern for files the rule applies to (e.g. "src/controllers/*").
    pub target_path: String,
    /// Imports that are forbidden in matching files.
    #[serde(default)]
    pub forbidden_imports: Vec<String>,
    /// Imports that are required in matching files.
    #[serde(default)]
    pub required_imports: Vec<String>,
    /// Human-readable explanation of the rule.
    pub message: String,
    /// Severity: "error" (block) or "warning" (advisory).
    #[serde(default)]
    pub severity: RuleSeverity,
    /// Tags for categorization.
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuleSeverity {
    #[default]
    Error,
    Warning,
}

/// The rules file schema: `.architectural-rules.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RulesConfig {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// One violation found when evaluating a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub rule_id: String,
    pub severity: RuleSeverity,
    pub message: String,
    /// The file that would violate the rule.
    pub file: String,
    /// The specific forbidden import or missing required import.
    pub detail: String,
}

// ── Session / execution memory (Phase 4) ───────────────────────────────────────

/// A session record in the execution-memory log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub timestamp_ms: i64,
    pub agent_name: String,
    pub summary: String,
}

/// A touched-file record in the execution-memory log.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
