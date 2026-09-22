//! trace — an architectural memory engine, served over the Model Context
//! Protocol.
//!
//! - Phase 1: Structural index — one tree-sitter pass per file extracts
//!   symbols, imports, call edges and HTTP routes for Rust, Python,
//!   TypeScript/JavaScript, Go and Java; the index is persisted in SQLite and
//!   refreshed incrementally (`structural`, `scan`).
//! - Phase 2: Constraint & guardrail engine — architectural invariants from
//!   `.architectural-rules.json`/`.yaml` or `trace.toml`, evaluated against
//!   proposed changes (`invariant`).
//! - Phase 3: ADR graph — Architecture Decision Records as Markdown with YAML
//!   front matter (plus existing adr-tools/MADR records), ranked search and
//!   supersession (`adr`).
//! - Phase 4: Execution memory — sessions, touched files and events in SQLite
//!   (`store`).
//!
//! Transports (`daemon`) serve the MCP tool surface (`mcp`) inline over
//! stdio or through a shared per-project daemon.
//!
//! Everything is deterministic and offline — no API keys, no network, no LLM.
//! AI tools are CLIENTS of this engine through MCP, never components of it.

pub mod adr;
pub mod daemon;
pub mod humanize;
pub mod invariant;
pub mod mcp;
pub mod model;
pub mod root;
pub mod scan;
pub mod service;
pub mod setup;
pub mod store;
pub mod structural;
pub mod tree_sitter_detector;

/// Bump to invalidate every cached extraction (a changed extractor = a changed
/// compiler — old object files are lies).
pub const EXTRACTOR_VERSION: u32 = structural::STRUCTURAL_EXTRACTOR_VERSION;
