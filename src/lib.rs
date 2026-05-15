//! micro-mind library surface.
//!
//! The binary in `main.rs` is the primary product. The library facet exists
//! to share the small pieces that bench / replay / summarize tooling needs
//! (the JSONL event schema, fixture format, trace analysis) without having
//! to copy types into every helper binary.
//!
//! Stability: only items re-exported from these modules are considered
//! "library API". Everything else under `src/` is binary-internal and may
//! change without a SemVer bump.

pub mod bench;
pub mod obs;
