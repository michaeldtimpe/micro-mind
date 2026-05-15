//! Core agent loop. Wires everything in `agent/` together.

pub mod coach;
pub mod compress;
pub mod context;
pub mod guards;

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::config;
use crate::llm::client::LlmClient;
use crate::llm::types::{ChatMessage, Usage};
use crate::repl::render;
use crate::tools::cache::ToolCache;
use crate::tools::{ToolCallResult, ToolDef, dispatch};
use micro_mind::obs::{Event, RecorderHandle};

/// Reason the loop terminated, for /explain.
#[derive(Debug, Clone)]
pub enum StopReason {
    FinalAnswer,
    TurnCap,
    WritePressure,
    Dedup,
    /// The model's reply was truncated by max_tokens (finish_reason="length").
    /// Distinct from TurnCap (which is the harness's loop limit).
    Length,
    Error(String),
}

impl StopReason {
    fn label(&self) -> String {
        match self {
            StopReason::FinalAnswer => "FinalAnswer".into(),
            StopReason::TurnCap => "TurnCap".into(),
            StopReason::WritePressure => "WritePressure".into(),
            StopReason::Dedup => "Dedup".into(),
            StopReason::Length => "Length".into(),
            StopReason::Error(e) => format!("Error: {e}"),
        }
    }
}

/// Per-conversation state. Persists across `run_turn` calls until /reset.
pub struct Session {
    pub client: LlmClient,
    pub tools: Vec<ToolDef>,
    pub tools_by_name: HashMap<String, ToolDef>,
    pub messages: Vec<ChatMessage>,
    pub cache: ToolCache,
    pub last_calls: Vec<ToolCallResult>,
    pub last_stop: Option<StopReason>,
    pub last_usage: Option<Usage>,
    pub cwd: PathBuf,
    pub recorder: RecorderHandle,
}

impl Session {
    pub fn new(
        client: LlmClient,
        tools: Vec<ToolDef>,
        cwd: PathBuf,
        system_prompt: String,
        recorder: RecorderHandle,
    ) -> Self {
        let tools_by_name = tools.iter().map(|t| (t.name.clone(), t.clone())).collect();
        Self {
            client,
            tools,
            tools_by_name,
            messages: vec![ChatMessage::system(system_prompt)],
            cache: ToolCache::new(),
            last_calls: Vec::new(),
            last_stop: None,
            last_usage: None,
            cwd,
            recorder,
        }
    }

    /// Reset everything except the system prompt — used by /reset.
    pub fn reset(&mut self) {
        let system = self.messages.first().cloned();
        self.messages.clear();
        if let Some(s) = system {
            self.messages.push(s);
        }
        self.cache = ToolCache::new();
        self.last_calls.clear();
        self.last_stop = None;
        self.last_usage = None;
    }

    pub fn pressure(&self) -> f32 {
        context::pressure(&self.messages, config::N_CTX)
    }
}

