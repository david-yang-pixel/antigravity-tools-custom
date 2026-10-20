//! Protocol conversion at the native provider boundary; billing remains in shared middleware.
use crate::proxy::pipeline::usage::CanonicalUsage;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Chat,
    Responses,
    Messages,
}
impl Protocol {
    pub fn for_path(path: &str) -> Option<Self> {
        match path {
            "/v1/chat/completions" => Some(Self::Chat),
            "/v1/responses" | "/responses" => Some(Self::Responses),
            "/v1/messages" => Some(Self::Messages),
            _ => None,
        }
    }
}
fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into()
}
fn array<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    v.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{key} must be an array"))
}
fn required(v: &Value, key: &str) -> Result<String, String> {
    let s = text(v, key);
    if s.is_empty() {
        Err(format!("{key} is required"))
    } else {
        Ok(s)
    }
}
fn reject(v: &Value, keys: &[&str]) -> Result<(), String> {
    for key in keys {
        if v.get(*key).is_some_and(|x| !x.is_null() && x != false) {
            return Err(format!("WorkBuddy does not support {key}; send complete conversation history and function tools"));
        }
    }
    Ok(())
}
fn response_content(value: &Value) -> Result<Value, String> {
    if value.is_string() {
        return Ok(value.clone());
    }
    let mut result = Vec::new();
    for part in value
        .as_array()
        .ok_or("Responses message content must be text or an array")?
    {
        result.push(match part["type"].as_str().unwrap_or("") {
            "input_text" | "output_text" | "text" => json!({"type":"text","text":required(part,"text")?}),
            "input_image" => json!({"type":"image_url","image_url":{"url":required(part,"image_url")?,"detail":part.get("detail").cloned().unwrap_or(json!("auto"))}}),
            _ => return Err("Unsupported WorkBuddy Responses content; use text or image URLs".into()),
        });
    }
    Ok(json!(result))
}
fn anthropic_content(part: &Value) -> Result<Value, String> {
    match part["type"].as_str().unwrap_or("") {
        "text" => Ok(json!({"type":"text","text":part["text"]})),
        "image" => {
            let source = &part["source"];
            let url = match source["type"].as_str().unwrap_or("") {
                "url" => required(source, "url")?,
                "base64" => format!(
                    "data:{};base64,{}",
                    required(source, "media_type")?,
                    required(source, "data")?
                ),
                _ => return Err("Unsupported WorkBuddy image source".into()),
            };
            Ok(json!({"type":"image_url","image_url":{"url":url}}))
        }
        _ => Err(
            "Unsupported WorkBuddy Messages content; use text, images and function tools".into(),
        ),
    }
}

