//! Administrator-defined OpenAI-compatible upstreams (base URL + API key). Chat Completions,
//! HTTP Responses and Messages requests are converted by the shared Chat codec and sent to
//! `{base_url}/chat/completions`. Auth, admission, canonical usage and billing stay in the
//! shared middleware; this layer only owns the wire exchange.
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use eventsource_stream::Eventsource;
use futures::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};

use super::chat_codec as codec;
use super::{error_response as error, published_request};
use crate::modules::custom_upstream::{self, Upstream};
use crate::modules::model_registry::PublishedModel;

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

pub async fn route(
    State(state): State<crate::proxy::server::AppState>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::POST
        || !crate::proxy::middleware::model_registry::is_inference_path(request.uri().path())
    {
        return next.run(request).await;
    }
    let (request, published) = match published_request(request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(id) = custom_upstream::upstream_id(&published.source_id) else {
        return next.run(request).await;
    };
    let upstream = match custom_upstream::get(id) {
        Ok(Some(upstream)) => upstream,
        Ok(None) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "custom_upstream_missing",
                "This model's upstream no longer exists; publish it from another source.",
            )
        }
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "custom_upstream_configuration_invalid",
                "Repair the custom upstream configuration in admin settings.",
            )
        }
    };
    if !upstream.enabled {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "custom_upstream_disabled",
            "This model's upstream is disabled.",
        );
    }
    let proxy = state.upstream_proxy.read().await.clone();
    let client = match custom_upstream::http(&proxy) {
        Ok(client) => client,
        Err(message) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "custom_upstream_unavailable",
                &message,
            )
        }
    };
    dispatch(client, upstream, published, state.request_timeout, request).await
}

async fn dispatch(
    client: reqwest::Client,
    upstream: Upstream,
    published: PublishedModel,
    timeout: u64,
    request: Request,
) -> Response {
    let Some(protocol) = codec::Protocol::for_path(request.uri().path()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "custom_upstream_protocol_unsupported",
            "OpenAI-compatible upstream models support Chat Completions, HTTP Responses and Messages inference only.",
        );
    };
    let (parts, body) = request.into_parts();
    let limit = std::env::var("ABV_MAX_BODY_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50 * 1024 * 1024);
    let Ok(bytes) = to_bytes(body, limit).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "Request exceeds the configured body limit.",
        );
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "Expected a JSON request.",
        );
    };
    let streaming = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let namespaced = codec::namespaced_tools(&payload);
    let payload = match codec::request_for(protocol, payload, codec::Dialect::OPENAI_COMPATIBLE) {
        Ok(payload) => prepare(payload, &published.native_model),
        Err(message) => {
            return error(
                StatusCode::BAD_REQUEST,
                "custom_upstream_request_unsupported",
                &message,
            )
        }
    };
    if let Some(capture) = parts
        .extensions
        .get::<crate::proxy::monitor::UpstreamRequestBodyHolder>()
    {
        capture.set_value(&payload);
    }
    let exchange = Exchange {
        client,
        upstream,
        alias: published.public_id,
        target: published.native_model,
        protocol,
        namespaced,
        deadline: tokio::time::Instant::now()
            + std::time::Duration::from_secs(timeout.clamp(1, 3600)),
    };
    exchange.forward(payload, streaming).await
}

/// Normalizes a converted request to the portable Chat Completions subset. The upstream is
/// always streamed (usage included) and non-streaming clients get the aggregated result.
pub(crate) fn prepare(mut payload: Value, model: &str) -> Value {
    payload["model"] = json!(model);
    payload["stream"] = json!(true);
    payload["stream_options"] = json!({"include_usage": true});
    if let Some(object) = payload.as_object_mut() {
        // Anthropic `thinking` is not a Chat Completions field; strict upstreams reject it.
        object.remove("thinking");
    }
    if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            // `developer` is OpenAI-only; `system` carries the same instruction everywhere.
            if message.get("role").and_then(Value::as_str) == Some("developer") {
                message["role"] = json!("system");
            }
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in content {
                    if let Some(url) = part
                        .get("image_url")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                    {
                        part["image_url"] = json!({ "url": url });
                    }
                }
            }
        }
    }
    payload
}

