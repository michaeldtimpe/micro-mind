//! Token estimation + context pressure + write-aware tool-result elision.
//!
//! Port of luxe's `context.py` with one critical change for small models:
//! **successful write_file / edit_file tool messages are never elided.**
//! Without this, the model forgets which files it has already changed and
//! either re-edits them or undoes earlier work.

use crate::config;
use crate::llm::types::ChatMessage;

/// Approximate token count for a string. 4 chars/token is the standard rule of thumb.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

pub fn estimate_messages_tokens(messages: &[ChatMessage]) -> usize {
    let mut total = 0usize;
    for m in messages {
        if let Some(c) = &m.content {
            total += estimate_tokens(c);
        }
        for tc in &m.tool_calls {
            total += estimate_tokens(&tc.function.name);
            total += estimate_tokens(&tc.function.arguments);
        }
        total += 4; // framing overhead
    }
    total
}

pub fn pressure(messages: &[ChatMessage], ctx_limit: usize) -> f32 {
    if ctx_limit == 0 {
        return 0.0;
    }
    estimate_messages_tokens(messages) as f32 / ctx_limit as f32
}

/// Identify whether a tool message represents a successful write that we
/// want to preserve through elision.
fn is_durable_write_result(m: &ChatMessage) -> bool {
    if m.role != "tool" {
        return false;
    }
    let Some(name) = m.name.as_deref() else { return false };
    if name != "write_file" && name != "edit_file" {
        return false;
    }
    let Some(content) = m.content.as_deref() else { return false };
    // Success markers emitted by fs_write tools.
    content.starts_with("write_file ok") || content.starts_with("edit_file ok")
}

/// Elide old tool results when context pressure exceeds the threshold.
///
/// - Preserves the `keep_recent` most-recent tool messages verbatim.
/// - Preserves *all* successful write_file/edit_file results verbatim.
/// - Replaces older `role: tool` contents with `[elided: <name> -> N bytes]`.
pub fn elide_old_tool_results(
    messages: &[ChatMessage],
    ctx_limit: usize,
    threshold: f32,
    keep_recent: usize,
) -> Vec<ChatMessage> {
    if pressure(messages, ctx_limit) < threshold {
        return messages.to_vec();
    }

    // Indices of tool messages in order.
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| if m.role == "tool" { Some(i) } else { None })
        .collect();

    if tool_indices.len() <= keep_recent {
        return messages.to_vec();
    }

    let keep_from = tool_indices.len() - keep_recent;
    let to_elide: std::collections::HashSet<usize> = tool_indices[..keep_from]
        .iter()
        .copied()
        .collect();

    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if !to_elide.contains(&i) || is_durable_write_result(m) {
                return m.clone();
            }
            let size = m.content.as_deref().map(|c| c.len()).unwrap_or(0);
            let name = m.name.as_deref().unwrap_or("tool");
            let stub = format!("[elided: {} -> {} bytes]", name, size);
            let mut elided = m.clone();
            elided.content = Some(stub);
            elided
        })
        .collect()
}

/// Convenience wrapper using the config-default threshold and keep_recent.
pub fn maybe_elide(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    elide_old_tool_results(
        messages,
        config::N_CTX,
        config::PRESSURE_THRESHOLD,
        config::KEEP_RECENT_TOOLS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::ChatMessage;

    fn tool_msg(name: &str, content: &str) -> ChatMessage {
        ChatMessage::tool_result("id", name, content)
    }

    #[test]
    fn estimate_tokens_works() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn pressure_zero_when_empty() {
        let m: Vec<ChatMessage> = vec![];
        assert!(pressure(&m, 8192) < 0.001);
    }

    #[test]
    fn no_elision_below_threshold() {
        let msgs = vec![
            ChatMessage::user("hi"),
            tool_msg("read_file", "small"),
        ];
        let out = elide_old_tool_results(&msgs, 8192, 0.7, 4);
        assert_eq!(out.len(), msgs.len());
        assert_eq!(out[1].content.as_deref().unwrap(), "small");
    }

    #[test]
    fn elides_when_above_threshold() {
        let big = "x".repeat(8192 * 4); // fills the ctx
        let mut msgs = vec![ChatMessage::user("hi")];
        for i in 0..10 {
            msgs.push(tool_msg("read_file", &format!("{i}-{big}")));
        }
        let out = elide_old_tool_results(&msgs, 8192, 0.7, 4);
        // Last 4 tool messages preserved, first 6 elided.
        let preserved: Vec<&ChatMessage> = out.iter().filter(|m| {
            m.role == "tool" && !m.content.as_deref().unwrap_or("").starts_with("[elided")
        }).collect();
        assert_eq!(preserved.len(), 4);
    }

    #[test]
    fn preserves_write_results_through_elision() {
        let big = "x".repeat(8192 * 4);
        let mut msgs = vec![ChatMessage::user("hi")];
        // Older write message that would normally be elided.
        msgs.push(tool_msg("write_file", "write_file ok: foo.rs (12 bytes)"));
        for i in 0..10 {
            msgs.push(tool_msg("read_file", &format!("{i}-{big}")));
        }
        let out = elide_old_tool_results(&msgs, 8192, 0.7, 4);
        // The write_file message must survive verbatim.
        let write_msg = out.iter().find(|m| m.name.as_deref() == Some("write_file")).unwrap();
        assert_eq!(write_msg.content.as_deref().unwrap(), "write_file ok: foo.rs (12 bytes)");
    }

    #[test]
    fn keep_recent_respected() {
        let big = "x".repeat(8192 * 4);
        let mut msgs = vec![ChatMessage::user("hi")];
        for i in 0..8 {
            msgs.push(tool_msg("grep", &format!("{i}-{big}")));
        }
        let out = elide_old_tool_results(&msgs, 8192, 0.7, 3);
        let kept = out.iter().filter(|m| {
            m.role == "tool" && !m.content.as_deref().unwrap_or("").starts_with("[elided")
        }).count();
        assert_eq!(kept, 3);
    }
}
