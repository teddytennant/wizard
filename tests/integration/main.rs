//! Every integration test in one binary.
//!
//! These were five files directly under `tests/`, which is five targets:
//! cargo builds and links one test binary per file there, and each of those
//! links the whole crate. That bought nothing. The five of them run 38 tests
//! in about 1.5 seconds put together, so the cost was almost entirely the
//! linking, paid five times over on a 223k-line crate against 729
//! dependencies.
//!
//! Adding a test file here means adding a `mod` line below. A new file
//! directly under `tests/` still works and still gets its own target, so
//! reach for that only when a test genuinely needs its own process; nothing
//! here does, since the ones that shell out spawn a child anyway.
//!
//! The `cfg` gates were `#![cfg(...)]` at the top of their own files. A module
//! cannot gate itself out of its parent, so they sit on the `mod` lines now
//! and mean exactly what they meant before: the file compiles to nothing
//! without those features.

#[cfg(all(feature = "provider-ollama", feature = "acp"))]
mod acp;
mod cli;
#[cfg(all(feature = "native", feature = "graph"))]
mod graph_explorer;
#[cfg(feature = "mesh")]
mod mesh_quic;
mod recorded_provider;
