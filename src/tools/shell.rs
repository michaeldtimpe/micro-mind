use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config;
use crate::tools::ToolDef;

const ALLOWLIST: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "find", "git", "cargo", "make", "python", "python3",
    "node", "npm", "pytest", "rustc", "rustfmt", "echo", "true", "false", "pwd",
];

/// Tokens / substrings rejected anywhere in any argv element.
/// We deliberately do not run via /bin/sh, but if the model emits these
/// they're a strong sign of an injection attempt and the rejection itself
/// is useful coaching.
const BAD_SUBSTRINGS: &[&str] = &["|", ">", "<", "&", ";", "$(", "`", "&&", "||", ">>"];

pub fn bash(cwd: PathBuf) -> ToolDef {
    let params = json!({
        "type": "object",
        "properties": {
            "cmd": {"type": "string", "description": "Shell command. Single command only — no pipes, redirects, &&, ;, $(...). First token must be allowlisted."},
            "timeout_s": {"type": "integer", "description": "Max seconds before kill. Default 30."}
        },
        "required": ["cmd"]
    });

    ToolDef::new(
        "bash",
        "Run an allowlisted command. NO pipes, redirects, &&, ;, $(...). Use one command per call.",
        params,
        move |args| -> Result<String, String> {
            let cmd_str = args.get("cmd").and_then(|v| v.as_str()).ok_or("cmd required")?;
            let timeout_s = args
                .get("timeout_s")
                .and_then(|v| v.as_u64())
                .unwrap_or(30)
                .min(300);

            let tokens = shlex::split(cmd_str)
                .ok_or_else(|| format!("Could not parse command (unbalanced quote?): {cmd_str}"))?;
            if tokens.is_empty() {
                return Err("Empty command.".into());
            }

            for tok in &tokens {
                for bad in BAD_SUBSTRINGS {
                    if tok.contains(bad) {
                        return Err(format!(
                            "Rejected: token {:?} contains shell metacharacter '{}'. \
                             Run one command at a time without pipes/redirects/chaining.",
                            tok, bad
                        ));
                    }
                }
            }

            let exe = &tokens[0];
            if !ALLOWLIST.contains(&exe.as_str()) {
                return Err(format!(
                    "Executable not allowed: {}. Allowlist: {}",
                    exe,
                    ALLOWLIST.join(", ")
                ));
            }

            // `python -c "..."` and `node -e "..."` are an escape hatch around the
            // metacharacter check — reject them explicitly.
            if matches!(exe.as_str(), "python" | "python3" | "node")
                && tokens.iter().any(|t| t == "-c" || t == "-e")
            {
                return Err(format!(
                    "Rejected: {} -c/-e is not allowed (lets the model bypass the command allowlist). \
                     Save the snippet to a file and run it instead.",
                    exe
                ));
            }

            let start = Instant::now();
            let mut child = Command::new(exe)
                .args(&tokens[1..])
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("Spawn failed: {}", e))?;

            // Poll-and-kill timeout using try_wait. Avoids pulling in tokio/std::thread orchestration.
            let deadline = start + Duration::from_secs(timeout_s);
            let exit_status = loop {
                if let Some(status) = child.try_wait().map_err(|e| format!("Wait failed: {}", e))? {
                    break status;
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "Killed after {}s timeout: {}",
                        timeout_s, cmd_str
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            };

            let mut stdout_buf = String::new();
            let mut stderr_buf = String::new();
            if let Some(mut out) = child.stdout.take() {
                use std::io::Read;
                let _ = out.read_to_string(&mut stdout_buf);
            }
            if let Some(mut err) = child.stderr.take() {
                use std::io::Read;
                let _ = err.read_to_string(&mut stderr_buf);
            }

            let elapsed_ms = start.elapsed().as_millis();
            let mut out = String::new();
            out.push_str(&format!(
                "$ {}\nexit={} time={}ms\n",
                cmd_str,
                exit_status.code().unwrap_or(-1),
                elapsed_ms
            ));
            if !stdout_buf.is_empty() {
                out.push_str("--- stdout ---\n");
                out.push_str(&stdout_buf);
                if !stdout_buf.ends_with('\n') {
                    out.push('\n');
                }
            }
            if !stderr_buf.is_empty() {
                out.push_str("--- stderr ---\n");
                out.push_str(&stderr_buf);
                if !stderr_buf.ends_with('\n') {
                    out.push('\n');
                }
            }

            // Per-tool 8 KB cap is applied centrally in tools::hard_truncate, but
            // we shave the visible body here too so the format markers survive.
            if out.len() > config::TOOL_OUTPUT_HARD_CAP {
                let kept = config::TOOL_OUTPUT_HARD_CAP - 100;
                out.truncate(kept);
                out.push_str("\n[truncated by shell.rs]");
            }

            // Non-zero exit is surfaced as Ok content (the model needs the diagnostic
            // to recover), not as a tool error. The coach layer will append a hint
            // on top if the stderr matches a known pattern.
            Ok(out)
        },
    )
    .cacheable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_pipe() {
        let dir = std::env::temp_dir();
        let tool = bash(dir.clone());
        let err = (tool.function)(&json!({"cmd": "ls | grep foo"})).unwrap_err();
        assert!(err.contains("metacharacter") || err.contains("not allowed"));
    }

    #[test]
    fn rejects_redirect() {
        let dir = std::env::temp_dir();
        let tool = bash(dir.clone());
        let err = (tool.function)(&json!({"cmd": "ls > out.txt"})).unwrap_err();
        assert!(err.contains("metacharacter"));
    }

    #[test]
    fn rejects_command_substitution() {
        let dir = std::env::temp_dir();
        let tool = bash(dir.clone());
        let err = (tool.function)(&json!({"cmd": "cat $(ls)"})).unwrap_err();
        assert!(err.contains("metacharacter"));
    }

    #[test]
    fn rejects_python_dash_c() {
        let dir = std::env::temp_dir();
        let tool = bash(dir.clone());
        let err = (tool.function)(&json!({"cmd": "python -c import os"})).unwrap_err();
        assert!(err.contains("-c/-e"));
    }

    #[test]
    fn rejects_non_allowlisted() {
        let dir = std::env::temp_dir();
        let tool = bash(dir.clone());
        let err = (tool.function)(&json!({"cmd": "curl http://example.com"})).unwrap_err();
        assert!(err.contains("not allowed"));
    }

    #[test]
    fn runs_simple_echo() {
        let dir = std::env::temp_dir();
        let tool = bash(dir.clone());
        let out = (tool.function)(&json!({"cmd": "echo hello"})).unwrap();
        assert!(out.contains("hello"));
        assert!(out.contains("exit=0"));
    }
}
