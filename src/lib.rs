//! trace — an architectural memory engine for Rust projects.
//!
//! Implements a 4-phase roadmap:
//!
//! - Phase 1: Fast AST Indexer — tree-sitter based symbol/import/route extraction
//!            with incremental caching and MCP tools `get_symbol_outline`,
//!            `find_callers`, `get_imports`.
//! - Phase 2: Constraint & Guardrail Engine — architectural invariants loaded
//!            from `.architectural-rules.json`, evaluated via `eval_plan`.
//! - Phase 3: ADR Graph — Architecture Decision Records stored in
//!            `docs/decisions/` (one YAML file per decision) plus a SQLite
//!            index, queried via `search_decisions` and recorded via
//!            `record_decision`.
//! - Phase 4: Execution Memory — append-only event log in SQLite (sessions,
//!            touched files, structural mutations), surfaced via
//!            `get_recent_history`.
//!
//! Everything is deterministic and offline — no API keys, no network, no LLM.
//! AI tools are CLIENTS of this engine through MCP, never components of it.

pub mod adr;
pub mod humanize;
pub mod invariant;
pub mod mcp;
pub mod model;
pub mod root;
pub mod scan;
pub mod store;
pub mod structural;
pub mod tree_sitter_detector;

/// Bump to invalidate every cached extraction (a changed extractor = a changed
/// compiler — old object files are lies).
pub const EXTRACTOR_VERSION: u32 = 1;
