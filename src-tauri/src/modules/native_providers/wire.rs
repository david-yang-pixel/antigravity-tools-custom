//! Provider-only payload/SSE differences. Client protocol adaptation stays in Chat codec.
use super::{http::Failure, *};
use serde_json::json;

pub fn qoder_request(a: &Account, model: &str, mut chat: Value) -> Result<Value, Failure> {
    let messages = chat
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or("Expected chat messages")?;
    for message in messages.iter_mut() {
        if message["role"] == "developer" {
            message["role"] = json!("system");
        }
    }
    let prompt = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string();
    let meta = a
        .models
        .iter()
        .find(|m| m.id == model)
        .ok_or("Model is absent from this account's catalog")?;
    let config = json!({"key":model,"display_name":if meta.name.is_empty() { model } else { &meta.name },"model":"","format":"openai","is_vl":meta.vision,"is_reasoning":meta.reasoning,
        "api_key":"","url":"","source":"system","max_input_tokens":if meta.context_window > 0 { meta.context_window } else { 180000 }});
    let mut parameters = json!({});
    for key in [
        "max_tokens",
        "temperature",
        "top_p",
        "reasoning_effort",
        "stop",
    ] {
        if let Some(value) = chat.get(key) {
            parameters[key] = value.clone();
        }
    }
    if parameters.get("max_tokens").is_none() {
        if let Some(value) = chat.get("max_completion_tokens") {
            parameters["max_tokens"] = value.clone();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    // Group by a stable conversation prefix rather than splitting each tool turn into a new
    // upstream conversation. No raw prompt or account credential enters the identifier.
    use sha2::{Digest, Sha256};
    let prefix = chat["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "system" || m["role"] == "user")
        .take(2)
        .cloned()
        .collect::<Vec<_>>();
    let conversation = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&prefix).map_err(|_| "Cannot encode conversation identity")?
        )
    );
    Ok(
        json!({"request_id":id,"request_set_id":uuid::Uuid::new_v4().to_string(),"chat_record_id":conversation,"session_id":conversation,
        "stream":true,"chat_task":"FREE_INPUT","chat_context":{"chatPrompt":"","extra":{"context":[],"modelConfig":config,"originalContent":{"type":"text","text":prompt}},"features":[],"text":{"type":"text","text":prompt}},
        "is_reply":true,"is_retry":false,"source":1,"version":"3","chat_prompt":"","session_type":"qodercli","agent_id":"agent_common","task_id":"common",
        "aliyun_user_type":a.credentials.user_type,"model_config":config,"messages":chat["messages"],"parameters":parameters,"tools":chat.get("tools").cloned().unwrap_or(json!([])),
        "business":{"product":"cli","version":qoder::sign::version(),"type":"agent","id":conversation,"name":prompt.chars().take(30).collect::<String>(),"begin_at":now()*1000,"stage":"start","model_key":model}}),
    )
}
pub fn trae_request(a: &Account, model: &str, mut chat: Value) -> Result<Value, Failure> {
    chat["stream"] = json!(true);
    chat["function"] = json!(trae::function(a, model));
    if model.eq_ignore_ascii_case("auto") {
        chat.as_object_mut()
            .ok_or("Invalid chat request")?
            .remove("model");
    } else {
        chat["model"] = json!(model);
        chat["config_name"] = json!(model);
    }
    if chat.get("max_tokens").is_none() {
        if let Some(value) = chat.get("max_completion_tokens").cloned() {
            chat["max_tokens"] = value;
        }
    }
    for key in ["stream_options", "max_completion_tokens", "thinking"] {
        chat.as_object_mut()
            .ok_or("Invalid chat request")?
            .remove(key);
    }
    let messages = chat
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or("Expected chat messages")?;
    for message in messages {
        if message["role"] == "developer" {
            message["role"] = json!("system");
        }
        if let Some(content) = message
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            message["content"] = json!([{"type":"text","text":content}]);
        }
        if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
            for call in calls {
                if let Some(function) = call
                    .as_object_mut()
                    .and_then(|call| call.remove("function"))
                {
                    call["function_call"] = function;
                }
            }
        }
    }
    if let Some(tools) = chat.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(parameters) = tool.pointer_mut("/function/parameters") {
                if parameters.is_object() || parameters.is_array() {
                    *parameters = json!(parameters.to_string());
                }
            }
        }
    }
    if chat["tool_choice"] == "none" {
        let object = chat.as_object_mut().unwrap();
        object.remove("tool_choice");
        object.remove("tools");
    } else if let Some(name) = chat
        .pointer("/tool_choice/function/name")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        chat["tool_choice"] = json!(name);
    }
    Ok(chat)
}

