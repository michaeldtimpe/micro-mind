use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

use crate::config;
use crate::llm::types::{
    ChatMessage, ChatRequest, ChatResponse, FunctionCall, ToolCall, ToolDefInner, ToolDefWire,
    Usage,
};
use crate::tools::ToolDef;

/// Outcome of a single chat round-trip. `native_tool_calls` is the count of
/// tool_calls returned by the server; recovered tool_calls (from prose) are
/// already merged into `message.tool_calls` and counted separately.
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub message: ChatMessage,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
    pub native_tool_calls: usize,
    pub recovered_tool_calls: usize,
}

pub struct LlmClient {
    pub base_url: String,
    pub model_name: String,
    agent: ureq::Agent,
}

impl LlmClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(180))
            .build();
        Self {
            base_url: base_url.into(),
            model_name: "qwen25-1.5b-instruct".into(),
            agent,
        }
    }

    pub fn chat(&self, messages: &[ChatMessage], tools: &[ToolDef]) -> Result<ChatOutcome> {
        let wire_tools: Vec<ToolDefWire> = tools
            .iter()
            .map(|t| ToolDefWire {
                kind: "function",
                function: ToolDefInner {
                    name: &t.name,
                    description: &t.description,
                    parameters: &t.parameters,
                },
            })
            .collect();

        let req = ChatRequest {
            model: &self.model_name,
            messages,
            tools: if wire_tools.is_empty() {
                None
            } else {
                Some(&wire_tools)
            },
            tool_choice: if wire_tools.is_empty() {
                None
            } else {
                Some("auto")
            },
            temperature: config::TEMPERATURE,
            top_p: config::TOP_P,
            repeat_penalty: config::REPEAT_PENALTY,
            seed: config::SEED,
            max_tokens: config::MAX_TOKENS,
        };

        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
        let resp: ChatResponse = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_json(serde_json::to_value(&req).context("encode request")?)
            .context("POST /v1/chat/completions")?
            .into_json()
            .context("decode chat response")?;

        let usage = resp.usage.clone();
        let choice = resp
            .choices
            .into_iter()
            .next()
            .context("no choices in chat response")?;
        let finish_reason = choice.finish_reason.clone();
        let mut msg = choice.message;
        let native_tool_calls = msg.tool_calls.len();
        let mut recovered_tool_calls = 0usize;

        // Text-recovery fallback: if no native tool_calls but the content
        // contains a parseable tool call, promote it. Belt-and-braces for
        // small models that occasionally fall back to text-channel calls.
        if msg.tool_calls.is_empty() {
            if let Some(text) = msg.content.as_deref() {
                let recovered = recover_tool_calls_from_text(text);
                if !recovered.is_empty() {
                    // Strip the recovered fragments out of the visible content.
                    let stripped = strip_recovered(text);
                    msg.content = if stripped.trim().is_empty() {
                        None
                    } else {
                        Some(stripped)
                    };
                    recovered_tool_calls = recovered.len();
                    msg.tool_calls = recovered;
                }
            }
        }

        Ok(ChatOutcome {
            message: msg,
            usage,
            finish_reason,
            native_tool_calls,
            recovered_tool_calls,
        })
    }
}

/// Try to recover tool calls embedded in assistant prose.
///
/// Two shapes are supported (mirroring luxe's `_parse_text_tool_calls`):
///   1. `<tool_call>{"name": ..., "arguments": {...}}</tool_call>`
///   2. A standalone top-level JSON object with `name` and `arguments` keys.
pub fn recover_tool_calls_from_text(text: &str) -> Vec<ToolCall> {
    let mut out = Vec::new();
    let mut counter = 0;

    // Pattern 1: <tool_call>...</tool_call>
    let open = "<tool_call>";
    let close = "</tool_call>";
    let mut cursor = 0;
    while let Some(start) = text[cursor..].find(open) {
        let abs_start = cursor + start + open.len();
        let Some(rel_end) = text[abs_start..].find(close) else {
            break;
        };
        let abs_end = abs_start + rel_end;
        let payload = text[abs_start..abs_end].trim();
        if let Some(tc) = parse_named_call(payload, &mut counter) {
            out.push(tc);
        }
        cursor = abs_end + close.len();
    }

    // Pattern 2: bare top-level JSON with name+arguments — only if we
    // didn't find any wrapped calls, to avoid double-counting.
    if out.is_empty() {
        if let Some(json_slice) = extract_first_balanced_json(text) {
            if let Some(tc) = parse_named_call(json_slice, &mut counter) {
                out.push(tc);
            }
        }
    }

    out
}

fn parse_named_call(s: &str, counter: &mut usize) -> Option<ToolCall> {
    let v: Value = serde_json::from_str(s).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let args = v
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let args_str = match args {
        Value::String(s) => s,
        other => serde_json::to_string(&other).ok()?,
    };
    *counter += 1;
    Some(ToolCall {
        id: format!("recovered_{}", counter),
        kind: "function".into(),
        function: FunctionCall {
            name,
            arguments: args_str,
        },
    })
}

fn extract_first_balanced_json(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_recovered(text: &str) -> String {
    let mut s = text.to_string();
    // Remove all <tool_call>...</tool_call> blocks.
    loop {
        let Some(start) = s.find("<tool_call>") else {
            break;
        };
        let Some(end) = s[start..].find("</tool_call>") else {
            break;
        };
        let abs_end = start + end + "</tool_call>".len();
        s.replace_range(start..abs_end, "");
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_wrapped_tool_call() {
        let text = r#"Let me check.<tool_call>{"name":"read_file","arguments":{"path":"x.rs"}}</tool_call>"#;
        let calls = recover_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
        assert!(calls[0].function.arguments.contains("x.rs"));
    }

    #[test]
    fn recovers_bare_json() {
        let text = r#"{"name":"grep","arguments":{"pattern":"TODO"}}"#;
        let calls = recover_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "grep");
    }

    #[test]
    fn ignores_plain_prose() {
        let text = "Hello, world. Two plus two is four.";
        let calls = recover_tool_calls_from_text(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn handles_arguments_as_string() {
        // Some models emit arguments already as a string.
        let text = r#"<tool_call>{"name":"x","arguments":"{\"k\":1}"}</tool_call>"#;
        let calls = recover_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments, "{\"k\":1}");
    }

    #[test]
    fn strip_recovered_removes_blocks() {
        let s = "before <tool_call>{}</tool_call> after";
        assert_eq!(strip_recovered(s), "before  after".trim());
    }
}
