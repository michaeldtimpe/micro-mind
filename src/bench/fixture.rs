//! Bench fixture: one task per TOML file.
//!
//! See `bench/tasks/*.toml` for examples. The schema is intentionally
//! limited to predicates we can validate from a JSONL trace alone.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub prompt: String,
    pub expect: TaskExpect,
    /// If true, `bench-run` spawns `micro-mind` in a fresh per-rep tempdir
    /// instead of the project root. Required for fixtures that exercise
    /// mutating tools (`write_file`, `edit_file`, mutating `bash`) — without
    /// it, state from one rep leaks into the next and the project root
    /// accumulates scratch files.
    #[serde(default)]
    pub cwd_isolated: bool,
    /// Files to seed into the scratch dir before invoking `micro-mind`.
    /// Only honored when `cwd_isolated = true`. Use for `edit_file` fixtures
    /// (the file must exist before the model can read-then-edit it).
    #[serde(default)]
    pub seed_files: Vec<SeedFile>,
    /// Empty directories to create in the scratch dir before invoking
    /// `micro-mind`. Only honored when `cwd_isolated = true`. Use for
    /// fixtures that need truly-empty subdirectories — for example,
    /// baiting `WritePressure` via `list_dir` on an empty dir requires
    /// the directory to exist *and* contain nothing (which a `seed_files`
    /// entry can't produce on its own, since seeding a file always
    /// leaves that file in the parent dir). Created after `seed_files`
    /// is written, so a seeded file under one of these paths will not
    /// be overwritten.
    #[serde(default)]
    pub seed_dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedFile {
    /// Path relative to the scratch dir.
    pub path: String,
    /// File contents written verbatim.
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskExpect {
    /// Must equal one of: "FinalAnswer", "TurnCap", "WritePressure", "Dedup",
    /// or "Error" (prefix match — "Error: …" trips on this).
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub min_tool_calls: Option<u32>,
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    /// Pass if at least one call to any of these tools is observed (OR).
    #[serde(default)]
    pub must_call_any_of: Vec<String>,
    /// Pass only if every tool in this list is called at least once (AND).
    /// Use for multi-tool workflows where the test is "did the model do
    /// step A then step B?" — e.g. grep followed by read_file.
    #[serde(default)]
    pub must_call_all_of: Vec<String>,
    /// Fail if any of these tools is observed.
    #[serde(default)]
    pub must_not_call: Vec<String>,
    /// Lower bound on `tool_errors` (tool results with `ok=false`). Use to
    /// positively assert that a guard/rejection fired — e.g. `min_tool_errors
    /// = 1` for a placeholder-rejection fixture.
    #[serde(default)]
    pub min_tool_errors: Option<u32>,
    /// Upper bound on `tool_errors`. Use to bound recovery-loop behavior —
    /// e.g. `max_tool_errors = 1` says "at most one rejection; more means
    /// the model is looping on the same failure".
    #[serde(default)]
    pub max_tool_errors: Option<u32>,
    /// Pass only if every guard kind in this list fired at least once.
    /// Matches `guards_by_kind` from the trace. Use to positively assert
    /// a specific guard intervened — e.g. `must_fire_guards = ["cold_read"]`
    /// for a fixture that bait `first_turn_cold_read_check`. Kind strings
    /// match the `kind` field on `Event::Guard` (`cold_read`, `dedup`,
    /// `read_before_write`, `length`, `write_pressure`, `turn_cap`).
    #[serde(default)]
    pub must_fire_guards: Vec<String>,
    /// Fail if any of these guard kinds fired. Symmetric counterpart to
    /// `must_fire_guards`. Use to lock the absence of accidental guard
    /// fires that would indicate a different bug than the one a fixture
    /// targets (e.g. a non-fire anchor that should never trip cold_read).
    #[serde(default)]
    pub must_not_fire_guards: Vec<String>,
    /// Lower bound on total `guard_fires` across all kinds. Orthogonal to
    /// `must_fire_guards`: kind predicates answer "which?", count
    /// predicates answer "how many?".
    #[serde(default)]
    pub min_guard_fires: Option<u32>,
    /// Upper bound on total `guard_fires` across all kinds. Use to bound
    /// recovery behavior — e.g. `max_guard_fires = 1` admits a single
    /// dedup-safety-net branch but rejects runaway loops.
    #[serde(default)]
    pub max_guard_fires: Option<u32>,
    /// Pass only if every tool name in this list appeared as at least one
    /// *synthetic* (harness-injected) call. Used to lock the auto-read
    /// contract on `read_before_write` recovery — e.g.
    /// `must_have_synthetic_calls = ["read_file"]` for a fixture that
    /// exercises the option-(b) auto-read path. Sourced from
    /// `synthetic_tool_calls_by_name` (schema v3); pre-v3 traces report no
    /// synthetic calls so this predicate is fail-closed against them.
    #[serde(default)]
    pub must_have_synthetic_calls: Vec<String>,
    /// Fail if any of these tool names appeared as a synthetic call.
    /// Symmetric counterpart to `must_have_synthetic_calls`. Use
    /// defensively — e.g. `must_not_have_synthetic_calls = ["edit_file"]`
    /// to ensure no future harness change accidentally synthesizes a
    /// mutating tool call.
    #[serde(default)]
    pub must_not_have_synthetic_calls: Vec<String>,
    /// Lower bound on `model_tool_calls` (= `tool_calls` minus
    /// `synthetic_tool_calls`). Pins compositionality on fixtures whose
    /// `tool_calls` count is dominated by harness intervention — without
    /// this, a fixture asserting `must_have_synthetic_calls = ["read_file"]`
    /// would trivially pass on the no-call-FA shape (synthetic read fires,
    /// model emits no tool call, harness intervention succeeded but no
    /// recovery happened). Use to require ≥N model-emitted calls beyond
    /// what the harness contributed — e.g. `min_model_tool_calls = 1` on
    /// fixture 12 says "the model must compose at least one tool call on
    /// the recovery turn beyond the synthetic read."
    #[serde(default)]
    pub min_model_tool_calls: Option<u32>,
    /// Hard upper bound on `stop.wall_ms` from the JSONL trace.
    #[serde(default)]
    pub max_wall_ms: Option<u64>,
    /// Hard upper bound on summed `total_tokens` across all chat_response events.
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    /// Substring expected somewhere in the final assistant message content.
    #[serde(default)]
    pub must_contain: Option<MustContain>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MustContain {
    pub text: String,
    #[serde(default = "default_case_insensitive")]
    pub case_insensitive: bool,
}

fn default_case_insensitive() -> bool {
    true
}

impl Fixture {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).context("parse fixture TOML")
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read fixture {}", path.display()))?;
        let mut fx: Fixture = Self::from_toml_str(&body)?;
        if fx.id.is_empty() {
            // Default id from filename stem.
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                fx.id = stem.to_string();
            }
        }
        Ok(fx)
    }

    /// Load every `*.toml` under `dir`, sorted by id.
    pub fn discover(dir: impl AsRef<Path>) -> Result<Vec<Self>> {
        let dir = dir.as_ref();
        let mut out = Vec::new();
        if !dir.is_dir() {
            anyhow::bail!("not a directory: {}", dir.display());
        }
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("toml") {
                out.push(Fixture::from_path(&path)?);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_fixture() {
        let src = r#"
            id = "x"
            prompt = "do a thing"
            [expect]
            stop_reason = "FinalAnswer"
        "#;
        let fx = Fixture::from_toml_str(src).unwrap();
        assert_eq!(fx.id, "x");
        assert_eq!(fx.expect.stop_reason.as_deref(), Some("FinalAnswer"));
        assert!(fx.expect.must_call_any_of.is_empty());
    }

    #[test]
    fn parses_must_contain_with_default_case() {
        let src = r#"
            id = "y"
            prompt = "p"
            [expect.must_contain]
            text = "42"
        "#;
        let fx = Fixture::from_toml_str(src).unwrap();
        let mc = fx.expect.must_contain.unwrap();
        assert_eq!(mc.text, "42");
        assert!(mc.case_insensitive, "default should be true");
    }

    #[test]
    fn parses_seed_dirs() {
        let src = r#"
            id = "with_dirs"
            prompt = "p"
            cwd_isolated = true
            seed_dirs = ["a", "b/c"]
            [expect]
        "#;
        let fx = Fixture::from_toml_str(src).unwrap();
        assert!(fx.cwd_isolated);
        assert_eq!(fx.seed_dirs, vec!["a".to_string(), "b/c".to_string()]);
    }

    #[test]
    fn seed_dirs_defaults_empty_when_absent() {
        let src = r#"
            id = "x"
            prompt = "p"
            [expect]
        "#;
        let fx = Fixture::from_toml_str(src).unwrap();
        assert!(fx.seed_dirs.is_empty());
    }

    #[test]
    fn parses_full_fixture_with_all_fields() {
        let src = r#"
            id = "full"
            description = "everything"
            prompt = "p"
            [expect]
            stop_reason = "FinalAnswer"
            min_tool_calls = 1
            max_tool_calls = 5
            must_call_any_of = ["read_file"]
            must_not_call = ["bash"]
            min_tool_errors = 1
            max_tool_errors = 2
            must_fire_guards = ["cold_read"]
            must_not_fire_guards = ["read_before_write"]
            min_guard_fires = 1
            max_guard_fires = 2
            must_have_synthetic_calls = ["read_file"]
            must_not_have_synthetic_calls = ["edit_file"]
            min_model_tool_calls = 1
            max_wall_ms = 1000
            max_total_tokens = 2000
        "#;
        let fx = Fixture::from_toml_str(src).unwrap();
        assert_eq!(fx.expect.min_tool_calls, Some(1));
        assert_eq!(fx.expect.must_call_any_of, vec!["read_file"]);
        assert_eq!(fx.expect.must_not_call, vec!["bash"]);
        assert_eq!(fx.expect.min_tool_errors, Some(1));
        assert_eq!(fx.expect.max_tool_errors, Some(2));
        assert_eq!(fx.expect.must_fire_guards, vec!["cold_read"]);
        assert_eq!(fx.expect.must_not_fire_guards, vec!["read_before_write"]);
        assert_eq!(fx.expect.min_guard_fires, Some(1));
        assert_eq!(fx.expect.max_guard_fires, Some(2));
        assert_eq!(fx.expect.must_have_synthetic_calls, vec!["read_file"]);
        assert_eq!(fx.expect.must_not_have_synthetic_calls, vec!["edit_file"]);
        assert_eq!(fx.expect.min_model_tool_calls, Some(1));
    }

    #[test]
    fn synthetic_call_predicates_default_when_absent() {
        let src = r#"
            id = "x"
            prompt = "p"
            [expect]
        "#;
        let fx = Fixture::from_toml_str(src).unwrap();
        assert!(fx.expect.must_have_synthetic_calls.is_empty());
        assert!(fx.expect.must_not_have_synthetic_calls.is_empty());
    }

    #[test]
    fn guard_predicates_default_when_absent() {
        let src = r#"
            id = "x"
            prompt = "p"
            [expect]
        "#;
        let fx = Fixture::from_toml_str(src).unwrap();
        assert!(fx.expect.must_fire_guards.is_empty());
        assert!(fx.expect.must_not_fire_guards.is_empty());
        assert!(fx.expect.min_guard_fires.is_none());
        assert!(fx.expect.max_guard_fires.is_none());
    }

    #[test]
    fn missing_id_recovered_from_filename() {
        use std::io::Write;
        let mut p = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("micro-mind-bench-{stamp}.toml"));
        let mut f = std::fs::File::create(&p).unwrap();
        write!(f, "id = \"\"\nprompt = \"x\"\n[expect]\n").unwrap();
        let fx = Fixture::from_path(&p).unwrap();
        assert!(fx.id.starts_with("micro-mind-bench-"));
        let _ = std::fs::remove_file(&p);
    }
}