pub fn request(protocol: Protocol, body: Value) -> Result<Value, String> {
    if !body.is_object() {
        return Err("Expected a JSON object".into());
    }
    if body.get("n").is_some_and(|n| n.as_u64() != Some(1)) {
        return Err("WorkBuddy supports n=1 only".into());
    }
    reject(
        &body,
        &[
            "audio",
            "modalities",
            "response_format",
            "functions",
            "function_call",
        ],
    )?;
    if protocol == Protocol::Chat {
        if array(&body, "messages")?.is_empty() {
            return Err("messages cannot be empty".into());
        }
        // Native WorkBuddy names a forced tool by string only; reject other object
        // choices here so they surface as request errors instead of account failures.
        if body.get("tool_choice").is_some_and(|c| {
            c.is_object() && c.pointer("/function/name").is_none_or(|n| !n.is_string())
        }) {
            return Err("Unsupported WorkBuddy tool choice".into());
        }
        return Ok(body);
    }
    let mut out = json!({});
    for key in [
        "model",
        "stream",
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "max_tokens",
        "stop",
    ] {
        if let Some(v) = body.get(key) {
            out[key] = v.clone();
        }
    }
    let mut messages = Vec::new();
    if protocol == Protocol::Responses {
        reject(
            &body,
            &["previous_response_id", "conversation", "background"],
        )?;
        if body
            .get("text")
            .and_then(|v| v.get("format"))
            .is_some_and(|f| f["type"] != "text")
        {
            return Err("WorkBuddy does not support structured Responses text formats".into());
        }
        if let Some(instructions) = body.get("instructions").filter(|v| !v.is_null()) {
            messages.push(json!({"role":"system","content":instructions}));
        }
        match body.get("input") {
            Some(Value::String(input)) => messages.push(json!({"role":"user","content":input})),
            Some(Value::Array(items)) => {
                for item in items {
                    match item.get("type").and_then(Value::as_str).unwrap_or("message") {
                    "message" => messages.push(json!({"role":item.get("role").cloned().unwrap_or(json!("user")),"content":response_content(&item["content"])?})),
                    "function_call" => messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":required(item,"call_id")?,"type":"function","function":{"name":call_name(item)?,"arguments":required(item,"arguments")?}}]})),
                    // Custom (freeform) tool calls travel as function calls with a single `input` string argument.
                    "custom_tool_call" => messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":required(item,"call_id")?,"type":"function","function":{"name":call_name(item)?,"arguments":json!({"input":text(item,"input")}).to_string()}}]})),
                    "function_call_output" | "custom_tool_call_output" => messages.push(json!({"role":"tool","tool_call_id":required(item,"call_id")?,"content": if item["output"].is_string() { item["output"].clone() } else { response_content(&item["output"])? }})),
                    "reasoning" => { /* Reasoning summaries are metadata, not user instructions. */ }
                    _ if drop_hosted_web_search(item, "input item") => {}
                    _ => return Err("Unsupported WorkBuddy Responses input item; provide message or function call history".into()),
                }
                }
            }
            _ => return Err("Responses input must be text or an array".into()),
        }
        if let Some(max) = body.get("max_output_tokens") {
            out["max_tokens"] = max.clone();
        }
        if let Some(effort) = body.pointer("/reasoning/effort") {
            out["reasoning_effort"] = effort.clone();
        }
        if let Some(tools) = body.get("tools") {
            let mut converted = Vec::new();
            for tool in tools.as_array().ok_or("tools must be an array")? {
                if drop_hosted_web_search(tool, "tool") {
                    continue;
                }
                if tool["type"] == "namespace" {
                    // Native tools are flat; the qualified name maps back in `Output`.
                    let namespace = required(tool, "name")?;
                    for child in array(tool, "tools")? {
                        let mut child = child.clone();
                        child["name"] =
                            json!(namespaced_name(&namespace, &required(&child, "name")?));
                        converted.push(responses_tool(&child)?);
                    }
                    continue;
                }
                converted.push(responses_tool(tool)?);
            }
            if !converted.is_empty() {
                out["tools"] = json!(converted);
            }
        }
        if let Some(choice) = body.get("tool_choice") {
            out["tool_choice"] = if choice.is_string() {
                choice.clone()
            } else if choice["type"] == "function" || choice["type"] == "custom" {
                json!({"type":"function","function":{"name":call_name(choice)?}})
            } else if drop_hosted_web_search(choice, "tool_choice") {
                json!("auto")
            } else {
                return Err("Unsupported WorkBuddy tool choice".into());
            };
        }
    } else {
        if let Some(system) = body.get("system") {
            let content = if system.is_string() {
                system.clone()
            } else {
                json!(system
                    .as_array()
                    .ok_or("system must be text or blocks")?
                    .iter()
                    .map(anthropic_content)
                    .collect::<Result<Vec<_>, _>>()?)
            };
            messages.push(json!({"role":"system","content":content}));
        }
        for message in array(&body, "messages")? {
            let role = required(message, "role")?;
            if message["content"].is_string() {
                messages.push(json!({"role":role,"content":message["content"]}));
                continue;
            }
            let mut content = Vec::new();
            let mut tools = Vec::new();
            let mut reasoning = String::new();
            for part in message["content"]
                .as_array()
                .ok_or("message content must be text or blocks")?
            {
                match part["type"].as_str().unwrap_or("") {
                    "tool_use" => tools.push(json!({"id":required(part,"id")?,"type":"function","function":{"name":required(part,"name")?,"arguments":part["input"].to_string()}})),
                    "tool_result" => {
                        if !content.is_empty() { messages.push(json!({"role":role,"content":std::mem::take(&mut content)})); }
                        let result = part.get("content").cloned().unwrap_or(json!(""));
                        let result = if result.is_string() { result } else { json!(result.as_array().ok_or("tool result must be text or blocks")?.iter().map(anthropic_content).collect::<Result<Vec<_>,_>>()?) };
                        messages.push(json!({"role":"tool","tool_call_id":required(part,"tool_use_id")?,"content":result}));
                    }
                    "thinking" => reasoning.push_str(&text(part,"thinking")),
                    "redacted_thinking" => {},
                    _ => content.push(anthropic_content(part)?),
                }
            }
            if !content.is_empty() || !tools.is_empty() || !reasoning.is_empty() {
                let mut message = json!({"role":role,"content":content});
                if !tools.is_empty() {
                    message["tool_calls"] = json!(tools);
                }
                if !reasoning.is_empty() {
                    message["reasoning_content"] = json!(reasoning);
                }
                messages.push(message);
            }
        }
        if let Some(stop) = body.get("stop_sequences") {
            out["stop"] = stop.clone();
        }
        if let Some(thinking) = body.get("thinking") {
            out["thinking"] = thinking.clone();
        }
        if let Some(tools) = body.get("tools") {
            out["tools"] = json!(tools.as_array().ok_or("tools must be an array")?.iter().map(|tool| {
                if tool.get("type").is_some_and(|v| v != "custom") { return Err("WorkBuddy supports function tools only".to_owned()); }
                Ok(json!({"type":"function","function":{"name":required(tool,"name")?,"description":tool.get("description").cloned().unwrap_or(json!("")),"parameters":tool["input_schema"]}}))
            }).collect::<Result<Vec<_>,String>>()?);
        }
        if let Some(choice) = body.get("tool_choice") {
            out["tool_choice"] = match choice["type"].as_str().unwrap_or("") {
                "auto" => json!("auto"),
                "any" => json!("required"),
                "none" => json!("none"),
                "tool" => json!({"type":"function","function":{"name":required(choice,"name")?}}),
                _ => return Err("Unsupported WorkBuddy tool choice".into()),
            };
            if let Some(disabled) = choice
                .get("disable_parallel_tool_use")
                .and_then(Value::as_bool)
            {
                out["parallel_tool_calls"] = json!(!disabled);
            }
        }
    }
    if messages.is_empty() {
        return Err("Conversation cannot be empty".into());
    }
    out["messages"] = json!(messages);
    Ok(out)
}

