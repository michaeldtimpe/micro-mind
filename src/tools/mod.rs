pub mod cache;
pub mod fs_read;
pub mod fs_utils;
pub mod fs_write;
pub mod shell;

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::config;

/// In-process tool function: takes JSON args, returns either a string result or an error string.
pub type ToolFn = Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>;

/// A registered tool: its name, human-readable description, JSON-Schema parameters, and function.
#[derive(Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub function: ToolFn,
    pub cacheable: bool,
}

impl ToolDef {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        function: impl Fn(&Value) -> Result<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            function: Arc::new(function),
            cacheable: false,
        }
    }

    pub fn cacheable(mut self) -> Self {
        self.cacheable = true;
        self
    }
}

/// Outcome of a single tool dispatch.
#[derive(Clone, Debug)]
pub struct ToolCallResult {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub result: String,
    pub error: Option<String>,
    pub wall_ms: u64,
    pub bytes_out: usize,
    pub cached: bool,
}

impl ToolCallResult {
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Validate args against a tool's JSON-Schema parameters.
/// Lightweight: checks `required` and primitive types only — same shape as luxe's `validate_args`.
pub fn validate_args(params: &Value, args: &Value) -> Option<String> {
    let Some(args_obj) = args.as_object() else {
        return Some(format!(
            "Arguments must be a JSON object, got {}",
            type_name(args)
        ));
    };

    if let Some(required) = params.get("required").and_then(|v| v.as_array()) {
        for r in required {
            if let Some(name) = r.as_str()
                && !args_obj.contains_key(name)
            {
                return Some(format!("Missing required argument: {}", name));
            }
        }
    }

    if let Some(props) = params.get("properties").and_then(|v| v.as_object()) {
        for (k, v) in args_obj {
            if let Some(prop_spec) = props.get(k)
                && let Some(expected) = prop_spec.get("type").and_then(|t| t.as_str())
            {
                let actual = type_name(v);
                let ok = match expected {
                    "string" => v.is_string(),
                    "integer" => v.is_i64() || v.is_u64(),
                    "number" => v.is_number(),
                    "boolean" => v.is_boolean(),
                    "array" => v.is_array(),
                    "object" => v.is_object(),
                    _ => true,
                };
                if !ok {
                    return Some(format!(
                        "Argument '{}' should be {}, got {}",
                        k, expected, actual
                    ));
                }
            }
        }
    }

    None
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Apply the hard output truncation at the tool layer.
///
/// Every tool result that reaches the agent loop has already been capped here.
/// Soft elision later in the loop is layered on top.
pub fn hard_truncate(s: &str) -> String {
    let cap = config::TOOL_OUTPUT_HARD_CAP;
    if s.len() <= cap {
        return s.to_string();
    }
    let mut out = s.as_bytes()[..cap].to_vec();
    // Truncate cleanly at a UTF-8 boundary.
    while std::str::from_utf8(&out).is_err() && !out.is_empty() {
        out.pop();
    }
    let mut text = String::from_utf8_lossy(&out).to_string();
    let dropped = s.len() - text.len();
    text.push_str(&format!(
        "\n[truncated: {} more bytes. Use grep / offset / max_bytes for more.]",
        dropped
    ));
    text
}

/// Dispatch a single tool by name.
///
/// - Strips trailing whitespace from the name (small-model lesson from luxe).
/// - Validates args against the tool's parameter schema.
/// - Routes through the cache for cacheable, read-only tools.
/// - Captures any panic-equivalent error and surfaces it to the agent as a recoverable
///   tool error rather than crashing the harness.
pub fn dispatch(
    name_raw: &str,
    arguments: &Value,
    tool_id: &str,
    tools: &HashMap<String, ToolDef>,
    cache: &mut cache::ToolCache,
) -> ToolCallResult {
    let name = name_raw.trim().to_string();
    let start = Instant::now();

    let mut tc = ToolCallResult {
        id: tool_id.to_string(),
        name: name.clone(),
        arguments: arguments.clone(),
        result: String::new(),
        error: None,
        wall_ms: 0,
        bytes_out: 0,
        cached: false,
    };

    let Some(def) = tools.get(&name) else {
        tc.error = Some(format!("Unknown tool: {}", name));
        tc.wall_ms = start.elapsed().as_millis() as u64;
        return tc;
    };

    if let Some(err) = validate_args(&def.parameters, arguments) {
        tc.error = Some(err);
        tc.wall_ms = start.elapsed().as_millis() as u64;
        return tc;
    }

    let (raw_result, raw_err, cached) = if def.cacheable {
        let (res, err, hit) = cache.get_or_run(&name, arguments, def.function.clone());
        (res, err, hit)
    } else {
        match (def.function)(arguments) {
            Ok(s) => (s, None, false),
            Err(e) => (String::new(), Some(e), false),
        }
    };

    let truncated = hard_truncate(&raw_result);
    tc.bytes_out = truncated.len();
    tc.result = truncated;
    tc.error = raw_err;
    tc.cached = cached;
    tc.wall_ms = start.elapsed().as_millis() as u64;
    tc
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry() -> (HashMap<String, ToolDef>, cache::ToolCache) {
        let mut m = HashMap::new();
        let echo = ToolDef::new(
            "echo",
            "echo back the message",
            json!({
                "type": "object",
                "properties": { "msg": {"type": "string"} },
                "required": ["msg"]
            }),
            |args| Ok(args["msg"].as_str().unwrap_or("").to_string()),
        );
        m.insert("echo".into(), echo);
        (m, cache::ToolCache::new())
    }

    #[test]
    fn dispatch_unknown_returns_error() {
        let (tools, mut cache) = registry();
        let tc = dispatch("nope", &json!({}), "1", &tools, &mut cache);
        assert!(!tc.is_ok());
        assert!(tc.error.unwrap().contains("Unknown tool"));
    }

    #[test]
    fn dispatch_strips_trailing_whitespace() {
        let (tools, mut cache) = registry();
        let tc = dispatch("echo\n", &json!({"msg": "hi"}), "1", &tools, &mut cache);
        assert!(tc.is_ok(), "got {:?}", tc.error);
        assert_eq!(tc.result, "hi");
    }

    #[test]
    fn validate_args_missing_required() {
        let p = json!({
            "type": "object",
            "properties": { "x": {"type": "string"} },
            "required": ["x"]
        });
        assert!(validate_args(&p, &json!({})).is_some());
    }

    #[test]
    fn validate_args_wrong_primitive() {
        let p = json!({
            "type": "object",
            "properties": { "x": {"type": "string"} }
        });
        let err = validate_args(&p, &json!({"x": 7}));
        assert!(err.unwrap().contains("should be string"));
    }

    #[test]
    fn validate_args_non_object_args() {
        let p = json!({});
        assert!(validate_args(&p, &json!([1, 2, 3])).is_some());
    }

    #[test]
    fn hard_truncate_under_cap_passthrough() {
        let s = "hello";
        assert_eq!(hard_truncate(s), "hello");
    }

    #[test]
    fn hard_truncate_over_cap_marks() {
        let s = "x".repeat(config::TOOL_OUTPUT_HARD_CAP + 100);
        let t = hard_truncate(&s);
        assert!(t.contains("[truncated:"));
        assert!(t.len() < s.len() + 100);
    }
}
