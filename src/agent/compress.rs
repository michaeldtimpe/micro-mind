//! Tool-result compressor.
//!
//! After each tool dispatch, emit a compact one-line *semantic* summary
//! alongside the raw result. Tiny models respond disproportionately well to
//! compressed state vs raw token sludge — review-confirmed as probably the
//! highest-leverage addition beyond the base loop.
//!
//! The summary is appended to the conversation as a system note, so the model
//! sees both: the raw result (truncated to 8 KB by the tool layer) and a
//! one-line distillation.

use crate::tools::ToolCallResult;

/// Build a one-line semantic summary for a tool call result.
/// Returns `None` for tool calls that don't benefit from a summary.
pub fn summarize(call: &ToolCallResult) -> Option<String> {
    if let Some(err) = &call.error {
        return Some(format!("{} → ERROR: {}", call.name, first_line(err)));
    }
    match call.name.as_str() {
        "read_file" => Some(summarize_read_file(call)),
        "list_dir" => Some(summarize_list_dir(call)),
        "list_files_recursive" => Some(summarize_list_recursive(call)),
        "grep" => Some(summarize_grep(call)),
        "bash" => Some(summarize_bash(call)),
        "write_file" | "edit_file" => Some(format!("{} → {}", call.name, first_line(&call.result))),
        _ => None,
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim()
}

fn summarize_read_file(call: &ToolCallResult) -> String {
    let path = call
        .arguments
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let body = strip_header(&call.result);
    let lines = body.lines().count();
    let bytes = body.len();
    let extras = scan_signals(body);
    let extras_part = if extras.is_empty() {
        String::new()
    } else {
        format!(", {}", extras.join(", "))
    };
    format!(
        "read_file {} → {} lines, {} bytes{}{}",
        path,
        lines,
        bytes,
        extras_part,
        if call.cached { " (cached)" } else { "" }
    )
}

fn summarize_list_dir(call: &ToolCallResult) -> String {
    let path = call
        .arguments
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let lines: Vec<&str> = call
        .result
        .lines()
        .filter(|l| !l.starts_with('['))
        .collect();
    let dirs = lines.iter().filter(|l| l.ends_with('/')).count();
    let files = lines.len() - dirs;
    format!("list_dir {} → {} files, {} dirs", path, files, dirs)
}

fn summarize_list_recursive(call: &ToolCallResult) -> String {
    let path = call
        .arguments
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let lines: Vec<&str> = call
        .result
        .lines()
        .filter(|l| !l.starts_with('['))
        .collect();
    let count = lines.len();
    let top_exts = top_extensions(&lines);
    let ext_part = if top_exts.is_empty() {
        String::new()
    } else {
        format!(", mostly {}", top_exts.join("/"))
    };
    format!(
        "list_files_recursive {} → {} entries{}",
        path, count, ext_part
    )
}

fn summarize_grep(call: &ToolCallResult) -> String {
    let pattern = call
        .arguments
        .get("pattern")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let path = call
        .arguments
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    if call.result.starts_with("No matches") {
        return format!("grep /{}/ {} → 0 matches", pattern, path);
    }
    let lines: Vec<&str> = call
        .result
        .lines()
        .filter(|l| !l.starts_with('['))
        .collect();
    let files: std::collections::HashSet<&str> =
        lines.iter().filter_map(|l| l.split(':').next()).collect();
    format!(
        "grep /{}/ {} → {} matches in {} files",
        pattern,
        path,
        lines.len(),
        files.len()
    )
}

fn summarize_bash(call: &ToolCallResult) -> String {
    // Tool result format: "$ cmd\nexit=N time=Tms\n--- stdout ---\n...\n--- stderr ---\n..."
    let mut lines = call.result.lines();
    let cmd_line = lines.next().unwrap_or("$ ?").trim_start_matches("$ ");
    let meta = lines.next().unwrap_or("");
    let mut exit_code: i32 = 0;
    let mut time_ms: String = "?".into();
    for piece in meta.split_whitespace() {
        if let Some(rest) = piece.strip_prefix("exit=") {
            exit_code = rest.parse().unwrap_or(0);
        }
        if let Some(rest) = piece.strip_prefix("time=") {
            time_ms = rest.to_string();
        }
    }
    let status = if exit_code == 0 { "OK" } else { "FAIL" };
    let extras = bash_extras(&call.result);
    let extras_part = if extras.is_empty() {
        String::new()
    } else {
        format!(", {}", extras.join("; "))
    };
    format!(
        "bash `{}` → {} (exit={} {}){}",
        truncate_mid(cmd_line, 60),
        status,
        exit_code,
        time_ms,
        extras_part
    )
}

fn bash_extras(out: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for line in out.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("passed") && lower.contains("failed") {
            hits.push(line.trim().to_string());
            break;
        }
        if line.contains("error[E") || line.contains("error:") {
            hits.push(line.trim().to_string());
            if hits.len() >= 2 {
                break;
            }
        }
    }
    hits.into_iter().take(2).collect()
}

