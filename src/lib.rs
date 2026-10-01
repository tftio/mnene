//! Core library for `mnene`, an agent-to-agent memory store.
//!
//! This module only declares submodules and re-exports; behavior lives in
//! each submodule.
//!
//! `mnene` gives agent harness sessions a narrow, durable way to leave
//! memories for later sessions and to receive, at start, the memories
//! earlier sessions left in the same project or task. This crate holds the
//! domain model, query construction, configuration resolution, `SQLite`
//! storage, and the MCP server loop; `src/main.rs` is a thin CLI shell over
//! it. The README describes the command surface, the configuration
//! resolution order, and the storage model.

/// Declared agent capabilities for the supervised agent surface.
///
/// `put`, `get`, `search`, `recall`, `overwrite`, `retract`, and `scopes`.
pub mod agent_surface;

/// Domain types and their pure validation rules.
///
/// Identifiers, memories, provenance, and errors.
pub mod model;

/// FTS5 query construction and score conversion.
///
/// Builds a safe match expression from user query text and converts an
/// FTS5 `bm25()` value into the reported search score.
pub mod query;

/// Configuration resolution and project discovery.
///
/// Resolves `Config` from process-edge inputs: `scope` through the T013
/// tiered project resolution order, `task` from the current git branch.
pub mod config;

/// SQLite-backed storage for memories.
pub mod store;

/// The Model Context Protocol server loop served over stdio.
pub mod mcp;

/// The `tftio-lib` doctor adapter (`meta doctor`'s tool-specific check).
pub mod doctor;

/// CLI verb shapes and the narrow domain-dispatch adapter `src/main.rs`
/// uses to route them through `tftio_lib::run_cli_from`.
pub mod verb;