/// A chunk is normalized before it reaches the shared client-protocol output codec.
/// None denotes an auxiliary event; `[DONE]` is the sole clean end marker.
pub fn event(provider: Provider, name: &str, data: &str) -> Result<Option<String>, Failure> {
    if data.trim() == "[DONE]" {
        return Ok(Some("[DONE]".into()));
    }
    if provider == Provider::Trae
        && !matches!(
            name,
            "" | "output" | "metadata" | "token_usage" | "done" | "error"
        )
    {
        return Ok(None);
    }
    if data.trim().is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(data)
        .map_err(|_| Failure::new(502, "Invalid provider stream event"))?;
    if provider == Provider::Qoder {
        if let Some(status) = number(&v, &["statusCodeValue"]) {
            if status != 200.0 {
                return Err(Failure::new(
                    status as u16,
                    "Qoder rejected the inference request",
                ));
            }
        }
        if let Some(body) = v.get("body").and_then(Value::as_str) {
            if body == "[DONE]" {
                return Ok(Some(body.into()));
            }
            let inner: Value = serde_json::from_str(body)
                .map_err(|_| Failure::new(502, "Invalid Qoder stream envelope"))?;
            if inner.get("error").is_some() {
                return Err(Failure::new(502, "Qoder returned an inference error"));
            }
            return Ok(Some(inner.to_string()));
        }
        if v.get("choices").is_some() || v.get("usage").is_some() {
            return Ok(Some(v.to_string()));
        }
        return Ok(None);
    }
    if v.get("choices").is_some() {
        return Ok(Some(v.to_string()));
    }
    let chunk = match name {
        "output" => {
            let mut delta = json!({});
            if let Some(content) = v.get("response") {
                delta["content"] = content.clone();
            }
            if let Some(reasoning) = v.get("reasoning_content") {
                delta["reasoning_content"] = reasoning.clone();
            }
            if let Some(calls) = v.get("tool_calls") {
                let mut calls = if calls.is_array() {
                    calls.as_array().unwrap().clone()
                } else {
                    vec![calls.clone()]
                };
                for (index, call) in calls.iter_mut().enumerate() {
                    if let Some(function) =
                        call.as_object_mut().and_then(|v| v.remove("function_call"))
                    {
                        call["function"] = function;
                    }
                    if call.get("index").is_none() {
                        call["index"] = json!(index);
                    }
                    if call.get("type").is_none() {
                        call["type"] = json!("function");
                    }
                }
                delta["tool_calls"] = json!(calls);
            }
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]})
        }
        "token_usage" => json!({"choices":[],"usage":v}),
        "done" => {
            let reason = text(&v, &["finish_reason"]);
            json!({"choices":[{"index":0,"delta":{},"finish_reason":if reason.is_empty() { "stop" } else { &reason }}]})
        }
        "error" => {
            return Err(Failure::new(
                number(&v, &["code"])
                    .filter(|n| *n >= 400.0 && *n < 600.0)
                    .unwrap_or(502.0) as u16,
                "TRAE returned an inference error",
            ))
        }
        _ => return Ok(None),
    };
    Ok(Some(chunk.to_string()))
}
