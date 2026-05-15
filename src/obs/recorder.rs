//! `Recorder` trait + no-op and JSONL implementations.
//!
//! See `obs/schema.md` for the event schema.

use serde::Serialize;
use serde_json::Value;
use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// All recordable event payloads.
///
/// Field naming follows the OpenAI / llama-server vocabulary where possible
/// (`prompt_tokens`, `completion_tokens`, `finish_reason`). Internal-only
/// fields use snake_case.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Outgoing chat request — captured before the HTTP POST.
    ChatRequest {
        turn: u32,
        n_messages: usize,
        n_tools: usize,
    },
    /// Chat response from llama-server.
    ChatResponse {
        turn: u32,
        wall_ms: u64,
        finish_reason: Option<String>,
        /// Number of native tool_calls returned by the server.
        native_tool_calls: usize,
        /// Number of tool_calls recovered from prose (text-channel fallback).
        recovered_tool_calls: usize,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        total_tokens: Option<u32>,
        error: Option<String>,
    },
    /// A tool dispatch is about to start.
    ToolCall {
        turn: u32,
        name: String,
        arguments: Value,
        tool_call_id: String,
    },
    /// A tool dispatch finished (or hit the guard layer).
    ToolResult {
        turn: u32,
        name: String,
        tool_call_id: String,
        ok: bool,
        wall_ms: u64,
        bytes_out: usize,
        cached: bool,
        error: Option<String>,
    },
    /// A harness guard fired (dedup, read-before-write, write-pressure, turn-cap).
    Guard {
        turn: u32,
        kind: String,
        detail: Option<String>,
    },
    /// The run_turn loop terminated.
    Stop {
        turn: u32,
        reason: String,
        wall_ms: u64,
    },
    /// A new session was opened (recording started).
    SessionStart {
        cwd: String,
        model: String,
        tools: Vec<String>,
    },
}

/// Public recorder interface. `record(...)` must be cheap and infallible
/// from the caller's perspective.
pub trait Recorder: Send + Sync {
    fn record(&self, event: Event);
}

/// Shared handle passed through `Session`. Always `Send + Sync + Clone`.
pub type RecorderHandle = Arc<dyn Recorder>;

/// Default: discards every event.
pub struct NoopRecorder;

impl Recorder for NoopRecorder {
    fn record(&self, _event: Event) {}
}

/// Append-only JSONL recorder. One file per process.
///
/// Failures during write are reported to stderr exactly once, then silenced
/// — recording is best-effort and must never break a run.
pub struct JsonlRecorder {
    inner: Mutex<JsonlInner>,
    pub path: PathBuf,
}

struct JsonlInner {
    writer: Option<BufWriter<File>>,
    warned: bool,
}

impl JsonlRecorder {
    /// Open `<dir>/micro-mind-<unix_ms>.jsonl` for append. Creates the dir.
    pub fn open_in_dir(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = dir.as_ref();
        create_dir_all(dir)?;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let path = dir.join(format!("micro-mind-{ts}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            inner: Mutex::new(JsonlInner {
                writer: Some(BufWriter::new(file)),
                warned: false,
            }),
            path,
        })
    }

    /// Open at an exact path (used by tests).
    pub fn open_path(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            inner: Mutex::new(JsonlInner {
                writer: Some(BufWriter::new(file)),
                warned: false,
            }),
            path,
        })
    }
}

impl Recorder for JsonlRecorder {
    fn record(&self, event: Event) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let envelope = serde_json::json!({
            "ts_ms": ts_ms,
            "payload": event,
        });
        let mut line = match serde_json::to_string(&envelope) {
            Ok(s) => s,
            Err(_) => return,
        };
        line.push('\n');

        let Ok(mut guard) = self.inner.lock() else { return };
        let Some(writer) = guard.writer.as_mut() else { return };
        if let Err(e) = writer.write_all(line.as_bytes()).and_then(|_| writer.flush()) {
            if !guard.warned {
                eprintln!("(recorder: write failed, further events dropped: {e})");
                guard.warned = true;
            }
            guard.writer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::read_to_string;

    fn tmpfile(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("micro-mind-rec-{ts}-{name}.jsonl"));
        p
    }

    #[test]
    fn noop_recorder_is_silent() {
        let r = NoopRecorder;
        // Just exercises the trait — no panics, no I/O.
        r.record(Event::Guard {
            turn: 0,
            kind: "dedup".into(),
            detail: None,
        });
    }

    #[test]
    fn jsonl_recorder_writes_one_line_per_event() {
        let path = tmpfile("basic");
        let r = JsonlRecorder::open_path(&path).expect("open");
        r.record(Event::ChatRequest { turn: 1, n_messages: 3, n_tools: 7 });
        r.record(Event::ToolResult {
            turn: 1,
            name: "read_file".into(),
            tool_call_id: "abc".into(),
            ok: true,
            wall_ms: 5,
            bytes_out: 42,
            cached: false,
            error: None,
        });
        drop(r); // flush
        let body = read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v0["payload"]["event"], "chat_request");
        assert_eq!(v0["payload"]["n_messages"], 3);
        let v1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v1["payload"]["event"], "tool_result");
        assert_eq!(v1["payload"]["ok"], true);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn jsonl_recorder_envelope_has_ts_ms() {
        let path = tmpfile("ts");
        let r = JsonlRecorder::open_path(&path).expect("open");
        r.record(Event::Stop { turn: 2, reason: "FinalAnswer".into(), wall_ms: 100 });
        drop(r);
        let body = read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert!(v["ts_ms"].as_u64().is_some());
        let _ = std::fs::remove_file(&path);
    }
}
