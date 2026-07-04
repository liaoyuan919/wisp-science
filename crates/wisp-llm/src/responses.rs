//! OpenAI first-party Responses API (`/v1/responses`).

use crate::message::{Content, Message, Part, Role, ToolCall, ToolSchema};
use crate::provider::{LlmError, Provider, Result, StreamSink};
use crate::{Completion, FunctionCall, Usage};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct OpenAiResponsesProvider {
    cfg: crate::provider::ProviderConfig,
    client: reqwest::Client,
}

impl OpenAiResponsesProvider {
    pub fn new(cfg: crate::provider::ProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("wisp-science")
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("reqwest client");
        Self { cfg, client }
    }

    fn endpoint(&self) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        if base.ends_with("/responses") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/responses")
        } else {
            format!("{base}/v1/responses")
        }
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::CONTENT_TYPE, reqwest::header::HeaderValue::from_static("application/json"));
        if !self.cfg.api_key.is_empty() {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key)) {
                h.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
        h
    }

    fn build_body(&self, messages: &[Message], tools: &[ToolSchema]) -> Value {
        let input: Vec<Value> = messages.iter().map(message_to_input).collect();
        let mut body = json!({
            "model": self.cfg.model,
            "input": input,
            "max_output_tokens": self.cfg.max_tokens,
        });
        let tools_json: Vec<Value> = tools.iter().map(tool_to_responses).collect();
        if !tools_json.is_empty() {
            body["tools"] = json!(tools_json);
        }
        if let Some(effort) = &self.cfg.reasoning_effort {
            body["reasoning"] = json!({ "effort": effort });
        }
        body
    }

    async fn request(&self, body: Value) -> Result<Value> {
        let resp = self.client.post(self.endpoint()).headers(self.headers()).json(&body).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(LlmError::Api { status, body: text });
        }
        Ok(serde_json::from_str(&text)?)
    }
}

fn message_to_input(m: &Message) -> Value {
    match m.role {
        Role::System => json!({ "role": "system", "content": m.content.as_text() }),
        Role::User => json!({ "role": "user", "content": content_to_responses(&m.content) }),
        Role::Assistant => json!({ "role": "assistant", "content": m.content.as_text() }),
        Role::Tool => json!({
            "type": "function_call_output",
            "call_id": m.tool_call_id.clone().unwrap_or_default(),
            "output": m.content.as_text(),
        }),
    }
}

fn content_to_responses(c: &Content) -> Value {
    match c {
        Content::Text(s) => json!(s),
        Content::Parts(parts) => json!(parts.iter().map(part_to_responses).collect::<Vec<_>>()),
    }
}

fn part_to_responses(p: &Part) -> Value {
    match p {
        Part::Text { text, .. } => json!({ "type": "input_text", "text": text }),
        Part::Image { image_url, .. } => json!({ "type": "input_image", "image_url": image_url.url.clone() }),
    }
}

fn tool_to_responses(t: &ToolSchema) -> Value {
    json!({
        "type": "function",
        "name": t.function.name.clone(),
        "description": t.function.description.clone(),
        "parameters": t.function.parameters.clone(),
    })
}

fn parse_completion(val: &Value) -> Completion {
    let mut content = val.get("output_text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut tool_calls = vec![];

    if let Some(output) = val.get("output").and_then(|v| v.as_array()) {
        for item in output {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("message") => {
                    if content.is_empty() {
                        content.push_str(&message_text(item));
                    }
                }
                Some("function_call") => {
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let arguments = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}").to_string();
                    tool_calls.push(ToolCall { id, kind: "function".into(), function: FunctionCall { name, arguments } });
                }
                _ => {}
            }
        }
    }

    let usage = val.get("usage").map(parse_usage).unwrap_or_default();
    let finish_reason = val.get("status").and_then(|v| v.as_str()).map(String::from);
    Completion { content, reasoning: None, tool_calls, finish_reason, usage }
}

fn message_text(item: &Value) -> String {
    item.get("content")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| {
                    p.get("text")
                        .or_else(|| p.get("output_text"))
                        .and_then(|v| v.as_str())
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn parse_usage(u: &Value) -> Usage {
    Usage {
        input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
    }
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
    fn name(&self) -> &str { "openai-responses" }
    fn model(&self) -> &str { &self.cfg.model }

    async fn complete(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<Completion> {
        let val = self.request(self.build_body(messages, tools)).await?;
        Ok(parse_completion(&val))
    }

    async fn stream(&self, messages: &[Message], tools: &[ToolSchema], sink: &mut dyn StreamSink) -> Result<Completion> {
        let comp = self.complete(messages, tools).await?;
        if !comp.content.is_empty() {
            sink.on_text(&comp.content);
        }
        for (i, tc) in comp.tool_calls.iter().enumerate() {
            sink.on_tool_call(i, &tc.function.name, &tc.function.arguments);
        }
        sink.on_usage(comp.usage.clone());
        Ok(comp)
    }
}
