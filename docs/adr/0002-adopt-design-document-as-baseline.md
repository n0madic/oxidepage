# ADR-0002: Adopt rust-engine-design.md as the architecture baseline

- Status: accepted
- Date: 2026-07-03

## Context

The project starts from a complete greenfield design (`docs/rust-engine-design.md`):
principles P1–P7, component selection (html5ever, stylo, taffy, parley,
QuickJS-NG, tiny-skia, …), workspace layout (§4.2), and a phased implementation
plan (§10).

## Decision

The design document is adopted as-is as the architecture baseline. The
workspace mirrors §4.2 with crates named `oxidepage-<component>`; crates whose
phase has not arrived exist as documented stubs so the dependency graph and CI
matrix are exercised from day one (Phase 0 exit criterion).

Two naming refinements relative to the document:

- The project/crate prefix is `oxidepage` rather than the placeholder `engine`;
  the CLI binary is `oxidepage`.
- The workspace root is the repository root (the document's `engine/` directory
  corresponds to the repository itself).

## Consequences

- Later phases have a fixed home and dependency direction from the start:
  `engine → page → {dom, style, layout, paint, net, bindings} → base`.
- Design changes require an ADR, keeping the baseline authoritative.