/// A non-streaming completion re-expressed as one stream chunk, for upstreams that ignore
/// `stream: true`.
pub(crate) fn completion_chunk(mut value: Value) -> Value {
    if let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            let Some(mut message) = choice.as_object_mut().and_then(|c| c.remove("message")) else {
                continue;
            };
            if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for (index, call) in calls.iter_mut().enumerate() {
                    if call.get("index").is_none() {
                        call["index"] = json!(index);
                    }
                }
            }
            choice["delta"] = message;
        }
    }
    value
}

/// Bounded, redacted `error.message` (or body prefix) of a rejected response.
async fn rejection_detail(upstream: reqwest::Response) -> String {
    let read = async {
        let mut body = Vec::new();
        let mut chunks = upstream.bytes_stream();
        while let Some(Ok(chunk)) = chunks.next().await {
            body.extend_from_slice(&chunk);
            if body.len() >= 8192 {
                break;
            }
        }
        body
    };
    let Ok(body) = tokio::time::timeout(std::time::Duration::from_secs(3), read).await else {
        return String::new();
    };
    let text = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
    let text: String = text.trim().chars().take(300).collect();
    crate::proxy::upstream::client::sanitize_error_for_log(&text)
}

struct Exchange {
    client: reqwest::Client,
    upstream: Upstream,
    alias: String,
    target: String,
    protocol: codec::Protocol,
    namespaced: codec::Namespaced,
    deadline: tokio::time::Instant,
}

type Events = BoxStream<'static, Result<String, String>>;

