//! Codex request rules applied to any Responses body before it is sent upstream.
use crate::modules::native_providers::http::Failure;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub const DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";
const MAX_CALL_ID: usize = 64;
const UNSUPPORTED: [&str; 15] = [
    "max_output_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "chat_template_kwargs",
    "user",
    "metadata",
    "prompt_cache_retention",
    "safety_identifier",
    "stream_options",
    "truncation",
    "stop_sequences",
    "background",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Http,
    Compact,
    WebSocket,
}

pub fn prepare(mut body: Value, model: &str, mode: Mode) -> Result<Value, Failure> {
    let Some(root) = body.as_object_mut() else {
        return Err(Failure::new(400, "Request body must be a JSON object"));
    };
    root.insert("model".into(), json!(model));
    for key in UNSUPPORTED {
        root.remove(key);
    }
    if mode == Mode::Compact {
        root.remove("stream");
        root.remove("store");
    } else {
        root.insert("stream".into(), json!(true));
        root.insert("store".into(), json!(false));
    }
    if mode != Mode::WebSocket {
        root.remove("previous_response_id");
    }
    if let Some(text) = root.get("input").and_then(Value::as_str) {
        let input = if text.is_empty() {
            json!([])
        } else {
            json!([{"type": "message", "role": "user", "content": text}])
        };
        root.insert("input".into(), input);
    }
    let system = take_system_messages(root);
    set_instructions(root, system);
    // An incremental WebSocket turn continues server-side state: its tool outputs answer calls
    // the upstream already holds and its item references must stay intact.
    let incremental = mode == Mode::WebSocket && root.contains_key("previous_response_id");
    if let Some(items) = root.get_mut("input").and_then(Value::as_array_mut) {
        normalize_items(items, incremental);
    }
    normalize_reasoning(root);
    flatten_tools(root);
    declare_web_search(root);
    Ok(body)
}

fn content_text(content: &Value) -> String {
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

fn take_system_messages(root: &mut Map<String, Value>) -> Vec<String> {
    let Some(items) = root.get_mut("input").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let is_system = |i: &Value| {
        i.get("role").and_then(Value::as_str) == Some("system")
            && i.get("type").and_then(Value::as_str).unwrap_or("message") == "message"
    };
    let texts = items
        .iter()
        .filter(|i| is_system(i))
        .map(|i| content_text(&i["content"]))
        .filter(|t| !t.trim().is_empty())
        .collect();
    items.retain(|i| !is_system(i));
    texts
}

fn set_instructions(root: &mut Map<String, Value>, mut parts: Vec<String>) {
    if let Some(existing) = root.get("instructions").and_then(Value::as_str) {
        if !existing.trim().is_empty() {
            parts.push(existing.to_string());
        }
    }
    let instructions = if parts.is_empty() {
        DEFAULT_INSTRUCTIONS.to_string()
    } else {
        parts.join("\n\n")
    };
    root.insert("instructions".into(), json!(instructions));
}

fn shorten_call_id(id: &str) -> String {
    let digest: String = Sha256::digest(id.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("call_{}", &digest[..MAX_CALL_ID - "call_".len()])
}

fn normalize_items(items: &mut Vec<Value>, incremental: bool) {
    let mut renamed: HashMap<String, String> = HashMap::new();
    for item in items.iter_mut().filter_map(Value::as_object_mut) {
        let reasoning = item.get("type").and_then(Value::as_str) == Some("reasoning");
        if reasoning || !incremental {
            item.remove("id");
        }
        if reasoning {
            if !item.get("summary").is_some_and(Value::is_array) {
                item.insert("summary".into(), json!([]));
            }
        }
        if let Some(Value::String(id)) = item.get_mut("call_id") {
            if id.len() > MAX_CALL_ID {
                let short = renamed
                    .entry(id.clone())
                    .or_insert_with(|| shorten_call_id(id))
                    .clone();
                *id = short;
            }
        }
    }
    if incremental {
        return;
    }
    let calls: HashSet<String> = items
        .iter()
        .filter(|i| {
            i.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with("_call"))
        })
        .filter_map(|i| i.get("call_id")?.as_str().map(String::from))
        .collect();
    items.retain(|i| {
        i.get("type").and_then(Value::as_str) != Some("function_call_output")
            || i.get("call_id")
                .and_then(Value::as_str)
                .is_some_and(|id| calls.contains(id))
    });
}

fn normalize_reasoning(root: &mut Map<String, Value>) {
    let Some(reasoning) = root.get_mut("reasoning").filter(|r| r.is_object()) else {
        return;
    };
    if reasoning["effort"] == "minimal" {
        reasoning["effort"] = json!("none");
    }
    const ENCRYPTED: &str = "reasoning.encrypted_content";
    let include = root.entry("include").or_insert_with(|| json!([]));
    if !include.is_array() {
        *include = json!([]);
    }
    let list = include.as_array_mut().expect("include is an array");
    if !list.iter().any(|v| v == ENCRYPTED) {
        list.push(json!(ENCRYPTED));
    }
}

fn flatten_tools(root: &mut Map<String, Value>) {
    let Some(tools) = root.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        if let Some(Value::Object(function)) = tool.remove("function") {
            for (key, value) in function {
                tool.entry(key).or_insert(value);
            }
        }
    }
}

fn declare_web_search(root: &mut Map<String, Value>) {
    let replayed = root
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|i| i.get("type").and_then(Value::as_str) == Some("web_search_call"))
        });
    if !replayed {
        return;
    }
    let tools = root.entry("tools").or_insert_with(|| json!([]));
    let Some(list) = tools.as_array_mut() else {
        return;
    };
    let declared = list.iter().any(|t| {
        t.get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("web_search"))
    });
    if declared {
        return;
    }
    let no_tools = list.is_empty();
    list.push(json!({"type": "web_search", "external_web_access": false}));
    if no_tools {
        root.insert("tool_choice".into(), json!("none"));
    }
}