/// WorkBuddy upstreams have no hosted web search, so Responses `web_search*`
/// tools (and their replayed calls) are dropped instead of failing the request.
fn drop_hosted_web_search(value: &Value, kind: &str) -> bool {
    let kind_name = value["type"].as_str().unwrap_or("");
    let dropped = kind_name.starts_with("web_search");
    if dropped {
        tracing::warn!("[WorkBuddy] Dropping unsupported hosted {kind} `{kind_name}`");
    }
    dropped
}

/// One Responses tool as a native function tool.
fn responses_tool(tool: &Value) -> Result<Value, String> {
    if tool["type"] == "custom" {
        return Ok(custom_tool_as_function(tool));
    }
    if tool["type"] != "function" {
        return Err("WorkBuddy supports function, custom and namespace tools only".to_owned());
    }
    let mut function = tool.clone();
    function.as_object_mut().unwrap().remove("type");
    Ok(json!({"type":"function","function":function}))
}

/// Gateway-wide flat name for a namespaced Responses tool (see the OpenAI mapper).
fn namespaced_name(namespace: &str, name: &str) -> String {
    crate::proxy::mappers::openai::request::qualify_namespace_tool_name(namespace, name)
}

/// Name a replayed call or forced choice used upstream: namespaced tools travel flat.
fn call_name(item: &Value) -> Result<String, String> {
    let name = required(item, "name")?;
    Ok(
        match item
            .get("namespace")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        {
            Some(namespace) => namespaced_name(namespace, &name),
            None => name,
        },
    )
}

/// Flat upstream name -> (namespace, client name) for every namespaced Responses
/// tool, so tool calls return in the form the client registered.
pub type Namespaced = HashMap<String, (String, String)>;

