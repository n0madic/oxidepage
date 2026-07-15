# ADR-0001: Record architecture decisions

- Status: accepted
- Date: 2026-07-03

## Context

OxidePage is a long-running, multi-phase project (see `docs/rust-engine-design.md`).
Load-bearing decisions must survive team and context changes, and the design
document itself needs a change-control mechanism finer-grained than editing it
in place.

## Decision

We record architecturally significant decisions as ADRs in `docs/adr/`,
numbered sequentially, using the template in `0000-template.md`. A decision is
"architecturally significant" when it affects crate boundaries, public API
shape, dependency selection, security posture, or a design principle (P1–P7 in
the design document).

The design document `docs/rust-engine-design.md` is the architecture baseline
(see ADR-0002). ADRs record deviations from and refinements of that baseline;
they win over the design document where they conflict.

## Consequences

- Every deviation from the design document leaves a written trace with its
  rationale.
- Reviewers can demand an ADR before approving a structural change.
- The overhead is one short markdown file per significant decision.
