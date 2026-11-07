//! Protocol-generic OpenAI Responses wire converters.
//!
//! The canonical Chat request (from `chat_codec::request_for`) becomes a Responses
//! request, upstream Responses events become Chat chunks that `chat_codec::Output`
//! accepts, and a non-streaming result is aggregated from the event stream. Provider
//! specific payload rules (store, instructions, removed fields) are not applied here.

use serde_json::{json, Map, Value};
use std::collections::HashMap;

#[cfg(test)]
#[path = "responses_wire/tests.rs"]
mod tests;

const EMPTY_TOOL_OUTPUT: &str = "(empty)";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The event kind, always read from the JSON `type` (data-only SSE frames carry no
/// `event:` name).
pub fn event_type(data: &Value) -> Option<&str> {
    data.get("type").and_then(Value::as_str)
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

// ---------------------------------------------------------------- request

fn parts_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn message_item(role: &str, part_type: &str, text: &str) -> Value {
    json!({"type":"message","role":role,"content":[{"type":part_type,"text":text}]})
}

fn user_parts(content: &Value) -> Result<Vec<Value>, String> {
    let parts = match content {
        Value::String(s) => return Ok(vec![json!({"type":"input_text","text":s})]),
        Value::Array(parts) => parts,
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for part in parts {
        match str_of(part, "type") {
            "text" | "input_text" => {
                out.push(json!({"type":"input_text","text":str_of(part,"text")}))
            }
            "image_url" | "input_image" => {
                let image = part.get("image_url").unwrap_or(&Value::Null);
                let url = image
                    .as_str()
                    .or_else(|| image.get("url").and_then(Value::as_str))
                    .ok_or("Image part is missing a url")?;
                let mut item = json!({"type":"input_image","image_url":url});
                if let Some(detail) = image.get("detail").filter(|d| d.is_string()) {
                    item["detail"] = detail.clone();
                }
                out.push(item);
            }
            other => return Err(format!("Unsupported content part type: {other}")),
        }
    }
    Ok(out)
}

fn flatten_tool(tool: &Value) -> Value {
    let Some(function) = tool
        .get("function")
        .filter(|_| str_of(tool, "type") == "function")
    else {
        return tool.clone();
    };
    let mut out = Map::new();
    out.insert("type".into(), json!("function"));
    for key in ["name", "description", "parameters", "strict"] {
        if let Some(v) = function.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

fn text_format(format: &Value) -> Option<Value> {
    match str_of(format, "type") {
        "json_schema" => {
            let schema = format.get("json_schema")?;
            let mut out = Map::new();
            out.insert("type".into(), json!("json_schema"));
            for key in ["name", "description", "schema", "strict"] {
                if let Some(v) = schema.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            Some(Value::Object(out))
        }
        kind @ ("json_object" | "text") => Some(json!({"type":kind})),
        _ => None,
    }
}

/// Converts a canonical Chat request into a Responses request.
pub fn from_chat(chat: Value) -> Result<Value, String> {
    let messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("Chat request has no messages")?;
    let mut input = Vec::new();
    for message in messages {
        let content = message.get("content").unwrap_or(&Value::Null);
        match str_of(message, "role") {
            role @ ("system" | "developer") => {
                let text = parts_text(content);
                if !text.is_empty() {
                    input.push(message_item(role, "input_text", &text));
                }
            }
            "user" => {
                let parts = user_parts(content)?;
                if !parts.is_empty() {
                    input.push(json!({"type":"message","role":"user","content":parts}));
                }
            }
            "assistant" => {
                let text = parts_text(content);
                if !text.is_empty() {
                    input.push(message_item("assistant", "output_text", &text));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let function = call.get("function").unwrap_or(&Value::Null);
                    let arguments = Some(str_of(function, "arguments"))
                        .filter(|a| !a.is_empty())
                        .unwrap_or("{}");
                    input.push(json!({
                        "type":"function_call",
                        "call_id":str_of(call,"id"),
                        "name":str_of(function,"name"),
                        "arguments":arguments,
                    }));
                }
            }
            "tool" | "function" => {
                let output = parts_text(content);
                input.push(json!({
                    "type":"function_call_output",
                    "call_id":str_of(message,"tool_call_id"),
                    "output":if output.is_empty() { EMPTY_TOOL_OUTPUT } else { &output },
                }));
            }
            other => return Err(format!("Unsupported message role: {other}")),
        }
    }
    let mut out = Map::new();
    out.insert("input".into(), Value::Array(input));
    for key in [
        "model",
        "stream",
        "parallel_tool_calls",
        "temperature",
        "top_p",
    ] {
        if let Some(v) = chat.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        out.insert(
            "tools".into(),
            Value::Array(tools.iter().map(flatten_tool).collect()),
        );
    }
    if let Some(choice) = chat.get("tool_choice").filter(|v| !v.is_null()) {
        let flat = match choice.get("function").and_then(|f| f.get("name")) {
            Some(name) if str_of(choice, "type") == "function" => {
                json!({"type":"function","name":name})
            }
            _ => choice.clone(),
        };
        out.insert("tool_choice".into(), flat);
    }
    if let Some(effort) = chat.get("reasoning_effort").filter(|v| v.is_string()) {
        out.insert(
            "reasoning".into(),
            json!({"effort":effort,"summary":"auto"}),
        );
    }
    if let Some(format) = chat.get("response_format").and_then(text_format) {
        out.insert("text".into(), json!({"format":format}));
    }
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------- stream

fn is_call_item(item: &Value) -> bool {
    matches!(
        str_of(item, "type"),
        "function_call" | "custom_tool_call" | "mcp_call"
    )
}

fn failure_message(event: &Value) -> String {
    let error = event
        .get("response")
        .and_then(|r| r.get("error"))
        .or_else(|| event.get("error"))
        .filter(|e| !e.is_null());
    let message = error
        .and_then(|e| e.get("message").and_then(Value::as_str))
        .or_else(|| event.get("message").and_then(Value::as_str))
        .filter(|m| !m.is_empty());
    match message {
        Some(m) => m.to_owned(),
        None => "Upstream response failed".to_owned(),
    }
}

fn chat_usage(usage: &Value) -> Option<Value> {
    let input = usage.get("input_tokens").and_then(Value::as_u64)?;
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut out = json!({
        "prompt_tokens":input,
        "completion_tokens":output,
        "total_tokens":input + output,
    });
    if let Some(cached) = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
    {
        out["prompt_tokens_details"] = json!({"cached_tokens":cached});
    }
    Some(out)
}

/// Stateful Responses SSE event to Chat chunk adapter.
pub struct ChatChunks {
    id: String,
    model: String,
    created: u64,
    /// output_index / item id -> chat tool call index
    calls: HashMap<String, usize>,
    next_call: usize,
    streamed_args: HashMap<usize, bool>,
    done: bool,
}

impl Default for ChatChunks {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatChunks {
    pub fn new() -> Self {
        Self {
            id: "chatcmpl-responses".into(),
            model: String::new(),
            created: now(),
            calls: HashMap::new(),
            next_call: 0,
            streamed_args: HashMap::new(),
            done: false,
        }
    }

    fn chunk(&self, delta: Value, finish: Value) -> String {
        json!({
            "id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}],
        })
        .to_string()
    }

    fn remember(&mut self, response: &Value) {
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            self.id = id.to_owned();
        }
        if let Some(model) = response.get("model").and_then(Value::as_str) {
            self.model = model.to_owned();
        }
    }

    fn call_index(&self, event: &Value) -> Option<usize> {
        let by_output = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|i| self.calls.get(&format!("#{i}")));
        by_output
            .or_else(|| self.calls.get(str_of(event, "item_id")))
            .copied()
    }

    fn tool_delta(index: usize, arguments: &str) -> Value {
        json!({"tool_calls":[{"index":index,"function":{"arguments":arguments}}]})
    }

    pub fn push(&mut self, event: &Value) -> Result<Vec<String>, String> {
        if self.done {
            return Ok(Vec::new());
        }
        let kind = event_type(event).unwrap_or("");
        let mut out = Vec::new();
        match kind {
            "response.created" | "response.in_progress" => {
                if let Some(response) = event.get("response") {
                    self.remember(response);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let delta = str_of(event, "delta");
                if !delta.is_empty() {
                    out.push(self.chunk(json!({"content":delta}), Value::Null));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let delta = str_of(event, "delta");
                if !delta.is_empty() {
                    out.push(self.chunk(json!({"reasoning_content":delta}), Value::Null));
                }
            }
            "response.output_item.added" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                if is_call_item(item) {
                    let index = self.next_call;
                    self.next_call += 1;
                    if let Some(i) = event.get("output_index").and_then(Value::as_u64) {
                        self.calls.insert(format!("#{i}"), index);
                    }
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        self.calls.insert(id.to_owned(), index);
                    }
                    let call_id = Some(str_of(item, "call_id"))
                        .filter(|c| !c.is_empty())
                        .unwrap_or_else(|| str_of(item, "id"));
                    let name = str_of(item, "name");
                    let initial = str_of(item, "arguments");
                    if !initial.is_empty() {
                        self.streamed_args.insert(index, true);
                    }
                    out.push(self.chunk(
                        json!({"tool_calls":[{"index":index,"id":call_id,"type":"function","function":{"name":name,"arguments":initial}}]}),
                        Value::Null,
                    ));
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let delta = str_of(event, "delta");
                if let (Some(index), false) = (self.call_index(event), delta.is_empty()) {
                    self.streamed_args.insert(index, true);
                    out.push(self.chunk(Self::tool_delta(index, delta), Value::Null));
                }
            }
            "response.output_item.done" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                if let (true, Some(index)) = (is_call_item(item), self.call_index(event)) {
                    let args = Some(str_of(item, "arguments"))
                        .filter(|a| !a.is_empty())
                        .unwrap_or_else(|| str_of(item, "input"));
                    if !args.is_empty() && !self.streamed_args.contains_key(&index) {
                        self.streamed_args.insert(index, true);
                        out.push(self.chunk(Self::tool_delta(index, args), Value::Null));
                    }
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                let response = event.get("response").unwrap_or(&Value::Null);
                self.remember(response);
                let reason = response
                    .get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let finish = if self.next_call > 0 {
                    "tool_calls"
                } else if kind == "response.incomplete" && reason == "max_output_tokens" {
                    "length"
                } else if kind == "response.incomplete" && reason == "content_filter" {
                    "content_filter"
                } else {
                    "stop"
                };
                out.push(self.chunk(json!({}), json!(finish)));
                if let Some(usage) = response.get("usage").and_then(chat_usage) {
                    out.push(
                        json!({
                            "id":self.id,"object":"chat.completion.chunk","created":self.created,
                            "model":self.model,"choices":[],"usage":usage,
                        })
                        .to_string(),
                    );
                }
                out.push("[DONE]".into());
                self.done = true;
            }
            "response.failed" | "error" => {
                self.done = true;
                return Err(failure_message(event));
            }
            "response.cancelled" => {
                self.done = true;
                return Err("Upstream response was cancelled".into());
            }
            _ => {}
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- aggregate

fn rebuild_from_deltas(events: &[Value]) -> Vec<Value> {
    let (mut reasoning, mut text) = (String::new(), String::new());
    let mut calls: Vec<Value> = Vec::new();
    for event in events {
        match event_type(event).unwrap_or("") {
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                reasoning.push_str(str_of(event, "delta"))
            }
            "response.output_text.delta" => text.push_str(str_of(event, "delta")),
            "response.output_item.added" => {
                if let Some(item) = event.get("item").filter(|i| is_call_item(i)) {
                    let mut item = item.clone();
                    item["arguments"] = json!(str_of(&item, "arguments"));
                    calls.push(item);
                }
            }
            "response.function_call_arguments.delta" => {
                let id = str_of(event, "item_id");
                let position = calls
                    .iter()
                    .position(|c| str_of(c, "id") == id)
                    .or(calls.len().checked_sub(1));
                if let Some(call) = position.map(|i| &mut calls[i]) {
                    let joined = format!("{}{}", str_of(call, "arguments"), str_of(event, "delta"));
                    call["arguments"] = json!(joined);
                }
            }
            _ => {}
        }
    }
    let mut output = Vec::new();
    if !reasoning.is_empty() {
        output
            .push(json!({"type":"reasoning","summary":[{"type":"summary_text","text":reasoning}]}));
    }
    if !text.is_empty() {
        output.push(json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}));
    }
    output.extend(calls);
    output
}

/// Builds the non-streaming result from the complete event list.
pub fn aggregate(events: &[Value]) -> Result<Value, String> {
    let mut terminal: Option<&Value> = None;
    for event in events {
        match event_type(event).unwrap_or("") {
            "response.completed" | "response.done" => {
                terminal = Some(event);
                break;
            }
            "response.incomplete" | "response.cancelled" => {
                terminal.get_or_insert(event);
            }
            "response.failed" | "error" if terminal.is_none() => {
                return Err(failure_message(event));
            }
            _ => {}
        }
    }
    let terminal = terminal.ok_or("Upstream stream ended without a final response")?;
    let mut response = terminal.get("response").cloned().unwrap_or(json!({}));
    if !response.is_object() {
        response = json!({});
    }
    let empty = response
        .get("output")
        .and_then(Value::as_array)
        .is_none_or(|o| o.is_empty());
    if empty {
        let mut seen = std::collections::HashSet::new();
        let mut items: Vec<Value> = Vec::new();
        for event in events {
            if event_type(event) != Some("response.output_item.done") {
                continue;
            }
            let Some(item) = event.get("item").filter(|i| i.is_object()) else {
                continue;
            };
            let id = str_of(item, "id");
            if id.is_empty() || seen.insert(id.to_owned()) {
                items.push(item.clone());
            }
        }
        if items.is_empty() {
            items = rebuild_from_deltas(events);
        }
        response["output"] = Value::Array(items);
    }
    if response.get("created_at").is_none_or(Value::is_null) {
        response["created_at"] = json!(now());
    }
    Ok(response)
}

/// Whether the event is client-visible output (the Responses analogue of the Chat check).
pub fn carries_output(event: &Value) -> bool {
    match event_type(event).unwrap_or("") {
        "response.output_text.delta"
        | "response.refusal.delta"
        | "response.reasoning_summary_text.delta"
        | "response.reasoning_text.delta"
        | "response.function_call_arguments.delta"
        | "response.custom_tool_call_input.delta" => true,
        "response.output_item.added" => event
            .get("item")
            .is_some_and(|i| !i.is_null() && str_of(i, "type") != "message"),
        "response.completed" | "response.done" | "response.incomplete" => event
            .get("response")
            .and_then(|r| r.get("output"))
            .and_then(Value::as_array)
            .is_some_and(|o| !o.is_empty()),
        _ => false,
    }
}
