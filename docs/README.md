# Developer documentation

This directory holds documentation for people building or contributing to
OxidePage. If you just want to use the engine, see the [main
README](../README.md) instead.

- [`development.md`](development.md) — prerequisites, build/test/lint
  commands, and the workspace's crate layout.
- [`testing.md`](testing.md) — the conformance test suites (WPT, html5lib,
  goldens, reftests) and the `cargo xtask` runners that drive them.
- [`status.md`](status.md) — implementation status by phase, with links to
  the ADR that recorded each phase's design decisions.
- [`adr/`](adr/) — architecture decision records. ADRs record deviations from
  and refinements of the design baseline, and **win over it where they
  conflict**.

The architecture baseline itself — design principles, pipeline, threading
model, security posture, and the phase plan — lives in
[`rust-engine-design.md`](rust-engine-design.md).
