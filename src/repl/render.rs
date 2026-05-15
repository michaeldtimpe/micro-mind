use serde_json::Value;

use crate::tools::ToolCallResult;

pub fn tool_call_start(name: &str, args: &Value) {
    let preview = preview_args(args);
    println!("▸ {} {}", name, preview);
}

pub fn tool_call_result(call: &ToolCallResult) {
    let status = if call.is_ok() { "ok" } else { "ERROR" };
    let suffix = if call.cached { " (cached)" } else { "" };
    let extra = if call.bytes_out > 0 {
        format!(" {} bytes", call.bytes_out)
    } else {
        String::new()
    };
    println!("  └ {} {}ms{}{}", status, call.wall_ms, extra, suffix);
    if let Some(err) = &call.error {
        println!("    {}", first_line(err));
    }
}

pub fn final_answer(text: &str) {
    println!("\n{}\n", text.trim());
}

pub fn guard(reason: &str) {
    println!("× guard fired: {}", reason);
}

pub fn error(msg: &str) {
    eprintln!("! {}", msg);
}

fn preview_args(args: &Value) -> String {
    match args {
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .take(3)
                .map(|(k, v)| format!("{}={}", k, preview_value(v)))
                .collect();
            parts.join(" ")
        }
        _ => preview_value(args),
    }
}

fn preview_value(v: &Value) -> String {
    match v {
        Value::String(s) => {
            if s.len() > 40 {
                format!("{:?}…", &s[..38])
            } else {
                format!("{:?}", s)
            }
        }
        other => other.to_string(),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}
