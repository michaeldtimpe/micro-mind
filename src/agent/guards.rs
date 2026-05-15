//! Runtime guards layered on top of the agent loop.
//!
//! - `SemanticDedup`: catches small models that mutate whitespace/path
//!   formatting to evade literal dedup.
//! - `ReadTracker`: enforces read-before-write so edits aren't made blind.
//! - `WritePressure`: bails the loop after a successful write + N zero-byte
//!   non-write calls (a typical "I'm done but still calling tools" pattern).

use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};

use crate::config;
use crate::tools::fs_utils::canonicalize_path;

/// Semantic-dedup: hashes a normalized form of (tool_name, arguments) and
/// fires after `consecutive_limit` consecutive matches.
pub struct SemanticDedup {
    history: VecDeque<String>,
    consecutive_limit: usize,
    window: usize,
}

impl Default for SemanticDedup {
    fn default() -> Self {
        Self::new(config::DEDUP_CONSECUTIVE_LIMIT)
    }
}

impl SemanticDedup {
    pub fn new(consecutive_limit: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(8),
            consecutive_limit,
            window: 8,
        }
    }

    /// Record a call and return true if it triggers the loop guard.
    /// "Loop" = the same normalized call appears `consecutive_limit` times in a row.
    pub fn record_and_check(&mut self, name: &str, args: &Value) -> bool {
        let key = normalize_call_key(name, args);
        self.history.push_back(key.clone());
        while self.history.len() > self.window {
            self.history.pop_front();
        }
        if self.history.len() < self.consecutive_limit {
            return false;
        }
        let tail: Vec<&String> = self
            .history
            .iter()
            .rev()
            .take(self.consecutive_limit)
            .collect();
        tail.iter().all(|s| **s == key)
    }

    pub fn reset(&mut self) {
        self.history.clear();
    }
}

/// Normalize a (name, args) pair for stable comparison.
/// - Trim whitespace on tool name.
/// - Sort object keys.
/// - Canonicalize any `path`-shaped string values.
/// - Trim string args.
pub fn normalize_call_key(name: &str, args: &Value) -> String {
    let n = name.trim();
    let normalized_args = normalize_value(args);
    format!("{n}|{}", canonical_json(&normalized_args))
}

fn normalize_value(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut sorted: Vec<(String, Value)> = m
                .iter()
                .map(|(k, v)| (k.clone(), normalize_value_for_field(k, v)))
                .collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let map: serde_json::Map<String, Value> = sorted.into_iter().collect();
            Value::Object(map)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_value).collect()),
        Value::String(s) => Value::String(s.trim().to_string()),
        other => other.clone(),
    }
}

fn normalize_value_for_field(key: &str, v: &Value) -> Value {
    let normalized = normalize_value(v);
    if matches!(key, "path" | "file" | "directory" | "dir") {
        if let Value::String(s) = &normalized {
            return Value::String(canonicalize_path(s));
        }
    }
    normalized
}

fn canonical_json(v: &Value) -> String {
    // serde_json's Map preserves insertion order; since `normalize_value` sorts
    // it, plain to_string is canonical here.
    serde_json::to_string(v).unwrap_or_default()
}

/// Tracks which paths have been read in the current turn, for `read_before_write`.
#[derive(Default)]
pub struct ReadTracker {
    seen: HashSet<String>,
}

impl ReadTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful read. We use the canonicalized path so the model
    /// can't dodge the gate by varying `./` / `//` etc.
    pub fn record_read(&mut self, name: &str, args: &Value) {
        if !matches!(
            name,
            "read_file" | "list_dir" | "list_files_recursive" | "grep"
        ) {
            return;
        }
        if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
            self.seen.insert(canonicalize_path(p));
        }
        // grep without an explicit path defaults to ".".
        if name == "grep" && !args.get("path").is_some() {
            self.seen.insert(".".to_string());
        }
        // grep / list_dir on a directory covers files reached by future writes
        // only loosely. The conservative call is to require an explicit read
        // of the target file before a write — but `list_files_recursive` of
        // the project root is a reasonable signal that the model has surveyed
        // the layout, so we also mark "." as seen in that case.
        if name == "list_files_recursive"
            && (args.get("path").and_then(|v| v.as_str()).unwrap_or(".") == ".")
        {
            self.seen.insert(".".to_string());
        }
    }

    /// Has the path (or its parent directory) been read in this turn?
    /// "Parent dir was listed" counts as a soft confirmation of layout.
    pub fn has_seen(&self, path: &str) -> bool {
        let canon = canonicalize_path(path);
        if self.seen.contains(&canon) {
            return true;
        }
        // Walk up the parent chain and check.
        let mut parent: &str = &canon;
        while let Some(idx) = parent.rfind('/') {
            parent = &parent[..idx];
            if self.seen.contains(parent) {
                return true;
            }
            if parent.is_empty() {
                break;
            }
        }
        // Always allow if root "." was scanned (the model has surveyed the repo).
        self.seen.contains(".")
    }

    pub fn reset(&mut self) {
        self.seen.clear();
    }
}

