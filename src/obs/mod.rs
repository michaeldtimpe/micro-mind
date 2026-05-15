//! Observability: append-only JSONL event recording.
//!
//! Phase 0/1 of the optimization/benchmarking work. The harness emits
//! structured events (chat request/response, tool call/result, guard,
//! stop) to a per-session JSONL file when the user passes `--record`.
//!
//! Design goals:
//!   - Append-only, line-delimited JSON. One event per line. Stable schema.
//!   - Default to a no-op recorder so all existing code paths stay free
//!     when recording isn't enabled.
//!   - Cheap, panic-free. A failed write is logged once to stderr and
//!     subsequent writes are dropped — recording must never break a run.
//!
//! Schema lives in `obs/schema.md` at the repo root.

pub mod recorder;

pub use recorder::{Event, JsonlRecorder, NoopRecorder, Recorder, RecorderHandle, SCHEMA_V};
