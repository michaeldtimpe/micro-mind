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
    /// Pass if at least one call to any of these tools is observed.
    #[serde(default)]
    pub must_call_any_of: Vec<String>,
    /// Fail if any of these tools is observed.
    #[serde(default)]
    pub must_not_call: Vec<String>,
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
            max_wall_ms = 1000
            max_total_tokens = 2000
        "#;
        let fx = Fixture::from_toml_str(src).unwrap();
        assert_eq!(fx.expect.min_tool_calls, Some(1));
        assert_eq!(fx.expect.must_call_any_of, vec!["read_file"]);
        assert_eq!(fx.expect.must_not_call, vec!["bash"]);
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