fn truncate_mid(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = max / 2 - 1;
    let tail = max - head - 3;
    let head_slice: String = s.chars().take(head).collect();
    let tail_slice: String = s
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{}...{}", head_slice, tail_slice)
}

fn strip_header(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix('[')
        && let Some(idx) = rest.find("]\n")
    {
        return &rest[idx + 2..];
    }
    text
}

fn scan_signals(text: &str) -> Vec<String> {
    let mut sig = Vec::new();
    // Look for common Rust/Python/JS function definitions in the first ~3KB.
    let head: String = text.chars().take(3000).collect();
    let mut fns = Vec::new();
    for line in head.lines() {
        let l = line.trim_start();
        if l.starts_with("fn ")
            || l.starts_with("pub fn ")
            || l.starts_with("def ")
            || l.starts_with("function ")
        {
            if let Some(name) = extract_def_name(l) {
                fns.push(name);
            }
            if fns.len() >= 3 {
                break;
            }
        }
    }
    if !fns.is_empty() {
        sig.push(format!("defines {}", fns.join(", ")));
    }
    sig
}

fn extract_def_name(line: &str) -> Option<String> {
    for prefix in &["pub fn ", "fn ", "def ", "function "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn top_extensions(entries: &[&str]) -> Vec<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for e in entries {
        if let Some(dot) = e.rfind('.') {
            let ext = &e[dot + 1..];
            if !ext.is_empty() && ext.len() <= 6 {
                *counts.entry(format!("*.{ext}")).or_insert(0) += 1;
            }
        }
    }
    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    sorted.into_iter().take(2).map(|(k, _)| k).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk(name: &str, args: serde_json::Value, result: &str) -> ToolCallResult {
        ToolCallResult {
            id: "1".into(),
            name: name.into(),
            arguments: args,
            result: result.into(),
            error: None,
            wall_ms: 1,
            bytes_out: result.len(),
            cached: false,
        }
    }

    #[test]
    fn summarize_read_file_counts_lines() {
        let r = mk(
            "read_file",
            json!({"path": "src/main.rs"}),
            "[src/main.rs bytes=0..30 of 30]\nfn main() {\n    println!(\"hi\");\n}",
        );
        let s = summarize(&r).unwrap();
        assert!(s.contains("read_file src/main.rs"));
        assert!(s.contains("lines"));
        assert!(s.contains("defines main"));
    }

    #[test]
    fn summarize_grep_counts() {
        let r = mk(
            "grep",
            json!({"pattern": "TODO", "path": "src/"}),
            "src/a.rs:3:// TODO\nsrc/b.rs:7: TODO again\n",
        );
        let s = summarize(&r).unwrap();
        assert!(s.contains("grep /TODO/"));
        assert!(s.contains("2 matches"));
        assert!(s.contains("2 files"));
    }

    #[test]
    fn summarize_grep_no_matches() {
        let r = mk(
            "grep",
            json!({"pattern": "FOO", "path": "."}),
            "No matches for /FOO/ in .\n",
        );
        let s = summarize(&r).unwrap();
        assert!(s.contains("0 matches"));
    }

    #[test]
    fn summarize_bash_ok() {
        let r = mk(
            "bash",
            json!({"cmd": "cargo check"}),
            "$ cargo check\nexit=0 time=1234ms\n--- stdout ---\nFinished\n",
        );
        let s = summarize(&r).unwrap();
        assert!(s.contains("bash"));
        assert!(s.contains("OK"));
        assert!(s.contains("exit=0"));
    }

    #[test]
    fn summarize_bash_fail_with_error() {
        let r = mk(
            "bash",
            json!({"cmd": "cargo build"}),
            "$ cargo build\nexit=101 time=900ms\n--- stderr ---\nerror[E0425]: cannot find value `x`\n",
        );
        let s = summarize(&r).unwrap();
        assert!(s.contains("FAIL"));
        assert!(s.contains("E0425"));
    }

    #[test]
    fn summarize_error_passthrough() {
        let mut r = mk("read_file", json!({"path": "x"}), "");
        r.error = Some("Path escapes the working directory: ../etc".into());
        let s = summarize(&r).unwrap();
        assert!(s.starts_with("read_file → ERROR"));
    }

    #[test]
    fn summarize_list_dir() {
        let r = mk(
            "list_dir",
            json!({"path": "src"}),
            "main.rs\nlib.rs\ntools/\nllm/\n",
        );
        let s = summarize(&r).unwrap();
        assert!(s.contains("list_dir src"));
        assert!(s.contains("2 files"));
        assert!(s.contains("2 dirs"));
    }

    #[test]
    fn summarize_list_recursive_top_exts() {
        let r = mk(
            "list_files_recursive",
            json!({"path": "."}),
            "src/main.rs\nsrc/lib.rs\nCargo.toml\nREADME.md\n",
        );
        let s = summarize(&r).unwrap();
        assert!(s.contains("4 entries"));
        assert!(s.contains("*.rs"));
    }
}
