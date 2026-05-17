//! Harness-level error coaching.
//!
//! When a tool returns an error (or a `bash` command exits non-zero with a
//! recognizable stderr), we prepend a short hint *before* the model sees the
//! result. This converts raw failures into corrective guidance — small
//! models without this often retry the same broken call indefinitely.
//!
//! Two layers:
//!   1. `coach(call)` mutates the tool result content to prepend a hint, IF
//!      we recognize the failure shape.
//!   2. `failure_memory_note(call)` emits a synthetic system-role message
//!      injected after the tool result, telling the model not to repeat the
//!      same call unchanged. Always emitted when `call.error.is_some()`.

use crate::tools::ToolCallResult;

/// Examine the result and prepend a hint to the body if a known failure
/// pattern is detected. Returns the (possibly modified) payload string that
/// should be sent back as the tool result content.
pub fn coach(call: &ToolCallResult) -> String {
    // First: hard tool errors carry their own message; the model gets
    // the error verbatim plus we may add a hint.
    if let Some(err) = &call.error {
        let hint = hint_for_error(&call.name, err);
        return if hint.is_empty() {
            err.clone()
        } else {
            format!("{}\nHint: {}", err, hint)
        };
    }
    // Non-zero bash exit: scan for known stderr patterns.
    if call.name == "bash" {
        let hint = hint_for_bash_output(&call.result);
        if !hint.is_empty() {
            return format!("{}\nHint: {}", call.result, hint);
        }
    }
    call.result.clone()
}

fn hint_for_error(tool: &str, err: &str) -> &'static str {
    let lower = err.to_ascii_lowercase();
    match tool {
        "edit_file" if lower.contains("could not find") => {
            "Read the file again — the snippet may differ in whitespace, or the text may not exist. Try a shorter, unique snippet."
        }
        "edit_file" if lower.contains("matched") && lower.contains("times") => {
            "The snippet is ambiguous. Make it longer/more unique, or pass replace_all=true if you really want to change every occurrence."
        }
        "write_file" if lower.contains("placeholder") => {
            "Replace placeholder markers with the real implementation before calling write_file."
        }
        "write_file" if lower.contains("accidental wipe") => {
            "If you mean to delete content, use edit_file targeting the specific text. write_file replaces the entire file."
        }
        _ if lower.contains("escapes the working directory") => {
            "Paths must be relative to the working directory. Use 'src' or 'src/foo.rs' — no leading slash and no '../'. Retry with a relative path."
        }
        "bash" if lower.contains("metacharacter") => {
            "Run a single command without pipes, redirects, or chaining. If you need multiple steps, call bash multiple times."
        }
        "bash" if lower.contains("not allowed") => {
            "That binary isn't in the allowlist. Try list_dir / read_file / grep, or a different allowlisted tool."
        }
        "bash" if lower.contains("timeout") => {
            "Increase timeout_s or narrow the command — e.g. cargo check instead of cargo test."
        }
        _ => "",
    }
}

fn hint_for_bash_output(out: &str) -> &'static str {
    let lower = out.to_ascii_lowercase();
    if lower.contains("unrecognized option") || lower.contains("unknown option") {
        return "Flag not supported on this system's grep/find. Try the basic form (no -P, no GNU-only flags).";
    }
    if lower.contains("command not found") {
        return "Binary not in allowlist or PATH. Try a different tool — list_dir / read_file / grep are usually enough.";
    }
    if lower.contains("no such file or directory") {
        return "Path doesn't exist. Run list_dir on the parent to confirm the layout.";
    }
    if lower.contains("permission denied") {
        return "Permission denied. The file may be outside the project root or not readable.";
    }
    ""
}

/// Whether to emit a synthetic system note after the tool result telling the
/// model not to repeat the same call unchanged. Mirrors luxe's failure
/// memory: small models will otherwise loop on the same broken call.
pub fn failure_memory_note(call: &ToolCallResult) -> Option<String> {
    let err = call.error.as_ref()?;
    Some(format!(
        "The previous tool call ({}) failed: {}\nDo not repeat the same call unchanged. \
         Adjust arguments, try a different tool, or stop and answer the user.",
        call.name,
        first_line(err)
    ))
}

