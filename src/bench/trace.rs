//! Read JSONL traces back into typed events.
//!
//! Tolerance rules:
//!   - Lines we can't parse at all are skipped with a warning to stderr.
//!   - Unknown event variants are skipped, not errors. The schema is
//!     supposed to be additive; consumers must keep working when a new
//!     event lands upstream.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::obs::Event;

/// One parsed line from a JSONL trace.
#[derive(Debug, Clone)]
pub struct TraceEvent {
    pub ts_ms: u64,
    pub event: Event,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    ts_ms: u64,
    payload: serde_json::Value,
}

/// Parse a JSONL file, skipping unparseable / unknown-variant lines.
///
/// Returns the events that *did* parse, in file order. Failures are reported
/// to stderr but never fatal — the bench tools should produce a partial
/// summary rather than crash on a corrupt trailing line.
pub fn parse_jsonl_file(path: impl AsRef<Path>) -> Result<Vec<TraceEvent>> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("open trace {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for (i, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let env: Envelope = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => {
                eprintln!(
                    "(trace {} line {}: envelope parse failed, skipping)",
                    path.display(),
                    i + 1
                );
                skipped += 1;
                continue;
            }
        };
        let ev: Event = match serde_json::from_value(env.payload) {
            Ok(e) => e,
            Err(_) => {
                // Unknown variant or missing required field. Additive schema
                // policy: keep going.
                skipped += 1;
                continue;
            }
        };
        out.push(TraceEvent {
            ts_ms: env.ts_ms,
            event: ev,
        });
    }
    if skipped > 0 {
        eprintln!(
            "(trace {}: {} unparseable lines skipped)",
            path.display(),
            skipped
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, body: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("micro-mind-trace-{n}-{name}.jsonl"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn parses_well_formed_trace() {
        let body = r#"{"ts_ms": 1, "payload": {"event": "chat_request", "turn": 0, "n_messages": 2, "n_tools": 7}}
{"ts_ms": 2, "payload": {"event": "stop", "turn": 0, "reason": "FinalAnswer", "wall_ms": 100}}
"#;
        let p = write_tmp("ok", body);
        let evs = parse_jsonl_file(&p).unwrap();
        assert_eq!(evs.len(), 2);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn skips_unknown_variant() {
        let body = r#"{"ts_ms": 1, "payload": {"event": "future_event", "x": 1}}
{"ts_ms": 2, "payload": {"event": "stop", "turn": 0, "reason": "FinalAnswer", "wall_ms": 1}}
"#;
        let p = write_tmp("unk", body);
        let evs = parse_jsonl_file(&p).unwrap();
        assert_eq!(evs.len(), 1, "future_event should have been skipped");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn tolerates_blank_and_garbage_lines() {
        let body = "\n\nnot json\n{\"ts_ms\": 1, \"payload\": {\"event\": \"stop\", \"turn\": 0, \"reason\": \"X\", \"wall_ms\": 1}}\n";
        let p = write_tmp("garbage", body);
        let evs = parse_jsonl_file(&p).unwrap();
        assert_eq!(evs.len(), 1);
        let _ = std::fs::remove_file(p);
    }
}
