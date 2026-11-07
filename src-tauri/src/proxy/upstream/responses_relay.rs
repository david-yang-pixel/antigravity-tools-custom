//! Responses-native delivery: upstream Responses events reach a Responses client as they
//! are (streaming), aggregated into one response (non-streaming), or as one unary JSON
//! answer (compaction). The public alias replaces every upstream model name.
use super::{chat_relay::Events, responses_wire};
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use serde_json::{json, Value};

pub(crate) struct ResponsesRelay {
    pub alias: String,
    pub headers: HeaderMap,
    pub error_code: &'static str,
}

/// Names the alias wherever the upstream named its model: `response.model` and a
/// top-level `model`.
pub(crate) fn rename(value: &mut Value, alias: &str) {
    if let Some(response) = value.get_mut("response").and_then(Value::as_object_mut) {
        if response.contains_key("model") {
            response.insert("model".into(), json!(alias));
        }
    }
    if let Some(object) = value.as_object_mut() {
        if object.contains_key("model") {
            object.insert("model".into(), json!(alias));
        }
    }
}

/// The error object of a Responses failure event: `response.failed` carries it in
/// `response.error`, an `error` event in `error` or as its own top-level fields.
pub(crate) fn failure_error(event: &Value) -> Option<Value> {
    let object = |v: Option<&Value>| v.filter(|e| e.is_object()).cloned();
    match responses_wire::event_type(event)? {
        "response.failed" => object(event.pointer("/response/error")),
        "error" => object(event.get("error")).or_else(|| {
            let mut error = serde_json::Map::new();
            for key in ["code", "message", "param"] {
                if let Some(value) = event.get(key).filter(|v| !v.is_null()) {
                    error.insert(key.into(), value.clone());
                }
            }
            error.contains_key("message").then(|| Value::Object(error))
        }),
        _ => None,
    }
}

fn frame(value: &Value) -> bytes::Bytes {
    let kind = responses_wire::event_type(value).unwrap_or("message");
    bytes::Bytes::from(format!("event: {kind}\ndata: {value}\n\n"))
}

impl ResponsesRelay {
    fn failed(&self, message: &str) -> Response {
        tracing::warn!(
            "Responses upstream response failed: {}",
            crate::proxy::upstream::client::sanitize_error_for_log(message)
        );
        super::error_response(
            StatusCode::BAD_GATEWAY,
            self.error_code,
            "The upstream response failed or was incomplete.",
        )
    }
    /// A failure the upstream reported in its own words, after output started.
    fn failed_upstream(&self, error: Value) -> Response {
        tracing::warn!("Responses upstream response failed after output started");
        self.decorate((StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))).into_response())
    }
    fn decorate(&self, mut response: Response) -> Response {
        response.headers_mut().extend(self.headers.clone());
        response
    }
    /// The terminal event a client receives when the upstream fails after output started.
    fn failure_event(alias: &str, code: &str, message: &str) -> Value {
        json!({"type":"response.failed","response":{"object":"response","model":alias,
            "status":"failed","error":{"code":code,"message":message}}})
    }

    pub async fn run(self, mut events: Events, streaming: bool) -> Response {
        if !streaming {
            let mut collected = Vec::new();
            while let Some(data) = events.next().await {
                let data = match data {
                    Ok(data) => data,
                    Err(message) => {
                        return match collected.iter().rev().find_map(failure_error) {
                            Some(error) => self.failed_upstream(error),
                            None => self.failed(&message),
                        }
                    }
                };
                if data == "[DONE]" {
                    break;
                }
                match serde_json::from_str::<Value>(&data) {
                    Ok(value) => collected.push(value),
                    Err(_) => return self.failed("Invalid upstream event"),
                }
            }
            return match responses_wire::aggregate(&collected) {
                Ok(mut response) => {
                    response["model"] = json!(self.alias);
                    self.decorate(Json(response).into_response())
                }
                Err(message) => self.failed(&message),
            };
        }
        let first = match events.next().await {
            Some(Ok(data)) if data != "[DONE]" => data,
            Some(Err(message)) => return self.failed(&message),
            _ => return self.failed("Upstream ended before producing a response"),
        };
        let (alias, code) = (self.alias.clone(), self.error_code);
        let body = async_stream::stream! {
            let mut pending = Some(first);
            // The upstream's own failure event already reached the client.
            let mut reported = false;
            loop {
                let data = match pending.take() {
                    Some(data) => data,
                    None => match events.next().await {
                        Some(Ok(data)) => data,
                        Some(Err(message)) => {
                            if !reported {
                                yield frame(&Self::failure_event(&alias, code, &message));
                            }
                            break;
                        }
                        None => break,
                    },
                };
                if data == "[DONE]" {
                    break;
                }
                let Ok(mut value) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                rename(&mut value, &alias);
                reported |= matches!(
                    responses_wire::event_type(&value),
                    Some("error" | "response.failed")
                );
                yield frame(&value);
            }
        };
        self.decorate(
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header("x-accel-buffering", "no")
                .body(Body::from_stream(
                    body.map(Ok::<_, std::convert::Infallible>),
                ))
                .unwrap(),
        )
    }

    /// One JSON answer (the first event) with `model` naming the alias.
    pub async fn unary(self, mut events: Events) -> Response {
        let data = match events.next().await {
            Some(Ok(data)) if data != "[DONE]" => data,
            Some(Err(message)) => return self.failed(&message),
            _ => return self.failed("Upstream returned no response"),
        };
        // Read through the end marker so the exchange settles as a success.
        while let Some(rest) = events.next().await {
            match rest {
                Ok(marker) if marker == "[DONE]" => break,
                Ok(_) => {}
                Err(message) => return self.failed(&message),
            }
        }
        match serde_json::from_str::<Value>(&data) {
            Ok(mut value) if value.is_object() => {
                value["model"] = json!(self.alias);
                self.decorate(Json(value).into_response())
            }
            _ => self.failed("Invalid upstream response"),
        }
    }
}
