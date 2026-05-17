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
use crate::llm::types::{ChatMessage, FunctionCall, ToolCall, Usage};
use crate::repl::render;
use crate::tools::cache::ToolCache;
use crate::tools::{ToolCallResult, ToolDef, dispatch};
use micro_mind::obs::{Event, Recorder, RecorderHandle, ToolOrigin};

/// Maximum line count tolerated for an auto-read on `read_before_write`
/// recovery. The tool layer's 24 KB byte default already constrains context
/// poisoning by volume; this guards against pathological cases (minified
/// or generated files with thousands of short lines) where the byte cap
/// alone could still drown the model in line noise. 800 lines comfortably
/// covers every source file in this repo.
const AUTO_READ_LINE_CEILING: usize = 800;

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

/// Attempt to recover from a `read_before_write` guard fire by performing
/// a bounded `read_file` ourselves and returning its content. On success
/// the caller delivers the content to the model as a synthetic
/// `tool_call(read_file)` + paired `tool_result` (b-toolresult shape —
/// see `synthetic_read_call_message`), collapsing the two-hop chain
/// (refusal → model reads → model retries with edit) into the single
/// hop the 1.5 B model can sustain on the 0% BFCL multi-turn floor.
/// Achieves 87% task success across 30 cold-server stress reps; see
/// `lessons.md` 2026-05-17 (fourth entry) and
/// `bench/baselines/main/12-stress-envelope.json`.
///
/// Bounding:
///   - The `read_file` tool layer enforces its own byte cap (24 KB default
///     / 64 KB hard), so we don't repeat that here.
///   - We additionally refuse content with more than `AUTO_READ_LINE_CEILING`
///     lines, catching pathological minified/generated files where the
///     byte cap alone doesn't constrain line count.
///
/// Provenance:
///   - Emits `ToolCall` and `ToolResult` events with
///     `origin = ToolOrigin::SyntheticGuardRecovery { guard:
///     "read_before_write" }` so trace consumers can distinguish the
///     harness-injected call from model output (schema v3).
///   - Always emits the `ToolResult` event, even on failure, so the trace
///     is balanced (one call → one result, regardless of outcome).
///
/// Recursion invariant:
///   - The synthetic dispatch is always `read_file`, never
///     `write_file`/`edit_file`; `read_before_write` only fires on the
///     latter, so the auto-read cannot itself trigger another auto-read.
///     The single-hop guarantee is structural, not policy.
///
/// Generalization marker (do NOT lift yet):
///   - If a second guard kind earns an auto-recovery affordance per the
///     audit rubric in `bench/PREDICATES.md` (today only
///     `read_before_write` qualifies — `length` is the most plausible
///     second candidate pending the probe in Tier 2.2 of the post-Phase-B
///     plan), the right lift is a `recovery_action: Option<SyntheticTool>`
///     shape carried on guard kinds at config time, with this function
///     becoming the dispatch for `SyntheticTool::ReadFile`. Premature
///     until that second user genuinely exists — see the abstraction-lift
///     gate documented in the post-(b) revised plan. Keep this comment as
///     the design-intent marker so the lift, when it happens, doesn't
///     have to be reverse-engineered from the call site.
fn try_auto_read_for_rbw(
    recorder: &dyn Recorder,
    tools_by_name: &HashMap<String, ToolDef>,
    cache: &mut ToolCache,
    target_path: &str,
    turn: u32,
    blocked_tool_call_id: &str,
) -> Option<ToolCallResult> {
    let synthetic_id = format!("synthetic-rbw-{blocked_tool_call_id}");
    let args = serde_json::json!({ "path": target_path });

    let origin = ToolOrigin::SyntheticGuardRecovery {
        guard: "read_before_write".into(),
    };

    recorder.record(Event::ToolCall {
        turn,
        name: "read_file".into(),
        arguments: args.clone(),
        tool_call_id: synthetic_id.clone(),
        origin: Some(origin.clone()),
    });
    render::tool_call_start("read_file", &args);

    let mut call = dispatch("read_file", &args, &synthetic_id, tools_by_name, cache);

    if call.error.is_some() {
        recorder.record(Event::ToolResult {
            turn,
            name: "read_file".into(),
            tool_call_id: synthetic_id,
            ok: false,
            wall_ms: call.wall_ms,
            bytes_out: 0,
            cached: call.cached,
            error: call.error.clone(),
            origin: Some(origin),
        });
        render::tool_call_result(&call);
        return None;
    }

    let line_count = call.result.lines().count();
    if line_count > AUTO_READ_LINE_CEILING {
        let err = format!(
            "auto-read aborted: {line_count} lines exceeds {AUTO_READ_LINE_CEILING}-line ceiling"
        );
        call.error = Some(err.clone());
        recorder.record(Event::ToolResult {
            turn,
            name: "read_file".into(),
            tool_call_id: synthetic_id,
            ok: false,
            wall_ms: call.wall_ms,
            bytes_out: call.bytes_out,
            cached: call.cached,
            error: Some(err),
            origin: Some(origin),
        });
        render::tool_call_result(&call);
        return None;
    }

    recorder.record(Event::ToolResult {
        turn,
        name: "read_file".into(),
        tool_call_id: synthetic_id,
        ok: true,
        wall_ms: call.wall_ms,
        bytes_out: call.bytes_out,
        cached: call.cached,
        error: None,
        origin: Some(origin),
    });
    render::tool_call_result(&call);
    Some(call)
}

