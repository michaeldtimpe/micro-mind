//! Aggregate a JSONL trace into a per-task `Summary` and check it against
//! a `Fixture`'s expectations.
//!
//! Pure data — no I/O. The bin wrappers handle file paths.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::bench::fixture::Fixture;
use crate::bench::trace::TraceEvent;
use crate::obs::Event;

/// One task's outcome — what we'd render in a markdown table row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub id: String,
    pub trace_path: Option<String>,
    pub passed: bool,
    pub failures: Vec<String>,
    pub stats: Summary,
}

/// Cheap stats over one trace. All counts are summed, latencies are scalars.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    pub tool_calls: u32,
    pub tool_calls_by_name: BTreeMap<String, u32>,
    pub cached_tool_calls: u32,
    pub tool_errors: u32,
    pub guard_fires: u32,
    pub guards_by_kind: BTreeMap<String, u32>,
    pub native_tool_calls: u64,
    pub recovered_tool_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub chat_round_trips: u32,
    pub chat_errors: u32,
    pub stop_reason: Option<String>,
    pub wall_ms: u64,
    /// Final assistant content as seen on the most recent `tool_call`-free
    /// chat turn — used by `must_contain`. Filled by the runner, not from
    /// the trace alone.
    #[serde(default)]
    pub final_answer: Option<String>,
}

/// Compute a Summary from a list of trace events. Pure.
pub fn summarize_trace(events: &[TraceEvent]) -> Summary {
    let mut s = Summary::default();
    for te in events {
        match &te.event {
            Event::ChatRequest { .. } => {}
            Event::ChatResponse {
                native_tool_calls,
                recovered_tool_calls,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                error,
                ..
            } => {
                s.chat_round_trips += 1;
                if error.is_some() {
                    s.chat_errors += 1;
                }
                s.native_tool_calls += *native_tool_calls as u64;
                s.recovered_tool_calls += *recovered_tool_calls as u64;
                if let Some(v) = prompt_tokens {
                    s.prompt_tokens += *v as u64;
                }
                if let Some(v) = completion_tokens {
                    s.completion_tokens += *v as u64;
                }
                if let Some(v) = total_tokens {
                    s.total_tokens += *v as u64;
                }
            }
            Event::ToolCall { .. } => {}
            Event::ToolResult {
                name, ok, cached, ..
            } => {
                s.tool_calls += 1;
                *s.tool_calls_by_name.entry(name.clone()).or_insert(0) += 1;
                if *cached {
                    s.cached_tool_calls += 1;
                }
                if !*ok {
                    s.tool_errors += 1;
                }
            }
            Event::Guard { kind, .. } => {
                s.guard_fires += 1;
                *s.guards_by_kind.entry(kind.clone()).or_insert(0) += 1;
            }
            Event::Stop {
                reason,
                wall_ms,
                final_answer,
                ..
            } => {
                s.stop_reason = Some(reason.clone());
                s.wall_ms = *wall_ms;
                // Schema v2: traces carry the final assistant content. When
                // present, this is authoritative — overrides any value the
                // bench-run subprocess capture may have written.
                if let Some(fa) = final_answer {
                    s.final_answer = Some(fa.clone());
                }
            }
            Event::SessionStart { .. } => {}
        }
    }
    s
}

