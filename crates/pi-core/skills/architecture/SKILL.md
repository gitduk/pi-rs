---
name: architecture
description: Design, review, or refactor code structure, types, module boundaries, dependencies, state, and persistence.
---

# Architecture

Design before making structural changes. Scale analysis and verification to the change's complexity and reversibility.

For design-only requests, report the design without editing. For reviews, report findings without editing.

## Process

1. Identify requirements, constraints, and acceptance checks.
2. Inspect relevant modules, data ownership, dependencies, and state mutations.
3. Find structural mismatches, duplication, and unnecessary complexity.
4. Design the simplest data model and boundaries that satisfy the requirements.
5. Compare alternatives when tradeoffs materially affect correctness, maintenance, or reversibility.
6. Implement in verifiable steps, preserving behavior unless changes are requested.
7. Run relevant checks and report the design, changes, and remaining risks.

## Principles

### Modeling
- Represent domain invariants in types when practical.
- Prefer fewer meaningful types; avoid redundant wrappers, variants, and optional fields.
- Use consistent names and preserve clear existing code.
- Do not bypass type safety to accommodate a design.

### Boundaries
- Validate external data at entry points.
- Separate business rules from I/O and other effects where practical.
- Keep dependencies directional; inject higher-level knowledge rather than coupling lower layers to callers.

### Ownership
- Define rules, constants, and formats once.
- Minimize shared mutable state; establish clear ownership and controlled writes.
- Make retryable operations idempotent where required.

### Simplicity
- Remove obsolete code and abstractions made unnecessary by the change.
- Keep presentation concerns out of domain models.
- Avoid special cases, flags, and duplicated branches that indicate a structural mismatch.

### Reversibility
- Invest more design effort in decisions that are expensive to reverse, especially public APIs, data formats, and persisted state.

## Review

Report actionable findings by severity, with supporting paths and line numbers, impact, and recommended fixes. Distinguish verified issues from assumptions. Briefly acknowledge strengths when relevant.

## Refactoring

Identify existing behavioral coverage before restructuring. Add focused tests for meaningful gaps. Separate structural changes from behavioral changes when practical, and verify preserved behavior afterward.

For large mechanical changes, validate a representative transformation before applying it broadly.