/// Fabricate an assistant message carrying a synthetic `tool_call` so the
/// auto-read content can be delivered to the model as a proper tool_result
/// (paired with this fabricated call) rather than as a system note. The
/// chat template's "after a tool_result" continuation reliably leads to
/// another tool_call on Qwen-style templates; the "after a system note"
/// continuation is biased toward emit-prose. b-toolresult probes whether
/// this format change alone shifts the model from prose-the-edit mode to
/// emit-edit_file mode on the recovery turn.
fn synthetic_read_call_message(tool_call_id: &str, path: &str) -> ChatMessage {
    let arguments = serde_json::to_string(&serde_json::json!({ "path": path }))
        .unwrap_or_else(|_| "{}".to_string());
    ChatMessage {
        role: "assistant".into(),
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: vec![ToolCall {
            id: tool_call_id.to_string(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments,
            },
        }],
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
                    // Auto-read on guard refusal — b-toolresult shape (the
                    // reps-10 stress run on b-current at 4/10 task success
                    // showed the model's "after a system note" continuation
                    // prose-the-edit 6/10 of the time. b-toolresult delivers
                    // the synthetic content as a proper tool_result paired
                    // with a fabricated assistant tool_call, so the chat
                    // template sees the natural tool-loop continuation
                    // pattern instead of a free-form system note.)
                    //
                    // Conversation shape on success:
                    //   assistant: tool_call(edit_file, id=A)   [blocked]
                    //   tool[A]:   refusal_note                 [counterfactual visibility]
                    //   assistant: tool_call(read_file, id=B)   [fabricated]
                    //   tool[B]:   <auto-read content>          [synthetic tool_result]
                    //   (next turn — model)
                    //
                    // Counterfactual visibility is preserved: the refusal
                    // note still answers the blocked call; the synthetic
                    // pair is recorded in the trace with
                    // origin=SyntheticGuardRecovery via try_auto_read_for_rbw.
                    state
                        .messages
                        .push(ChatMessage::tool_result(&tc.id, &tc.function.name, &note));
                    let synth = try_auto_read_for_rbw(
                        state.recorder.as_ref(),
                        &state.tools_by_name,
                        &mut state.cache,
                        path,
                        turns,
                        &tc.id,
                    );
                    if let Some(synth_call) = synth {
                        state
                            .messages
                            .push(synthetic_read_call_message(&synth_call.id, path));
                        state.messages.push(ChatMessage::tool_result(
                            &synth_call.id,
                            "read_file",
                            &synth_call.result,
                        ));
                        // Mark the path as read so the model's retry of
                        // `write_file`/`edit_file` doesn't re-trigger the
                        // guard. Without this, the next turn would loop
                        // through the same refusal path on every iteration.
                        reads.record_read(&synth_call.name, &synth_call.arguments);
                    } else if let Some(memory_note) =
                        coach::guard_failure_memory_note(&tc.function.name, "read_before_write")
                    {
                        // Auto-read fallback: mirror the pre-(b) shape — the
                        // model gets the refusal note + a do-not-repeat
                        // nudge and has to compose the read itself. Same
                        // failure-memory wiring as the dispatch path.
                        state.messages.push(ChatMessage::system(memory_note));
                    }
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
                // Symmetric with read_before_write: ask coach whether this
                // guard kind wants a failure-memory nudge. Returns None for
                // cold_read today (the refusal note already steers toward
                // "answer the user directly"), so the call is a no-op — but
                // wiring it here means new guard kinds inherit the option
                // through `guard_failure_memory_note` rather than needing
                // per-site code changes.
                if let Some(memory_note) =
                    coach::guard_failure_memory_note(&tc.function.name, "cold_read")
                {
                    state.messages.push(ChatMessage::system(memory_note));
                }
                continue;
            }

            // Dispatch.
            state.recorder.record(Event::ToolCall {
                turn: turns,
                name: tc.function.name.clone(),
                arguments: args.clone(),
                tool_call_id: tc.id.clone(),
                origin: None,
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
                origin: None,
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
