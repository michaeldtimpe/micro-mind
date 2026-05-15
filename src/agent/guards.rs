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
    if matches!(key, "path" | "file" | "directory" | "dir")
        && let Value::String(s) = &normalized
    {
        return Value::String(canonicalize_path(s));
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
        if name == "grep" && args.get("path").is_none() {
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
}

/// Emit a synthetic system note that the model should see when the dedup
/// guard fires.
pub fn dedup_system_note() -> String {
    "You just repeated the same tool call several times in a row. Reconsider — \
     try different arguments, a different tool, or stop and answer the user."
        .to_string()
}

/// Emit a synthetic tool-failure note that the model should see when a
/// modify-existing call is attempted before reading the target.
pub fn read_before_write_note(path: &str) -> String {
    format!(
        "Refused: read {} before modifying it. Call read_file (or list_dir / grep) on it first.",
        path
    )
}

/// Variant of the read-before-write note tailored for `write_file`, which
/// is often used to create *new* files. The recovery path is "survey the
/// directory first", not "read the file first" — the latter confuses small
/// models, which interpret it as "the file doesn't exist" and give up.
pub fn read_before_write_note_for_write(path: &str) -> String {
    format!(
        "Refused: cannot write {} without surveying the directory first. \
         Call list_dir on the target directory (or list_files_recursive on \".\") \
         to confirm what's there, then retry write_file.",
        path
    )
}

/// Synthetic system note for when the previous response was truncated by
/// max_tokens. Persists across `run_turn` calls so the model sees it on
/// the next user input.
pub fn length_truncation_note() -> String {
    "Your previous response was cut off at the max_tokens limit. Be more concise: \
     answer directly without restating the question, and prefer one tool call at a time."
        .to_string()
}

/// First-turn cold-read guard. Returns Some(refusal_note) if the model is
/// calling `read_file` on turn 0 with a path the user did not mention in
/// their input. Mitigates the BFCL "over-call on irrelevance" failure mode
/// where small models reach for tools on self-answerable questions and
/// invent stub paths like `/dev/null` to satisfy the tool channel.
///
/// Scope:
/// - Turn 0 only. By turn 1+ the model has tool results in context and is
///   reasoning more naturally; gating later turns would be disruptive.
/// - Only `read_file`. Grep with a generic search path (`.`, `src`) is a
///   legitimate exploration pattern and shouldn't be gated.
/// - Skip when path is empty or `.` (project survey).
///
/// Matching is case-insensitive and substring-based against both the
/// canonicalized path and its basename. False positives (model has a
/// legitimate reason to read an unmentioned file) result in a recoverable
/// refusal — the model gets the note and can retry with a path the user
/// referenced, or answer directly.
pub fn first_turn_cold_read_check(
    turn: u32,
    user_input: &str,
    tool_name: &str,
    args: &Value,
) -> Option<String> {
    if turn != 0 {
        return None;
    }
    if tool_name != "read_file" {
        return None;
    }
    let path = args.get("path").and_then(|v| v.as_str())?;
    let canon = canonicalize_path(path);
    if canon.is_empty() || canon == "." {
        return None;
    }
    let basename = canon.rsplit('/').next().unwrap_or(&canon);
    let lower = user_input.to_lowercase();
    let canon_lower = canon.to_lowercase();
    let basename_lower = basename.to_lowercase();
    if lower.contains(&canon_lower) || lower.contains(&basename_lower) {
        return None;
    }
    Some(format!(
        "Refused: the user did not reference '{}' in their request. \
         On the first turn, only call read_file on paths the user mentioned. \
         Answer the user directly without reading.",
        path
    ))
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
    fn cold_read_fires_on_unmentioned_path_turn_zero() {
        // The /dev/null shape from 03-decline-irrelevant: model invents a
        // stub path on a math question.
        let r = first_turn_cold_read_check(
            0,
            "What is 17 + 25?",
            "read_file",
            &json!({"path": "/dev/null"}),
        );
        assert!(r.is_some(), "expected refusal, got {:?}", r);
    }

    #[test]
    fn cold_read_allows_path_mentioned_in_prompt() {
        // 01-read-readme shape: user mentions README.md, model reads it.
        let r = first_turn_cold_read_check(
            0,
            "Read README.md and tell me in one sentence what micro-mind is.",
            "read_file",
            &json!({"path": "/Users/x/proj/README.md"}),
        );
        assert!(r.is_none(), "expected pass-through, got {:?}", r);
    }

    #[test]
    fn cold_read_allows_basename_match_case_insensitive() {
        // Match must be case-insensitive.
        let r = first_turn_cold_read_check(
            0,
            "tell me about cargo.toml",
            "read_file",
            &json!({"path": "Cargo.toml"}),
        );
        assert!(r.is_none());
    }

    #[test]
    fn cold_read_only_applies_to_turn_zero() {
        // Turn 1+ should pass through — the model has tool results in context.
        let r = first_turn_cold_read_check(
            1,
            "What is 17 + 25?",
            "read_file",
            &json!({"path": "/dev/null"}),
        );
        assert!(r.is_none());
    }

    #[test]
    fn cold_read_only_applies_to_read_file() {
        // grep with a generic search path is legitimate exploration.
        let r = first_turn_cold_read_check(
            0,
            "find all TODOs",
            "grep",
            &json!({"path": "src", "pattern": "TODO"}),
        );
        assert!(r.is_none());
    }

    #[test]
    fn cold_read_allows_dot_path() {
        // Project survey is always fine.
        let r = first_turn_cold_read_check(
            0,
            "look around",
            "read_file",
            &json!({"path": "."}),
        );
        assert!(r.is_none());
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
