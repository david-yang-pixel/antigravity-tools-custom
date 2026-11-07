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
    // The payload has no tool-selection field: `none` is honored by offering no tools;
    // `required`/forced choices are rejected before dispatch (proxy::upstream::native).
    let tools = if chat["tool_choice"] == "none" {
        json!([])
    } else {
        chat.get("tools").cloned().unwrap_or(json!([]))
    };
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
        "aliyun_user_type":a.credentials.user_type,"model_config":config,"messages":chat["messages"],"parameters":parameters,"tools":tools,
        "business":{"product":"cli","version":qoder::sign::version(),"type":"agent","id":conversation,"name":prompt.chars().take(30).collect::<String>(),"begin_at":now()*1000,"stage":"start","model_key":model}}),
    )
}
/// A chunk is normalized before it reaches the shared client-protocol output codec.
/// None denotes an auxiliary event; `[DONE]` is the sole clean end marker.
pub fn event(provider: Provider, data: &str) -> Result<Option<String>, Failure> {
    if data.trim() == "[DONE]" {
        return Ok(Some("[DONE]".into()));
    }
    if data.trim().is_empty() {
        return Ok(None);
    }
    let decoded = qoder_client::signing::decrypt(data)
        .map_err(|_| Failure::new(502, "Cannot decrypt Qoder stream event"))?;
    if decoded.trim() == "[DONE]" {
        return Ok(Some("[DONE]".into()));
    }
    let v: Value = serde_json::from_str(&decoded)
        .map_err(|_| Failure::new(502, "Invalid provider stream event"))?;
    match provider {
        Provider::ChatGpt => return Err(Failure::new(501, "ChatGPT support is incomplete")),
        Provider::Qoder => {
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
                let decoded = qoder_client::signing::decrypt(body)
                    .map_err(|_| Failure::new(502, "Cannot decrypt Qoder stream envelope"))?;
                if decoded.trim() == "[DONE]" {
                    return Ok(Some("[DONE]".into()));
                }
                let inner: Value = serde_json::from_str(&decoded)
                    .map_err(|_| Failure::new(502, "Invalid Qoder stream envelope"))?;
                if inner.get("error").is_some() {
                    return Err(Failure::new(502, "Qoder returned an inference error"));
                }
                return Ok(Some(inner.to_string()));
            }
            if v.get("choices").is_some() || v.get("usage").is_some() {
                return Ok(Some(v.to_string()));
            }
            Ok(None)
        }
    }
}
