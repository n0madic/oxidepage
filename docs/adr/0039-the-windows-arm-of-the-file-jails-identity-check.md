# ADR-0039: The Windows arm of the file jail's identity check

- Status: accepted
- Date: 2026-08-21

## Context

ADR-0038 D4 closed the `file_root` jail's TOCTOU window by confirming that the
handle `open` returned is the object `canonicalize` cleared, and recorded the
decision as costing "no `unsafe`, no `openat2`, no new dependency — which is
what workspace-wide `unsafe_code = "deny"` requires".

That was true of the Unix arm and untrue of the Windows one. `dev`+`ino` has no
Windows equivalent that `std` exposes on stable: the pair that identifies an
object is the volume serial number plus the file index, both of which live on
`std::os::windows::fs::MetadataExt` behind the `windows_by_handle` feature gate
(rust-lang/rust#63010, unstable since 2019 and with no stabilisation in sight,
because the values are not the durable identity the API shape implies). So the
arm as written could only ever compile on nightly, and the Windows CI job that
would have said so was the one job the commit did not get a green run from.

The arms are not interchangeable: dropping the check on Windows would make the
jail path-only there, i.e. exactly the hole D4 exists to close, on the platform
whose junction/symlink surface is the reason to want it closed.

## Decision

`same_object`'s Windows arm compares `same_file::Handle`s instead of reading the
unstable `Metadata` accessors. `same-file` (BurntSushi, 1.0.x since 2019, already
in the tree as `walkdir`'s dependency) calls `GetFileInformationByHandle` and
compares the same volume-serial-number + file-index pair, so the security
property is unchanged; what changes is who writes the `unsafe`.

The dependency is declared under `[target.'cfg(windows)'.dependencies]`, so
Unix and wasm builds acquire nothing. Both sides of the comparison must be
handles, so the arm still opens the canonical path a second time rather than
`stat`ing it, and passes a `try_clone` of our handle because `Handle` owns and
closes what it is given.

ADR-0038 D4's "no new dependency" is amended to "no new dependency on Unix; one
120-line Windows shim over the same syscall, because the alternative is `unsafe`
in a security check".

## Consequences

The workspace still denies `unsafe_code` with a single documented exception
(`dom/src/stylo.rs`), and the jail's guarantee — the bytes came from the inode
that was vetted — now holds on every tier-1 platform rather than on the two that
CI happened to compile.

The cost is one more crate in the Windows dependency graph, and it is a crate
whose only job is this comparison. `same-file` pulls `winapi-util` →
`windows-sys`, both already present transitively, so the advisory surface does
not widen.

More generally: an arm behind a `#[cfg]` the local machine never builds is not
covered by "it compiles", and a red CI job on the platform that arm targets is
the only thing that would have caught this one. A `cfg(windows)` code path
belongs in the same commit as a green Windows run, not merely in the same commit
as a green `cargo build` on macOS.