pub fn namespaced_tools(body: &Value) -> Namespaced {
    let mut map = Namespaced::new();
    for tool in body["tools"].as_array().into_iter().flatten() {
        let Some(namespace) = tool["name"]
            .as_str()
            .filter(|_| tool["type"] == "namespace")
        else {
            continue;
        };
        for child in tool["tools"].as_array().into_iter().flatten() {
            if let Some(name) = child["name"].as_str() {
                map.insert(
                    namespaced_name(namespace, name),
                    (namespace.into(), name.into()),
                );
            }
        }
    }
    map
}

/// Maps a Responses `custom` (freeform) tool onto a function tool whose only
/// argument is the raw `input` string, matching the gateway-wide convention.
fn custom_tool_as_function(tool: &Value) -> Value {
    let mut input = json!({"type":"string","description":"Raw freeform input for this tool."});
    if let Some(definition) = tool.pointer("/format/definition").and_then(Value::as_str) {
        let syntax = tool
            .pointer("/format/syntax")
            .and_then(Value::as_str)
            .unwrap_or("grammar");
        input["description"] = json!(format!("Raw freeform input for this tool; it must follow this {syntax} definition:\n{definition}"));
    }
    json!({"type":"function","function":{
        "name": tool["name"],
        "description": tool.get("description").cloned().unwrap_or(json!("")),
        "parameters": {"type":"object","properties":{"input":input},"required":["input"]},
    }})
}