/// Tracks the write-pressure signal: after a successful write, every
/// zero-byte non-write tool call counts toward an early exit threshold.
#[derive(Default)]
pub struct WritePressure {
    pub writes: usize,
    pub zero_byte_streak: usize,
}

impl WritePressure {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the counters for a single tool result. Returns true if the
    /// "we're done, model is just spinning" exit should fire.
    pub fn observe(&mut self, name: &str, ok: bool, bytes_out: usize) -> bool {
        let is_write = matches!(name, "write_file" | "edit_file");
        if is_write && ok {
            self.writes += 1;
            self.zero_byte_streak = 0;
            return false;
        }
        if self.writes > 0 && bytes_out == 0 {
            self.zero_byte_streak += 1;
            return self.zero_byte_streak >= config::WRITE_PRESSURE_ZERO_BYTE_LIMIT;
        }
        self.zero_byte_streak = 0;
        false
    }

    pub fn reset(&mut self) {
        self.writes = 0;
        self.zero_byte_streak = 0;
    }
}

/// Emit a synthetic system note that the model should see when the dedup
/// guard fires.
pub fn dedup_system_note() -> String {
    "You just repeated the same tool call several times in a row. Reconsider — \
     try different arguments, a different tool, or stop and answer the user."
        .to_string()
}

/// Emit a synthetic tool-failure note that the model should see when a write
/// is attempted before reading the target.
pub fn read_before_write_note(path: &str) -> String {
    format!(
        "Refused: read {} before modifying it. Call read_file (or list_dir / grep) on it first.",
        path
    )
}

/// Build the tool args representation we store in dedup keys. Useful in tests.
pub fn _normalize_for_tests(name: &str, args: &Value) -> String {
    normalize_call_key(name, args)
}

#[allow(dead_code)]
fn _silence_json(_: Value) {
    // Linker-shaped silence for unused import warnings during scaffolding.
}

#[allow(dead_code)]
fn _silence_json_macro() {
    let _ = json!({});
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dedup_catches_consecutive_same() {
        let mut d = SemanticDedup::new(3);
        let args = json!({"path": "src/main.rs"});
        assert!(!d.record_and_check("read_file", &args));
        assert!(!d.record_and_check("read_file", &args));
        assert!(d.record_and_check("read_file", &args));
    }

    #[test]
    fn dedup_ignores_non_consecutive() {
        let mut d = SemanticDedup::new(3);
        let a = json!({"path": "a"});
        let b = json!({"path": "b"});
        d.record_and_check("read_file", &a);
        d.record_and_check("read_file", &b);
        assert!(!d.record_and_check("read_file", &a));
    }

    #[test]
    fn dedup_normalizes_whitespace_paths() {
        let mut d = SemanticDedup::new(3);
        let a = json!({"path": "src/main.rs"});
        let b = json!({"path": "./src/main.rs"});
        let c = json!({"path": "src//main.rs"});
        assert!(!d.record_and_check("read_file", &a));
        assert!(!d.record_and_check("read_file", &b));
        assert!(d.record_and_check("read_file", &c));
    }

    #[test]
    fn dedup_normalizes_key_order() {
        let mut d = SemanticDedup::new(3);
        let a = json!({"pattern": "TODO", "path": "src"});
        let b = json!({"path": "src", "pattern": "TODO"});
        assert!(!d.record_and_check("grep", &a));
        assert!(!d.record_and_check("grep", &b));
        assert!(d.record_and_check("grep", &a));
    }

    #[test]
    fn read_tracker_records_and_checks() {
        let mut rt = ReadTracker::new();
        rt.record_read("read_file", &json!({"path": "src/main.rs"}));
        assert!(rt.has_seen("src/main.rs"));
        assert!(rt.has_seen("./src/main.rs"));
        assert!(!rt.has_seen("src/other.rs"));
    }

    #[test]
    fn read_tracker_directory_covers_file() {
        let mut rt = ReadTracker::new();
        rt.record_read("list_dir", &json!({"path": "src"}));
        assert!(rt.has_seen("src/main.rs"));
    }

    #[test]
    fn read_tracker_root_scan_covers_all() {
        let mut rt = ReadTracker::new();
        rt.record_read("list_files_recursive", &json!({"path": "."}));
        assert!(rt.has_seen("anything/at/all.rs"));
    }

    #[test]
    fn write_pressure_fires_after_three_zero_byte_idles() {
        let mut wp = WritePressure::new();
        assert!(!wp.observe("write_file", true, 32));
        assert!(!wp.observe("read_file", true, 0));
        assert!(!wp.observe("read_file", true, 0));
        assert!(wp.observe("read_file", true, 0));
    }

    #[test]
    fn write_pressure_resets_on_byte_output() {
        let mut wp = WritePressure::new();
        wp.observe("write_file", true, 32);
        wp.observe("read_file", true, 0);
        wp.observe("read_file", true, 0);
        // A non-empty result resets the streak.
        wp.observe("read_file", true, 100);
        assert!(!wp.observe("read_file", true, 0));
    }
}
