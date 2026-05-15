//! Core agent loop. Wires everything in `agent/` together.

pub mod coach;
pub mod compress;
pub mod context;
pub mod guards;

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config;
use crate::llm::client::LlmClient;
use crate::llm::types::ChatMessage;
use crate::repl::render;
use crate::tools::cache::ToolCache;
use crate::tools::{ToolCallResult, ToolDef, dispatch};

/// Reason the loop terminated, for /explain.
#[derive(Debug, Clone)]
pub enum StopReason {
    FinalAnswer,
    TurnCap,
    WritePressure,
    Dedup,
    Error(String),
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
    pub cwd: PathBuf,
}

impl Session {
    pub fn new(client: LlmClient, tools: Vec<ToolDef>, cwd: PathBuf, system_prompt: String) -> Self {
        let tools_by_name = tools
            .iter()
            .map(|t| (t.name.clone(), t.clone()))
            .collect();
        Self {
            client,
            tools,
            tools_by_name,
            messages: vec![ChatMessage::system(system_prompt)],
            cache: ToolCache::new(),
            last_calls: Vec::new(),
            last_stop: None,
            cwd,
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
    let mut turns = 0usize;

    loop {
        if turns >= config::MAX_TURNS {
            state.last_stop = Some(StopReason::TurnCap);
            render::guard("turn cap");
            break;
        }

        // Soft elision when pressure climbs.
        if state.pressure() > config::PRESSURE_THRESHOLD {
            state.messages = context::maybe_elide(&state.messages);
        }

        let resp = match state.client.chat(&state.messages, &state.tools) {
            Ok(m) => m,
            Err(e) => {
                state.last_stop = Some(StopReason::Error(e.to_string()));
                render::error(&format!("chat request failed: {e}"));
                break;
            }
        };

        // Push the assistant turn verbatim — including any recovered tool_calls.
        state.messages.push(resp.clone());

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
                render::guard(&format!("dedup → {}", tc.function.name));
                state
                    .messages
                    .push(ChatMessage::tool_result(&tc.id, &tc.function.name, &note));
                dedup_fired = true;
                break;
            }

            // Guard: read-before-write.
            if matches!(tc.function.name.as_str(), "write_file" | "edit_file") {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if !path.is_empty() && !reads.has_seen(path) {
                    let note = guards::read_before_write_note(path);
                    render::guard(&format!("read-before-write → {}", path));
                    state
                        .messages
                        .push(ChatMessage::tool_result(&tc.id, &tc.function.name, &note));
                    continue;
                }
            }

            // Dispatch.
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

            // Semantic compression as a system note alongside the raw result.
            if let Some(summary) = compress::summarize(&call) {
                state.messages.push(ChatMessage::system(format!("Tool summary: {summary}")));
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
                render::guard("write-pressure");
                state.last_calls.push(call);
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

    Ok(())
}

#[cfg(test)]
mod tests {
    // Loop-level tests against the live model belong in tests/smoke_with_model.rs;
    // the guard/cache/context primitives are unit-tested in their own modules.
}