/// Failure-memory note for guard refusals — the analog of
/// `failure_memory_note` for the `continue`-style guard branches in
/// `agent/mod.rs` (read_before_write, cold_read) that never reach
/// `dispatch` and therefore never accumulate a `ToolCallResult`.
///
/// Returns `Some(note)` only for guard kinds where retry-with-different-
/// shape is the productive outcome. The refusal note pushed as the
/// tool_result already contains the recovery instruction ("Call read_file
/// first" / "Call list_dir … then retry write_file") — what's missing in
/// the guard path versus the placeholder-rejection path is the
/// "do not repeat" system nudge. Adding it closes the gap documented in
/// `lessons.md` 2026-05-17 (`read_before_write` 0/3 recovery vs.
/// placeholder 9/10).
///
/// Returns `None` for guard kinds where retry is either undesired
/// (`cold_read`'s refusal already steers toward "answer the user
/// directly") or structurally pointless (`length`, `turn_cap` end the
/// loop; `dedup`, `write_pressure` are currently unreachable on prompt
/// bait per `09`/`10`). Adding a memory note for those risks unlocking
/// paths whose non-fire shape is intentionally pinned by their anchor
/// fixtures.
pub fn guard_failure_memory_note(tool: &str, kind: &str) -> Option<String> {
    match kind {
        "read_before_write" => Some(format!(
            "The previous {tool} call was refused by the read_before_write guard. \
             Do not repeat the same call unchanged. Read the target path first \
             (call read_file on it), then re-issue the {tool} call."
        )),
        _ => None,
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn err_call(tool: &str, e: &str) -> ToolCallResult {
        ToolCallResult {
            id: "1".into(),
            name: tool.into(),
            arguments: json!({}),
            result: String::new(),
            error: Some(e.into()),
            wall_ms: 0,
            bytes_out: 0,
            cached: false,
        }
    }

    fn ok_call(tool: &str, body: &str) -> ToolCallResult {
        ToolCallResult {
            id: "1".into(),
            name: tool.into(),
            arguments: json!({}),
            result: body.into(),
            error: None,
            wall_ms: 0,
            bytes_out: body.len(),
            cached: false,
        }
    }

    #[test]
    fn edit_file_no_match_gets_hint() {
        let c = err_call("edit_file", "edit_file: could not find `old` in a.rs.");
        let coached = coach(&c);
        assert!(coached.contains("could not find"));
        assert!(coached.contains("Hint"));
        assert!(coached.contains("whitespace"));
    }

    #[test]
    fn edit_file_ambiguous_gets_hint() {
        let c = err_call("edit_file", "edit_file: `old` matched 3 times in foo.rs.");
        let hint = coach(&c);
        assert!(hint.contains("ambiguous"));
    }

    #[test]
    fn bash_pipe_gets_hint() {
        let c = err_call("bash", "Rejected: token contains metacharacter '|'.");
        let hint = coach(&c);
        assert!(hint.contains("Hint"));
        assert!(hint.contains("pipes"));
    }

    #[test]
    fn bash_unrecognized_option_hint() {
        let c = ok_call(
            "bash",
            "$ grep -P TODO\nexit=2 time=10ms\n--- stderr ---\ngrep: unrecognized option: -P\n",
        );
        let coached = coach(&c);
        assert!(coached.contains("Flag not supported"));
    }

    #[test]
    fn no_hint_for_unknown_error_shape() {
        let c = err_call("edit_file", "some unrelated mystery");
        let coached = coach(&c);
        assert_eq!(coached, "some unrelated mystery");
    }

    #[test]
    fn failure_memory_present_on_error() {
        let c = err_call("read_file", "Cannot stat x: not found");
        assert!(failure_memory_note(&c).is_some());
    }

    #[test]
    fn failure_memory_absent_on_success() {
        let c = ok_call("read_file", "contents");
        assert!(failure_memory_note(&c).is_none());
    }

    #[test]
    fn guard_failure_memory_fires_for_read_before_write_edit() {
        let note = guard_failure_memory_note("edit_file", "read_before_write");
        let note = note.expect("read_before_write should produce a memory note");
        assert!(note.contains("edit_file"));
        assert!(note.contains("read_file"));
        assert!(note.contains("do not repeat") || note.contains("Do not repeat"));
    }

    #[test]
    fn guard_failure_memory_fires_for_read_before_write_write() {
        // Same kind, different tool. The note must surface the tool name
        // so the model sees the actual call shape it should not repeat.
        let note = guard_failure_memory_note("write_file", "read_before_write");
        let note = note.expect("read_before_write should produce a memory note");
        assert!(note.contains("write_file"));
    }

    #[test]
    fn guard_failure_memory_silent_for_cold_read() {
        // Documented no-op per bench/PREDICATES.md guard audit rubric:
        // cold_read's refusal text already says "Answer the user directly"
        // — adding a "do not repeat" nudge on top would be redundant and
        // risks unlocking the 03-decline-irrelevant non-call shape. The
        // refusal itself is the correct affordance; no auto-recovery is
        // appropriate.
        assert!(guard_failure_memory_note("read_file", "cold_read").is_none());
    }

    #[test]
    fn guard_failure_memory_silent_for_safety_brake_guards() {
        // Documented no-op per bench/PREDICATES.md doctrine: "Guards that
        // exist as safety brakes against runaway model behavior must not
        // have auto-recovery affordances." Auto-recovery on a safety
        // brake is "ignore the brake" — defeats the brake's purpose.
        //
        // dedup fires on consecutive-identical-call loops; the only
        // mechanically-resolving "recovery" is exiting the loop, which
        // dedup already does. write_pressure fires on a post-write
        // zero-byte streak suggesting the model has nothing useful left
        // to do; same reasoning — recovery is exit. Both are pinned as
        // structurally-unreachable on 1.5 B by fixtures 09 and 10
        // respectively (`lessons.md` 2026-05-16), so the absence of
        // auto-recovery is also empirically untestable on this model
        // scale even if it were architecturally permitted.
        assert!(guard_failure_memory_note("read_file", "dedup").is_none());
        assert!(guard_failure_memory_note("list_dir", "write_pressure").is_none());
    }

    #[test]
    fn guard_failure_memory_silent_for_terminal_guards() {
        // Documented no-op per bench/PREDICATES.md guard audit rubric:
        // length and turn_cap are terminal — both end the loop, leaving
        // no next iteration where a memory note could influence
        // anything. turn_cap is additionally a safety brake (same
        // doctrine as dedup / write_pressure above).
        //
        // For length specifically: the Tier-2.2 probe (fixture
        // 13a-length-write-file-bulk + the fixture-12 stress envelope
        // covering semantic-derailment + fixture 04 covering
        // clean-cutoff) characterized the three plausible failure
        // families. None admits a clean auto-recovery shape:
        //   - malformed-args: empirically empty on 1.5 B (model
        //     abstracts long inputs to short tool args; probe 13a
        //     showed 10/10 reps at <3000 tokens with no length fire).
        //   - semantic-derailment: model is in a prose loop;
        //     "retry with tighter max_tokens" produces shorter loop.
        //   - clean-cutoff: model is producing legitimate output that
        //     exceeded max_tokens; resumption requires reliable
        //     mid-token continuation which the model isn't.
        // Current handling (push "be more concise" note for the next
        // user turn, break) is the correct disposition. See
        // `lessons.md` 2026-05-17 (fifth entry) for the empirical
        // record.
        assert!(guard_failure_memory_note("read_file", "length").is_none());
        assert!(guard_failure_memory_note("read_file", "turn_cap").is_none());
    }
}
