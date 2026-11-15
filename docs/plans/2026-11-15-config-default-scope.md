---
plan_format_version: 2
plan_id: PLAN-20261115-config-default-scope
title: Configure a fallback memory scope
status: implemented
created_at: 2026-11-15
updated_at: 2026-11-15
owner: operator
source:
  type: manual
  url: null
  external_id: null
  imported_at: null
bug:
  summary: Non-project memory use requires an environment scope.
  severity: low
  affected_area: configuration
  user_impact: Non-project scoped commands fail.
execution:
  requires_operator_approval_before_implementation: false
  requires_plan_updates_during_execution: true
  task_graph_status: complete
project: null
---

<!-- AGENT_GENERATED: t; AGENT_MODEL: GPT-6.1; AGENT_DATE: 2026-11-15 -->

# ADR: Configure a fallback memory scope

## Problem Statement

Non-project commands cannot store or retrieve scoped memories without an
environment override. The operator requested a configurable fallback, not a
built-in scope.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

The operator approved a config file supplying default_scope and approved using
installed planner 0.5.2 instead of the broken pinned executable.

## Context

Config::resolve tries environment, project metadata, clanker, and repository
directory. Outside these cases scoped commands fail. The CLI resolves one
configuration for CLI and MCP operation.

## Constraints

Keep existing precedence and memory rows unchanged. Parse typed TOML and report
invalid files with their paths. Preserve the unrelated mise.lock modification
in the main checkout.

## Non-Goals

No automatic personal scope, database migration, config-writing command, or
change to agent attribution.

## Decision

Read $XDG_CONFIG_HOME/mnene/config.toml, else $HOME/.config/mnene/config.toml.
A present file requires nonblank string default_scope and rejects unknown keys.
Missing files are allowed. Load once at the process edge before opening storage;
use the value after all existing scope tiers and record scope_source as config.
No path is invented when both home variables are absent.

## Alternatives Considered

The operator rejected a built-in default. An exported scope overrides project
discovery and does not serve as a fallback.

## Consequences

Project use remains unchanged without a file. Non-project use needs explicit
configuration. Invalid configuration stops domain commands. A documented
CLI-005 exception allows the configured scope without adding a provenance flag.

## Operator Guidance Log

2026-11-15: The operator approved planner 0.5.2 for this task.

2026-11-15: The operator approved repairing verification tools without changing
their pinned versions. Missing Nix loader paths were replaced in locally
installed binaries; the pinned planner 0.5.0 now runs as well.

## Decision Log

2026-11-15: Preserve all existing scope tiers and add configuration last.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: Implement and verify configurable fallback scope
    status: done
    depends_on: []
    owner: root
    description: Add typed configuration loading and final-tier scope fallback.
    work_items:
      - Load typed TOML, record config provenance, test errors, and document setup.
    invariants:
      - Existing scope tiers override configuration; no built-in scope is added.
    acceptance_checks:
      - Run mise run check and mise run check:coverage.
    completion_evidence: >
      Implemented config loading and fallback in src/config.rs, wired it into
      src/main.rs, added config provenance and errors in src/model.rs, updated
      src/agent_surface.rs, documented setup in README.md, and added tests in
      tests/cli.rs and tests/mcp.rs. Cargo.toml and Cargo.lock reference the
      already-locked TOML dependency without moving dependency versions.
      mise run check passed; mise run check:coverage passed with 316 tests and
      100% line coverage. git diff --check passed. The main checkout's mise.lock
      modification remains untouched.
```
<!-- TASK_GRAPH:END -->

# Task Details

## T001 — Implement and verify configurable fallback scope

Status: `done`

Depends on: none

### Description

Implement the typed loader, fallback, provenance label, documentation, and tests
against real CLI and storage behavior.

### Invariants

- Existing scope tiers override configuration; no built-in scope is added.

### Acceptance Checks

- Run `mise run check` and `mise run check:coverage`.

### Completion Evidence

Implementation has not started.

2026-11-15: Implementation and tests added in src/config.rs, src/main.rs,
src/model.rs, src/agent_surface.rs, tests/cli.rs, tests/mcp.rs, Cargo.toml,
Cargo.lock, and README.md. All 315 tests passed. Initial full checks found a
broken rustdoc link, now corrected. Coverage identified closing-line mappings
in a new test; fixture setup was split into named inputs without removing any
assertions. Verification is being rerun.

2026-11-15: Added direct library error tests after coverage showed an
unexercised library instantiation of the config-error closures. All 316 tests
pass, line coverage is 100%, and mise run check passes every gate, including
documentation, audits, packaging, and lockfile checks. Changed paths are
src/config.rs, src/main.rs, src/model.rs, src/agent_surface.rs, tests/cli.rs,
tests/mcp.rs, Cargo.toml, Cargo.lock, README.md, and this plan. The lockfile
change adds only a reference to the already-locked TOML crate; no dependency
versions moved. The original checkout's mise.lock change remains untouched.

# Execution Protocol

Validate the plan before implementation. Update task state and date as work
proceeds. On completion, record verification and changed paths in both task
representations. Preserve guidance, decisions, and evidence by appending. If a
requirement or environment failure blocks progress, record it and ask the
operator rather than changing the requirements.