/// Process one user message: chat → tool dispatch → loop → final answer or guard.
pub fn run_turn(state: &mut Session, user_input: &str) -> Result<()> {
    state.messages.push(ChatMessage::user(user_input));
    state.last_calls.clear();

    let mut dedup = guards::SemanticDedup::default();
    let mut reads = guards::ReadTracker::new();
    let mut write_pressure = guards::WritePressure::new();
    let mut turns = 0u32;
    let turn_start = Instant::now();
    // Track the last non-empty assistant content so the Stop event can
    // surface it (schema v2). For non-FinalAnswer terminations this is
    // best-effort — the most recent assistant prose, whatever it was.
    let mut last_assistant_content: Option<String> = None;

    loop {
        if turns as usize >= config::MAX_TURNS {
            state.last_stop = Some(StopReason::TurnCap);
            state.recorder.record(Event::Guard {
                turn: turns,
                kind: "turn_cap".into(),
                detail: None,
            });
            render::guard("turn cap");
            break;
        }

        // Soft elision when pressure climbs.
        if state.pressure() > config::PRESSURE_THRESHOLD {
            state.messages = context::maybe_elide(&state.messages);
        }

        state.recorder.record(Event::ChatRequest {
            turn: turns,
            n_messages: state.messages.len(),
            n_tools: state.tools.len(),
        });
        let chat_started = Instant::now();
        let outcome = match state.client.chat(&state.messages, &state.tools) {
            Ok(o) => o,
            Err(e) => {
                let err_str = e.to_string();
                state.recorder.record(Event::ChatResponse {
                    turn: turns,
                    wall_ms: chat_started.elapsed().as_millis() as u64,
                    finish_reason: None,
                    native_tool_calls: 0,
                    recovered_tool_calls: 0,
                    prompt_tokens: None,
                    completion_tokens: None,
                    total_tokens: None,
                    error: Some(err_str.clone()),
                });
                state.last_stop = Some(StopReason::Error(err_str));
                render::error(&format!("chat request failed: {e}"));
                break;
            }
        };

        let resp = outcome.message;
        let chat_ms = chat_started.elapsed().as_millis() as u64;

        state.recorder.record(Event::ChatResponse {
            turn: turns,
            wall_ms: chat_ms,
            finish_reason: outcome.finish_reason.clone(),
            native_tool_calls: outcome.native_tool_calls,
            recovered_tool_calls: outcome.recovered_tool_calls,
            prompt_tokens: outcome.usage.as_ref().map(|u| u.prompt_tokens),
            completion_tokens: outcome.usage.as_ref().map(|u| u.completion_tokens),
            total_tokens: outcome.usage.as_ref().map(|u| u.total_tokens),
            error: None,
        });
        if outcome.usage.is_some() {
            state.last_usage = outcome.usage;
        }

        // Push the assistant turn verbatim — including any recovered tool_calls.
        state.messages.push(resp.clone());
        if let Some(content) = &resp.content
            && !content.trim().is_empty()
        {
            last_assistant_content = Some(content.clone());
        }

        // Length-truncation guard: if max_tokens cut off the response, don't
        // dispatch any tool_calls (they may be incomplete) and don't loop.
        // Push a system note so the model sees the truncation hint next turn.
        if outcome.finish_reason.as_deref() == Some("length") {
            state.recorder.record(Event::Guard {
                turn: turns,
                kind: "length".into(),
                detail: None,
            });
            render::guard("response truncated (length)");
            state
                .messages
                .push(ChatMessage::system(guards::length_truncation_note()));
            state.last_stop = Some(StopReason::Length);
            break;
        }

        if resp.tool_calls.is_empty() {
            if let Some(content) = &resp.content {
                render::final_answer(content);
            }
            state.last_stop = Some(StopReason::FinalAnswer);
            break;
        }

        let mut dedup_fired = false;
        for tc in &resp.tool_calls {
            let args = tc.parse_arguments().unwrap_or(Value::Null);

            // Guard: semantic dedup.
            if dedup.record_and_check(&tc.function.name, &args) {
                let note = guards::dedup_system_note();
                state.recorder.record(Event::Guard {
                    turn: turns,
                    kind: "dedup".into(),
                    detail: Some(tc.function.name.clone()),
                });
                render::guard(&format!("dedup → {}", tc.function.name));
                state
                    .messages
                    .push(ChatMessage::tool_result(&tc.id, &tc.function.name, &note));
                dedup_fired = true;
                break;
            }

            // Guard: read-before-write. For `edit_file`, always enforce —
            // editing without a prior read is the failure mode we care about.
            // For `write_file`, only enforce when the target ALREADY exists
            // on disk; creating a brand-new file has nothing to read, and
            // 1.5 B models interpret the refusal as "the file doesn't exist"
            // and stop instead of surveying-and-retrying (lessons.md
            // 2026-05-15 "polite apology" entry).
            if matches!(tc.function.name.as_str(), "write_file" | "edit_file") {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let needs_check = !path.is_empty()
                    && !reads.has_seen(path)
                    && (tc.function.name == "edit_file"
                        || crate::tools::fs_utils::safe_path(&state.cwd, path)
                            .map(|p| p.exists())
                            .unwrap_or(false));
                if needs_check {
                    let note = if tc.function.name == "write_file" {
                        guards::read_before_write_note_for_write(path)
                    } else {
                        guards::read_before_write_note(path)
                    };
                    state.recorder.record(Event::Guard {
                        turn: turns,
                        kind: "read_before_write".into(),
                        detail: Some(path.to_string()),
                    });
                    render::guard(&format!("read-before-write → {}", path));
                    state
                        .messages
                        .push(ChatMessage::tool_result(&tc.id, &tc.function.name, &note));
                    continue;
                }
            }

            // Guard: first-turn cold-read.
            if let Some(note) =
                guards::first_turn_cold_read_check(turns, user_input, &tc.function.name, &args)
            {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                state.recorder.record(Event::Guard {
                    turn: turns,
                    kind: "cold_read".into(),
                    detail: Some(path.clone()),
                });
                render::guard(&format!("cold-read → {}", path));
                state
                    .messages
                    .push(ChatMessage::tool_result(&tc.id, &tc.function.name, &note));
                continue;
            }

            // Dispatch.
            state.recorder.record(Event::ToolCall {
                turn: turns,
                name: tc.function.name.clone(),
                arguments: args.clone(),
                tool_call_id: tc.id.clone(),
            });
            render::tool_call_start(&tc.function.name, &args);
            let mut call = dispatch(
                &tc.function.name,
                &args,
                &tc.id,
                &state.tools_by_name,
                &mut state.cache,
            );
            render::tool_call_result(&call);

            // Coach the result body before it enters the conversation.
            let coached_body = coach::coach(&call);
            call.result = coached_body.clone();
            call.bytes_out = coached_body.len();
            state.messages.push(ChatMessage::tool_result(
                &tc.id,
                &tc.function.name,
                &coached_body,
            ));

            state.recorder.record(Event::ToolResult {
                turn: turns,
                name: call.name.clone(),
                tool_call_id: call.id.clone(),
                ok: call.is_ok(),
                wall_ms: call.wall_ms,
                bytes_out: call.bytes_out,
                cached: call.cached,
                error: call.error.clone(),
            });

            // Semantic compression as a system note alongside the raw result.
            if let Some(summary) = compress::summarize(&call) {
                state
                    .messages
                    .push(ChatMessage::system(format!("Tool summary: {summary}")));
            }

            // Failure-memory injection.
            if let Some(note) = coach::failure_memory_note(&call) {
                state.messages.push(ChatMessage::system(note));
            }

            // Track reads for read-before-write.
            if call.is_ok() {
                reads.record_read(&call.name, &call.arguments);
            }

            // Write-pressure tracking.
            if write_pressure.observe(&call.name, call.is_ok(), call.bytes_out) {
                state.last_stop = Some(StopReason::WritePressure);
                state.recorder.record(Event::Guard {
                    turn: turns,
                    kind: "write_pressure".into(),
                    detail: None,
                });
                render::guard("write-pressure");
                state.last_calls.push(call);
                state.recorder.record(Event::Stop {
                    turn: turns,
                    reason: "WritePressure".into(),
                    wall_ms: turn_start.elapsed().as_millis() as u64,
                    final_answer: last_assistant_content.clone(),
                });
                return Ok(());
            }

            state.last_calls.push(call);
        }

        if dedup_fired {
            state.last_stop = Some(StopReason::Dedup);
            break;
        }

        turns += 1;
    }

    if let Some(reason) = &state.last_stop {
        state.recorder.record(Event::Stop {
            turn: turns,
            reason: reason.label(),
            wall_ms: turn_start.elapsed().as_millis() as u64,
            final_answer: last_assistant_content,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Loop-level tests against the live model belong in tests/smoke_with_model.rs;
    // the guard/cache/context primitives are unit-tested in their own modules.
}
