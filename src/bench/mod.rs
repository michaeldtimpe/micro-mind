//! Benchmarking & replay primitives.
//!
//! This module is library-only — every entry point under `src/bin/` reads
//! from here, so the bench/replay/summarize/compare tools all share the
//! same fixture and trace-event types.
//!
//! Goals (Phase 2/3/4):
//!   - Deterministic fixtures (TOML) with predicate-style expectations.
//!   - Trace = JSONL written by `--record`. We re-parse it back into typed
//!     events here. The parser is intentionally tolerant: unknown event
//!     variants are dropped, not errors.
//!   - Aggregate stats per trace: tool counts, guard fires, tokens, wall.
//!   - Compare two summaries (baseline vs candidate) with explicit
//!     regression thresholds.
//!
//! Non-goal: do not import the model / agent loop here. All bench code is
//! supposed to be runnable with no Rust-side dependency on llama-server,
//! so that `bench-replay`, `bench-summarize`, and `bench-compare` can run
//! in CI on a machine with no GPU.

pub mod fixture;
pub mod summary;
pub mod trace;

pub use fixture::{Fixture, MustContain, TaskExpect};
pub use summary::{Summary, TaskOutcome, summarize_trace};
pub use trace::{TraceEvent, parse_jsonl_file};
