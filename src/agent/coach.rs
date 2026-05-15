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
}