impl Exchange {
    async fn forward(self, payload: Value, streaming: bool) -> Response {
        let remaining = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        let sent = self
            .upstream
            .authorize(self.client.post(self.upstream.endpoint("chat/completions")))
            .header(header::ACCEPT, "text/event-stream, application/json")
            .json(&payload)
            .timeout(remaining)
            .send()
            .await;
        let upstream = match sent {
            Ok(response) => response,
            Err(e) if e.is_timeout() => {
                return error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "custom_upstream_timeout",
                    "The upstream did not respond in time.",
                )
            }
            Err(_) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "custom_upstream_unavailable",
                    "Cannot reach the upstream; check its base URL and network.",
                )
            }
        };
        let status = upstream.status();
        if !status.is_success() {
            return self.rejected(upstream).await;
        }
        let json_body = upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
        let events = if json_body {
            match Self::json_events(upstream).await {
                Ok(events) => events,
                Err(message) => return self.failed(&message),
            }
        } else {
            Self::sse_events(upstream)
        };
        self.relay(events, streaming).await
    }

    async fn rejected(&self, upstream: reqwest::Response) -> Response {
        let code = upstream.status().as_u16();
        let detail = rejection_detail(upstream).await;
        tracing::warn!(
            "Custom upstream {} rejected model {} with HTTP {}: {}",
            self.upstream.id,
            self.target,
            code,
            if detail.is_empty() {
                "<no detail>"
            } else {
                &detail
            }
        );
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        match code {
            401 | 403 => error(
                StatusCode::BAD_GATEWAY,
                "custom_upstream_auth_failed",
                "The upstream rejected the configured API key.",
            ),
            429 => error(
                StatusCode::TOO_MANY_REQUESTS,
                "custom_upstream_rate_limited",
                &format!("The upstream is rate limiting requests{suffix}"),
            ),
            // Request-shape rejections are the caller's to fix; relay status and reason.
            400 | 404 | 413 | 422 => error(
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
                "custom_upstream_rejected",
                &format!("The upstream returned HTTP {code}{suffix}"),
            ),
            _ => error(
                StatusCode::BAD_GATEWAY,
                "custom_upstream_rejected",
                &format!("The upstream returned HTTP {code}{suffix}"),
            ),
        }
    }

    fn failed(&self, message: &str) -> Response {
        tracing::warn!(
            "Custom upstream {} failed for model {}: {}",
            self.upstream.id,
            self.target,
            message
        );
        error(
            StatusCode::BAD_GATEWAY,
            "custom_upstream_invalid_response",
            message,
        )
    }

    fn sse_events(upstream: reqwest::Response) -> Events {
        // Bound bytes before the SSE parser, including a peer that never terminates an event.
        let mut received = 0usize;
        let source = upstream.bytes_stream().map(move |chunk| {
            let chunk = chunk.map_err(|_| std::io::Error::other("connection interrupted"))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err(std::io::Error::other("response exceeds size limit"));
            }
            Ok(chunk)
        });
        source
            .eventsource()
            .map(|event| {
                event
                    .map(|event| event.data)
                    .map_err(|_| "Upstream stream interrupted".to_string())
            })
            .boxed()
    }

    async fn json_events(upstream: reqwest::Response) -> Result<Events, String> {
        let mut body = Vec::new();
        let mut chunks = upstream.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|_| "Upstream response was interrupted")?;
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err("Upstream response exceeds size limit".into());
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&body).map_err(|_| "Upstream returned invalid JSON")?;
        let chunk = completion_chunk(value).to_string();
        Ok(futures::stream::iter([Ok(chunk), Ok("[DONE]".to_string())]).boxed())
    }

    async fn relay(self, mut events: Events, streaming: bool) -> Response {
        let mut output =
            codec::Output::new(self.protocol, &self.alias).with_namespaced(self.namespaced.clone());
        if streaming {
            // Inspect provider errors before committing HTTP 200, so shared billing sees a
            // failed status when no generation has started.
            let mut initial = output.start();
            while !output.has_output() {
                let data = match events.next().await {
                    Some(Ok(data)) if data != "[DONE]" => data,
                    Some(Err(message)) => return self.failed(&message),
                    _ => return self.failed("Upstream ended before producing a completion"),
                };
                if data.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&data) else {
                    return self.failed("Invalid upstream event");
                };
                match output.push(value) {
                    Ok(frames) => initial.extend(frames),
                    Err(message) => return self.failed(&message),
                }
            }
            let body = async_stream::try_stream! {
                for frame in initial { yield bytes::Bytes::from(frame); }
                while let Some(data) = events.next().await {
                    let data = data.map_err(std::io::Error::other)?;
                    if data == "[DONE]" { break; }
                    if data.trim().is_empty() { continue; }
                    let value: Value = serde_json::from_str(&data)
                        .map_err(|_| std::io::Error::other("Invalid upstream stream event"))?;
                    for frame in output.push(value).map_err(std::io::Error::other)? {
                        yield bytes::Bytes::from(frame);
                    }
                }
                let (_, frames) = output.end().map_err(std::io::Error::other)?;
                for frame in frames { yield bytes::Bytes::from(frame); }
            };
            let response = Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header("x-accel-buffering", "no")
                .body(Body::from_stream(
                    body.map(|item: Result<bytes::Bytes, std::io::Error>| item),
                ))
                .unwrap();
            return self.decorate(response);
        }
        while let Some(data) = events.next().await {
            let data = match data {
                Ok(data) => data,
                Err(message) => return self.failed(&message),
            };
            if data == "[DONE]" {
                break;
            }
            if data.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                return self.failed("Invalid upstream event");
            };
            if let Err(message) = output.push(value) {
                return self.failed(&message);
            }
        }
        match output.end() {
            Ok((value, _)) => self.decorate(Json(value).into_response()),
            Err(message) => self.failed(&message),
        }
    }

    fn decorate(&self, mut response: Response) -> Response {
        let headers = response.headers_mut();
        if let Ok(value) = HeaderValue::from_str(&self.target) {
            headers.insert("x-mapped-model", value);
        }
        headers.insert(
            "x-upstream-provider",
            HeaderValue::from_static(custom_upstream::PROVIDER),
        );
        // Token Stats groups usage by this key, one row per upstream.
        if let Ok(value) = HeaderValue::from_str(&format!("{}@openai-compatible", self.upstream.id))
        {
            headers.insert("x-account-email", value);
        }
        response
    }
}

#[cfg(test)]
mod tests;