/// Check a Summary against a Fixture's expectations. Returns the list of
/// failure reasons; empty = pass.
pub fn check_expectations(fx: &Fixture, s: &Summary) -> Vec<String> {
    let mut fails = Vec::new();

    if let Some(want) = &fx.expect.stop_reason {
        match &s.stop_reason {
            None => fails.push(format!("stop_reason missing (wanted {want})")),
            Some(got) => {
                let matches = if want == "Error" {
                    got.starts_with("Error")
                } else {
                    got == want
                };
                if !matches {
                    fails.push(format!("stop_reason: got {got}, wanted {want}"));
                }
            }
        }
    }
    if let Some(min) = fx.expect.min_tool_calls
        && s.tool_calls < min
    {
        fails.push(format!("tool_calls={} < min={min}", s.tool_calls));
    }
    if let Some(max) = fx.expect.max_tool_calls
        && s.tool_calls > max
    {
        fails.push(format!("tool_calls={} > max={max}", s.tool_calls));
    }
    if !fx.expect.must_call_any_of.is_empty() {
        let any = fx
            .expect
            .must_call_any_of
            .iter()
            .any(|n| s.tool_calls_by_name.contains_key(n));
        if !any {
            fails.push(format!(
                "no call to any of: {} (saw: {})",
                fx.expect.must_call_any_of.join(","),
                s.tool_calls_by_name
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
    }
    for bad in &fx.expect.must_not_call {
        if s.tool_calls_by_name.contains_key(bad) {
            fails.push(format!("forbidden tool call: {bad}"));
        }
    }
    if let Some(max) = fx.expect.max_wall_ms
        && s.wall_ms > max
    {
        fails.push(format!("wall_ms={} > max={max}", s.wall_ms));
    }
    if let Some(max) = fx.expect.max_total_tokens
        && s.total_tokens > max
    {
        fails.push(format!("total_tokens={} > max={max}", s.total_tokens));
    }
    // NOTE: must_contain reads `final_answer`, which is filled either by
    // `bench-run` (from subprocess stdout) OR by the schema-v2 Stop event
    // (preferred when present). Pre-v2 traces have `None` here and will
    // fail-closed on this predicate unless bench-run's capture is also wired.
    if let Some(mc) = &fx.expect.must_contain {
        match &s.final_answer {
            None => fails.push(format!(
                "must_contain: no final answer captured (wanted {:?})",
                mc.text
            )),
            Some(answer) => {
                let hit = if mc.case_insensitive {
                    answer.to_lowercase().contains(&mc.text.to_lowercase())
                } else {
                    answer.contains(&mc.text)
                };
                if !hit {
                    fails.push(format!(
                        "must_contain: {:?} not found in final answer",
                        mc.text
                    ));
                }
            }
        }
    }

    fails
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::trace::TraceEvent;
    use crate::obs::Event;

    fn te(event: Event) -> TraceEvent {
        TraceEvent { ts_ms: 0, event }
    }

    #[test]
    fn summary_counts_tool_calls_and_tokens() {
        let evs = vec![
            te(Event::ChatResponse {
                turn: 0,
                wall_ms: 50,
                finish_reason: Some("tool_calls".into()),
                native_tool_calls: 1,
                recovered_tool_calls: 0,
                prompt_tokens: Some(100),
                completion_tokens: Some(20),
                total_tokens: Some(120),
                error: None,
            }),
            te(Event::ToolResult {
                turn: 0,
                name: "read_file".into(),
                tool_call_id: "x".into(),
                ok: true,
                wall_ms: 5,
                bytes_out: 200,
                cached: false,
                error: None,
            }),
            te(Event::ToolResult {
                turn: 0,
                name: "read_file".into(),
                tool_call_id: "y".into(),
                ok: true,
                wall_ms: 1,
                bytes_out: 100,
                cached: true,
                error: None,
            }),
            te(Event::Stop {
                turn: 1,
                reason: "FinalAnswer".into(),
                wall_ms: 250,
                final_answer: None,
            }),
        ];
        let s = summarize_trace(&evs);
        assert_eq!(s.tool_calls, 2);
        assert_eq!(s.cached_tool_calls, 1);
        assert_eq!(s.total_tokens, 120);
        assert_eq!(s.tool_calls_by_name.get("read_file"), Some(&2));
        assert_eq!(s.wall_ms, 250);
        assert_eq!(s.stop_reason.as_deref(), Some("FinalAnswer"));
    }

    #[test]
    fn check_pass_when_no_constraints() {
        let fx = Fixture {
            id: "x".into(),
            description: "".into(),
            prompt: "".into(),
            expect: Default::default(),
        };
        let s = Summary::default();
        assert!(check_expectations(&fx, &s).is_empty());
    }

    #[test]
    fn check_fails_on_min_tool_calls() {
        let fx_src = r#"
            id = "t"
            prompt = "p"
            [expect]
            min_tool_calls = 2
        "#;
        let fx = Fixture::from_toml_str(fx_src).unwrap();
        let s = Summary {
            tool_calls: 1,
            ..Default::default()
        };
        let fails = check_expectations(&fx, &s);
        assert_eq!(fails.len(), 1);
        assert!(fails[0].contains("min=2"));
    }

    #[test]
    fn check_fails_on_must_not_call() {
        let fx_src = r#"
            id = "t"
            prompt = "p"
            [expect]
            must_not_call = ["bash"]
        "#;
        let fx = Fixture::from_toml_str(fx_src).unwrap();
        let mut s = Summary::default();
        s.tool_calls_by_name.insert("bash".into(), 1);
        let fails = check_expectations(&fx, &s);
        assert!(fails.iter().any(|f| f.contains("bash")));
    }

    #[test]
    fn check_must_contain_case_insensitive() {
        let fx_src = r#"
            id = "t"
            prompt = "p"
            [expect.must_contain]
            text = "FORTY-TWO"
        "#;
        let fx = Fixture::from_toml_str(fx_src).unwrap();
        let s = Summary {
            final_answer: Some("the answer is forty-two indeed".into()),
            ..Default::default()
        };
        assert!(check_expectations(&fx, &s).is_empty());
    }

    #[test]
    fn check_stop_reason_error_prefix_match() {
        let fx_src = r#"
            id = "t"
            prompt = "p"
            [expect]
            stop_reason = "Error"
        "#;
        let fx = Fixture::from_toml_str(fx_src).unwrap();
        let s = Summary {
            stop_reason: Some("Error: timed out".into()),
            ..Default::default()
        };
        assert!(check_expectations(&fx, &s).is_empty());
    }
}