#[derive(Default)]
struct Tool {
    id: String,
    name: String,
    arguments: String,
}
pub struct Output {
    protocol: Protocol,
    alias: String,
    id: String,
    created: i64,
    text: String,
    reasoning: String,
    tools: BTreeMap<u64, Tool>,
    finish: Option<String>,
    usage: Option<Value>,
    size: usize,
    sequence: u64,
    slots: BTreeMap<String, usize>,
    blocks: Vec<Value>,
    active_block: Option<(String, usize)>,
    namespaced: Namespaced,
}
fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap_or("message")
    )
}
impl Output {
    pub fn new(protocol: Protocol, alias: &str) -> Self {
        Self {
            protocol,
            alias: alias.into(),
            id: uuid::Uuid::new_v4().simple().to_string(),
            created: chrono::Utc::now().timestamp(),
            text: String::new(),
            reasoning: String::new(),
            tools: BTreeMap::new(),
            finish: None,
            usage: None,
            size: 0,
            sequence: 0,
            slots: BTreeMap::new(),
            blocks: Vec::new(),
            active_block: None,
            namespaced: Namespaced::new(),
        }
    }
    /// Restore namespaced Responses tool calls from their flat upstream names.
    pub fn with_namespaced(mut self, namespaced: Namespaced) -> Self {
        self.namespaced = namespaced;
        self
    }
    fn response_event(&mut self, mut value: Value) -> String {
        value["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        event(value)
    }
    pub fn has_output(&self) -> bool {
        !self.text.is_empty()
            || !self.reasoning.is_empty()
            || !self.tools.is_empty()
            || self.finish.is_some()
    }
    pub fn start(&mut self) -> Vec<String> {
        match self.protocol {
            Protocol::Chat => vec![],
            Protocol::Responses => {
                let mut response = self.response();
                response["status"] = json!("in_progress");
                response["usage"] = Value::Null;
                vec![
                    self.response_event(json!({"type":"response.created","response":response})),
                    self.response_event(json!({"type":"response.in_progress","response":response})),
                ]
            }
            Protocol::Messages => vec![event(
                json!({"type":"message_start","message":{"id":format!("msg_{}",self.id),"type":"message","role":"assistant","model":self.alias,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}),
            )],
        }
    }
    fn response_slot(&mut self, key: &str, item: Value, events: &mut Vec<String>) -> usize {
        if let Some(index) = self.slots.get(key) {
            return *index;
        }
        let index = self.blocks.len();
        self.blocks.push(item.clone());
        self.slots.insert(key.into(), index);
        events.push(self.response_event(
            json!({"type":"response.output_item.added","output_index":index,"item":item}),
        ));
        index
    }
    fn message_delta(&mut self, kind: &str, value: &str, events: &mut Vec<String>) {
        if self.active_block.as_ref().map(|b| b.0.as_str()) != Some(kind) {
            if let Some((_, index)) = self.active_block.take() {
                events.push(event(json!({"type":"content_block_stop","index":index})));
            }
            let index = self.blocks.len();
            let block = if kind == "text" {
                json!({"type":"text","text":""})
            } else {
                json!({"type":"thinking","thinking":""})
            };
            self.blocks.push(block.clone());
            events.push(event(
                json!({"type":"content_block_start","index":index,"content_block":block}),
            ));
            self.active_block = Some((kind.into(), index));
        }
        let index = self.active_block.as_ref().unwrap().1;
        let key = if kind == "text" { "text" } else { "thinking" };
        let mut accumulated = text(&self.blocks[index], key);
        accumulated.push_str(value);
        self.blocks[index][key] = json!(accumulated);
        events.push(event(json!({"type":"content_block_delta","index":index,"delta":{"type":format!("{kind}_delta"),key:value}})));
    }
    pub fn push(&mut self, mut chunk: Value) -> Result<Vec<String>, String> {
        self.size += chunk.to_string().len();
        if self.size > 32 * 1024 * 1024 {
            return Err("WorkBuddy response exceeds size limit".into());
        }
        if chunk.get("error").is_some() || chunk.get("code").is_some_and(|v| v.as_i64() != Some(0))
        {
            return Err("WorkBuddy rejected the model request; check account authorization and model availability".into());
        }
        if let Some(usage) = chunk.get("usage").filter(|v| v.is_object()) {
            if usage.get("prompt_tokens").is_some() || usage.get("completion_tokens").is_some() {
                let mut normalized = self.usage.clone().unwrap_or(json!({}));
                for (k, v) in usage.as_object().unwrap() {
                    normalized[k] = v.clone();
                }
                normalized.as_object_mut().unwrap().remove("credit");
                if let Some(hit) = normalized.get("prompt_cache_hit_tokens").cloned() {
                    normalized["prompt_tokens_details"] = json!({"cached_tokens":hit});
                }
                self.usage = Some(normalized.clone());
                chunk["usage"] = normalized;
            } else {
                chunk.as_object_mut().unwrap().remove("usage");
            }
        }
        let mut events = Vec::new();
        let choices = chunk
            .get_mut("choices")
            .and_then(Value::as_array_mut)
            .ok_or("WorkBuddy returned an invalid completion event")?;
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                return Err("WorkBuddy returned multiple choices".into());
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                self.finish = Some(reason.into());
            }
            let Some(delta) = choice.get_mut("delta") else {
                continue;
            };
            for (field, kind) in [("reasoning_content", "reasoning"), ("content", "text")] {
                let value = text(delta, field);
                if value.is_empty() {
                    continue;
                }
                if kind == "text" {
                    self.text.push_str(&value);
                } else {
                    self.reasoning.push_str(&value);
                }
                match self.protocol {
                    Protocol::Chat => {}
                    Protocol::Messages => self.message_delta(
                        if kind == "text" { "text" } else { "thinking" },
                        &value,
                        &mut events,
                    ),
                    Protocol::Responses => {
                        let id =
                            format!("{}_{}", if kind == "text" { "msg" } else { "rs" }, self.id);
                        let first = !self.slots.contains_key(kind);
                        let item = if kind == "text" {
                            json!({"id":id,"type":"message","role":"assistant","status":"in_progress","content":[]})
                        } else {
                            json!({"id":id,"type":"reasoning","summary":[]})
                        };
                        let index = self.response_slot(kind, item, &mut events);
                        if first {
                            events.push(self.response_event(if kind == "text" { json!({"type":"response.content_part.added","item_id":id,"output_index":index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}) } else { json!({"type":"response.reasoning_summary_part.added","item_id":id,"output_index":index,"summary_index":0,"part":{"type":"summary_text","text":""}}) }));
                        }
                        events.push(self.response_event(if kind == "text" { json!({"type":"response.output_text.delta","item_id":id,"output_index":index,"content_index":0,"delta":value}) } else { json!({"type":"response.reasoning_summary_text.delta","item_id":id,"output_index":index,"summary_index":0,"delta":value}) }));
                    }
                }
            }
            if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    if index > 255 {
                        return Err("Too many WorkBuddy tool calls".into());
                    }
                    let tool = self.tools.entry(index).or_default();
                    let id = text(call, "id");
                    if tool.id.is_empty() {
                        tool.id = if id.is_empty() {
                            format!("call_{}_{}", self.id, index)
                        } else {
                            id
                        };
                        call["id"] = json!(tool.id);
                        call["type"] = json!("function");
                    } else {
                        if !id.is_empty() && id != tool.id {
                            return Err("WorkBuddy changed a tool call identity mid-stream".into());
                        }
                        call.as_object_mut()
                            .ok_or("Invalid tool call")?
                            .remove("id");
                        call.as_object_mut().unwrap().remove("type");
                    }
                    let function = call.get_mut("function").ok_or("Missing tool function")?;
                    let name = text(function, "name");
                    if !name.is_empty() {
                        if tool.name.is_empty() {
                            tool.name = name;
                        } else if tool.name == name {
                            function
                                .as_object_mut()
                                .ok_or("Invalid tool function")?
                                .remove("name");
                        } else {
                            return Err("WorkBuddy changed a tool name mid-stream".into());
                        }
                    }
                    let args = text(function, "arguments");
                    tool.arguments.push_str(&args);
                    let all_args = tool.arguments.clone();
                    let tool_id = tool.id.clone();
                    let name = tool.name.clone();
                    if self.protocol == Protocol::Responses && !name.is_empty() {
                        let key = format!("tool_{index}");
                        let item_id = format!("fc_{}_{}", self.id, index);
                        let first = !self.slots.contains_key(&key);
                        let args = if first { all_args } else { args };
                        let mut item = json!({"id":item_id,"type":"function_call","call_id":tool_id,"name":name,"arguments":"","status":"in_progress"});
                        if let Some((namespace, client_name)) = self.namespaced.get(&name) {
                            item["name"] = json!(client_name);
                            item["namespace"] = json!(namespace);
                        }
                        let output_index = self.response_slot(&key, item, &mut events);
                        if !args.is_empty() {
                            events.push(self.response_event(json!({"type":"response.function_call_arguments.delta","item_id":item_id,"output_index":output_index,"delta":args})));
                        }
                    }
                }
            }
        }
        if self.protocol == Protocol::Chat {
            chunk["model"] = json!(self.alias);
            chunk["object"] = json!("chat.completion.chunk");
            chunk["id"] = json!(format!("chatcmpl-{}", self.id));
            chunk["created"] = json!(self.created);
            events.push(format!("data: {chunk}\n\n"));
        }
        Ok(events)
    }
    fn usage(&self, protocol: Protocol) -> Value {
        let Some(raw) = &self.usage else {
            return Value::Null;
        };
        let u = CanonicalUsage::from_gemini(raw);
        match protocol {
            Protocol::Chat => raw.clone(),
            Protocol::Responses => {
                json!({"input_tokens":u.total_input_tokens,"output_tokens":u.output_tokens,"total_tokens":u.total_tokens,"input_tokens_details":{"cached_tokens":u.cached_tokens},"output_tokens_details":{"reasoning_tokens":u.reasoning_tokens}})
            }
            Protocol::Messages => {
                json!({"input_tokens":u.total_input_tokens.saturating_sub(u.cached_tokens),"output_tokens":u.output_tokens,"cache_read_input_tokens":u.cached_tokens,"cache_creation_input_tokens":0})
            }
        }
    }
    fn stop_reason(&self) -> &str {
        match self.finish.as_deref().unwrap_or("stop") {
            "tool_calls" | "function_call" => "tool_use",
            "length" => "max_tokens",
            "content_filter" => "refusal",
            _ => {
                if self.tools.is_empty() {
                    "end_turn"
                } else {
                    "tool_use"
                }
            }
        }
    }
    fn response(&self) -> Value {
        json!({"id":format!("resp_{}",self.id),"object":"response","created_at":self.created,"status":if self.finish.as_deref() == Some("length") { "incomplete" } else { "completed" },"model":self.alias,"output":self.blocks,"usage":self.usage(Protocol::Responses),"error":null,"incomplete_details":if self.finish.as_deref() == Some("length") { json!({"reason":"max_output_tokens"}) } else { Value::Null }})
    }
    pub fn end(&mut self) -> Result<(Value, Vec<String>), String> {
        if self.finish.is_none() {
            return Err("WorkBuddy stream ended before completion".into());
        }
        if self.tools.values().any(|t| t.name.is_empty()) {
            return Err("WorkBuddy returned an incomplete tool call".into());
        }
        let mut events = Vec::new();
        let response = match self.protocol {
            Protocol::Chat => {
                events.push("data: [DONE]\n\n".into());
                let mut message = json!({"role":"assistant","content":if self.text.is_empty() && !self.tools.is_empty() { Value::Null } else { json!(self.text) }});
                if !self.reasoning.is_empty() {
                    message["reasoning_content"] = json!(self.reasoning);
                }
                if !self.tools.is_empty() {
                    message["tool_calls"] = json!(self.tools.values().map(|t| json!({"id":t.id,"type":"function","function":{"name":t.name,"arguments":t.arguments}})).collect::<Vec<_>>());
                }
                json!({"id":format!("chatcmpl-{}",self.id),"object":"chat.completion","created":self.created,"model":self.alias,"choices":[{"index":0,"message":message,"finish_reason":self.finish}],"usage":self.usage(Protocol::Chat)})
            }
            Protocol::Messages => {
                if let Some((_, index)) = self.active_block.take() {
                    events.push(event(json!({"type":"content_block_stop","index":index})));
                }
                for tool in self.tools.values() {
                    let input: Value = serde_json::from_str(&tool.arguments)
                        .map_err(|_| "WorkBuddy returned invalid tool arguments")?;
                    let index = self.blocks.len();
                    let block =
                        json!({"type":"tool_use","id":tool.id,"name":tool.name,"input":input});
                    self.blocks.push(block);
                    events.push(event(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":tool.id,"name":tool.name,"input":{}}})));
                    events.push(event(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":tool.arguments}})));
                    events.push(event(json!({"type":"content_block_stop","index":index})));
                }
                let usage = self.usage(Protocol::Messages);
                events.push(event(json!({"type":"message_delta","delta":{"stop_reason":self.stop_reason(),"stop_sequence":null},"usage":usage})));
                events.push(event(json!({"type":"message_stop"})));
                json!({"id":format!("msg_{}",self.id),"type":"message","role":"assistant","model":self.alias,"content":self.blocks,"stop_reason":self.stop_reason(),"stop_sequence":null,"usage":usage})
            }
            Protocol::Responses => {
                for (key, index) in self.slots.clone() {
                    let mut item = self.blocks[index].clone();
                    let id = item["id"].clone();
                    if key == "text" {
                        let part = json!({"type":"output_text","text":self.text,"annotations":[]});
                        item["content"] = json!([part]);
                        item["status"] = json!("completed");
                        events.push(self.response_event(json!({"type":"response.output_text.done","item_id":id,"output_index":index,"content_index":0,"text":self.text})));
                        events.push(self.response_event(json!({"type":"response.content_part.done","item_id":id,"output_index":index,"content_index":0,"part":part})));
                    } else if key == "reasoning" {
                        let part = json!({"type":"summary_text","text":self.reasoning});
                        item["summary"] = json!([part]);
                        events.push(self.response_event(json!({"type":"response.reasoning_summary_text.done","item_id":id,"output_index":index,"summary_index":0,"text":self.reasoning})));
                        events.push(self.response_event(json!({"type":"response.reasoning_summary_part.done","item_id":id,"output_index":index,"summary_index":0,"part":part})));
                    } else {
                        let tool =
                            &self.tools[&key.trim_start_matches("tool_").parse::<u64>().unwrap()];
                        item["arguments"] = json!(tool.arguments);
                        item["status"] = json!("completed");
                        events.push(self.response_event(json!({"type":"response.function_call_arguments.done","item_id":id,"output_index":index,"arguments":item["arguments"]})));
                    }
                    self.blocks[index] = item.clone();
                    events.push(self.response_event(json!({"type":"response.output_item.done","output_index":index,"item":item})));
                }
                let response = self.response();
                events.push(self.response_event(json!({"type":if response["status"] == "incomplete" { "response.incomplete" } else { "response.completed" },"response":response})));
                response
            }
        };
        Ok((response, events))
    }
}
