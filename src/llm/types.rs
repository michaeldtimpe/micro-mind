use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Some(text.into()), name: None, tool_call_id: None, tool_calls: vec![] }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Some(text.into()), name: None, tool_call_id: None, tool_calls: vec![] }
    }
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: Some(text.into()), name: None, tool_call_id: None, tool_calls: vec![] }
    }
    pub fn assistant_calls(content: Option<String>, calls: Vec<ToolCall>) -> Self {
        Self { role: "assistant".into(), content, name: None, tool_call_id: None, tool_calls: calls }
    }
    pub fn tool_result(call_id: impl Into<String>, name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            name: Some(name.into()),
            tool_call_id: Some(call_id.into()),
            tool_calls: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn default_tool_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// OpenAI spec: arguments is a *string* of JSON, not a JSON object.
    pub arguments: String,
}

impl ToolCall {
    pub fn parse_arguments(&self) -> Result<Value, serde_json::Error> {
        if self.function.arguments.trim().is_empty() {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        serde_json::from_str(&self.function.arguments)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<&'a [ToolDefWire<'a>]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    pub temperature: f32,
    pub top_p: f32,
    pub repeat_penalty: f32,
    pub seed: u32,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefWire<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: ToolDefInner<'a>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefInner<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub parameters: &'a Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub message: ChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}
