//! Built-in WorkBuddy provider. Protocol conversion is isolated in codec.
//! Auth, admission, canonical usage and billing stay in the shared middleware.
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};
mod codec;

use crate::modules::workbuddy::{self, Settings};
use workbuddy::accounts::scheduling::Policy;

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"type": code, "code": code, "message": message}})),
    )
        .into_response()
}

fn endpoint(path: &str) -> Option<&'static str> {
    match path {
        "/v1/chat/completions" => Some("/v1/chat/completions"),
        "/v1/responses" | "/responses" => Some("/v1/responses"),
        "/v1/messages" => Some("/v1/messages"),
        _ => None,
    }
}

pub async fn route(
    State(state): State<crate::proxy::server::AppState>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::POST || !is_inference_path(request.uri().path()) {
        return next.run(request).await;
    }
    let mapping = state.custom_mapping.read().await.clone();
    let settings = match workbuddy::load() {
        Ok(settings) => settings,
        Err(_) => return configuration_error(request, next, &mapping).await,
    };
    let policy = Policy {
        sticky: state.token_manager.get_sticky_config().await,
        breaker: state.token_manager.get_circuit_breaker_config().await,
        preferred: settings.preferred_account_id.clone(),
    };
    dispatch(
        settings,
        mapping,
        state.request_timeout,
        policy,
        request,
        next,
    )
    .await
}

async fn configuration_error(
    request: Request,
    next: Next,
    mapping: &std::collections::HashMap<String, String>,
) -> Response {
    let (request, model) =
        match crate::proxy::middleware::user_token_models::requested_model(request).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if model.is_some_and(|model| workbuddy::known_model_on_configuration_error(&model, mapping)) {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "workbuddy_configuration_invalid",
            "Repair the WorkBuddy upstream configuration in admin settings.",
        )
    } else {
        next.run(request).await
    }
}

fn is_inference_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/chat/completions"
            | "/v1/responses"
            | "/responses"
            | "/responses/compact"
            | "/v1/messages"
            | "/v1/messages/count_tokens"
            | "/v1/completions"
            | "/v1/images/generations"
            | "/v1/images/edits"
            | "/v1/audio/transcriptions"
    ) || path.starts_with("/v1beta/models/")
}

async fn dispatch(
    settings: Settings,
    mapping: std::collections::HashMap<String, String>,
    timeout: u64,
    policy: Policy,
    request: Request,
    next: Next,
) -> Response {
    if settings.models.is_empty() || request.method() != Method::POST {
        return next.run(request).await;
    }
    let path = request.uri().path().to_owned();
    // Inspect only inference routes. Telemetry/health requests keep their existing behavior.
    if !is_inference_path(&path) {
        return next.run(request).await;
    }
    let (request, requested_model) =
        match crate::proxy::middleware::user_token_models::requested_model(request).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let Some(alias) = requested_model else {
        return next.run(request).await;
    };
    let Some(target) = settings.target_model(&alias, &mapping) else {
        return next.run(request).await;
    };
    if !settings.enabled {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "workbuddy_disabled",
            "This model's WorkBuddy upstream is disabled.",
        );
    }
    let Some(endpoint) = endpoint(&path) else {
        return error(StatusCode::BAD_REQUEST, "workbuddy_protocol_unsupported", "WorkBuddy models support Chat Completions, HTTP Responses and Messages inference only.");
    };
    let (parts, body) = request.into_parts();
    let limit = std::env::var("ABV_MAX_BODY_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50 * 1024 * 1024);
    let bytes = match to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "Request exceeds the configured body limit.",
            )
        }
    };
    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(payload) => payload,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "Expected a JSON request.",
            )
        }
    };
    let protocol = codec::Protocol::for_path(endpoint).expect("checked endpoint");
    let streaming = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let payload = match codec::request(protocol, payload) {
        Ok(payload) => payload,
        Err(message) => {
            return error(
                StatusCode::BAD_REQUEST,
                "workbuddy_request_unsupported",
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
    let scope = parts
        .extensions
        .get::<crate::proxy::middleware::auth::UserTokenIdentity>()
        .map(|identity| identity.token_id.as_str())
        .unwrap_or("administrator");
    forward(
        timeout, payload, streaming, &alias, target, protocol, scope, policy,
    )
    .await
}

async fn forward(
    timeout: u64,
    payload: Value,
    streaming: bool,
    alias: &str,
    target: &str,
    protocol: codec::Protocol,
    scope: &str,
    policy: Policy,
) -> Response {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(timeout.clamp(1, 3600));
    let session = workbuddy::client::session_key(scope, &payload);
    let lease =
        match workbuddy::accounts::scheduling::acquire(target, &session, &policy, timeout).await {
            Ok(lease) => lease,
            Err(message) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "workbuddy_account_unavailable",
                    &message,
                )
            }
        };
    let account = match workbuddy::accounts::fresh_before(&lease.account.id, deadline).await {
        Ok(Ok(account)) if account.enabled => account,
        Ok(Ok(_)) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "workbuddy_account_disabled",
                "WorkBuddy account was disabled",
            )
        }
        Ok(Err(message)) => {
            lease.failure("Account authorization failed", 30);
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "workbuddy_authorization_failed",
                &message,
            );
        }
        Err(_) => {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "workbuddy_timeout",
                "WorkBuddy request timed out while preparing account authorization",
            );
        }
    };
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return error(
            StatusCode::GATEWAY_TIMEOUT,
            "workbuddy_timeout",
            "WorkBuddy request timed out before generation",
        );
    }
    let upstream = match workbuddy::client::chat(&account, target, payload, scope, deadline).await {
        Ok(response) => response,
        Err(message) => {
            lease.failure("Upstream connection failed", 10);
            return error(StatusCode::BAD_GATEWAY, "workbuddy_unavailable", &message);
        }
    };
    if !upstream.status().is_success() {
        let status = upstream.status();
        let retry_after = upstream
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok());
        lease.failure(
            &format!("Upstream HTTP {}", status.as_u16()),
            retry_after.unwrap_or(if status.as_u16() == 429 { 60 } else { 15 }),
        );
        return error(if status.as_u16() == 429 { StatusCode::TOO_MANY_REQUESTS } else { StatusCode::BAD_GATEWAY },
            "workbuddy_upstream_rejected", &format!("WorkBuddy returned HTTP {}; refresh account authorization or check model availability",status.as_u16()));
    }
    if !upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        lease.failure("Invalid upstream response type", 15);
        return error(
            StatusCode::BAD_GATEWAY,
            "workbuddy_invalid_response",
            "WorkBuddy did not return a model event stream; check account authorization",
        );
    }
    // Bound bytes before the SSE parser, including a peer that never terminates an event.
    let mut received = 0usize;
    let source = upstream.bytes_stream().map(move |chunk| {
        let chunk = chunk.map_err(|_| std::io::Error::other("WorkBuddy connection interrupted"))?;
        received = received.saturating_add(chunk.len());
        if received > 32 * 1024 * 1024 {
            return Err(std::io::Error::other(
                "WorkBuddy response exceeds size limit",
            ));
        }
        Ok(chunk)
    });
    let mut events = source.eventsource();
    let mut output = codec::Output::new(protocol, alias);
    if streaming {
        // Inspect provider errors before committing HTTP 200. Shared billing must
        // see a failed status when no generation has started (including per-call prices).
        let mut initial = output.start();
        while !output.has_output() {
            let event = match events.next().await {
                Some(Ok(event)) => event,
                _ => {
                    lease.failure("Upstream ended before generation", 15);
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "workbuddy_empty_response",
                        "WorkBuddy ended before producing a completion",
                    );
                }
            };
            if event.data == "[DONE]" {
                lease.failure("Upstream ended before generation", 15);
                return error(
                    StatusCode::BAD_GATEWAY,
                    "workbuddy_empty_response",
                    "WorkBuddy ended before producing a completion",
                );
            }
            if event.data.trim().is_empty() {
                continue;
            }
            let value = match serde_json::from_str(&event.data) {
                Ok(value) => value,
                Err(_) => {
                    lease.failure("Invalid upstream event", 15);
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "workbuddy_invalid_response",
                        "Invalid WorkBuddy event",
                    );
                }
            };
            match output.push(value) {
                Ok(frames) => initial.extend(frames),
                Err(message) => {
                    lease.failure("Upstream rejected the completion", 15);
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "workbuddy_upstream_rejected",
                        &message,
                    );
                }
            }
        }
        let body = async_stream::try_stream! {
            let _lease = lease; // Drop releases pool capacity on success, failure and client disconnect.
            for frame in initial { yield bytes::Bytes::from(frame); }
            while let Some(event) = events.next().await {
                let event = event.map_err(|_| { _lease.failure("Upstream stream interrupted", 15); std::io::Error::other("WorkBuddy stream interrupted") })?;
                if event.data == "[DONE]" { break; }
                if event.data.trim().is_empty() { continue; }
                let value: Value = serde_json::from_str(&event.data).map_err(|_| { _lease.failure("Invalid upstream event", 15); std::io::Error::other("Invalid WorkBuddy stream event") })?;
                for frame in output.push(value).map_err(|e| { _lease.failure("Upstream rejected generation", 15); std::io::Error::other(e) })? { yield bytes::Bytes::from(frame); }
            }
            let (_, frames) = output.end().map_err(|e| { _lease.failure("Incomplete upstream generation", 15); std::io::Error::other(e) })?;
            _lease.success();
            for frame in frames { yield bytes::Bytes::from(frame); }
        };
        return decorate(
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header("x-accel-buffering", "no")
                .body(Body::from_stream(
                    body.map(|item: Result<bytes::Bytes, std::io::Error>| item),
                ))
                .unwrap(),
            target,
        );
    }
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(_) => {
                lease.failure("Upstream stream interrupted", 15);
                return error(
                    StatusCode::BAD_GATEWAY,
                    "workbuddy_stream_interrupted",
                    "WorkBuddy stream interrupted",
                );
            }
        };
        if event.data == "[DONE]" {
            break;
        }
        if event.data.trim().is_empty() {
            continue;
        }
        let value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(_) => {
                lease.failure("Invalid upstream event", 15);
                return error(
                    StatusCode::BAD_GATEWAY,
                    "workbuddy_invalid_response",
                    "Invalid WorkBuddy event",
                );
            }
        };
        if let Err(message) = output.push(value) {
            lease.failure("Upstream rejected generation", 15);
            return error(
                StatusCode::BAD_GATEWAY,
                "workbuddy_invalid_response",
                &message,
            );
        }
    }
    match output.end() {
        Ok((value, _)) => {
            lease.success();
            decorate(Json(value).into_response(), target)
        }
        Err(message) => {
            lease.failure("Incomplete upstream generation", 15);
            error(
                StatusCode::BAD_GATEWAY,
                "workbuddy_incomplete_response",
                &message,
            )
        }
    }
}
fn decorate(mut response: Response, target: &str) -> Response {
    response.headers_mut().insert(
        "x-mapped-model",
        target.parse().expect("validated model id"),
    );
    response
        .headers_mut()
        .insert("x-upstream-provider", "workbuddy".parse().unwrap());
    response
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod configuration_tests {
    use super::*;
    use axum::{routing::post, Router};
    use tower::ServiceExt;

    async fn broken_config(request: Request, next: Next) -> Response {
        assert!(workbuddy::load().is_err());
        configuration_error(request, next, &Default::default()).await
    }

    #[tokio::test]
    async fn workbuddy_config_failure_rejects_known_models_but_preserves_other_requests() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        workbuddy::save(
            serde_json::from_value(json!({"enabled":false,"models":{"custom-wb":"native-model"}}))
                .unwrap(),
        )
        .unwrap();
        workbuddy::load().unwrap();
        std::fs::write(
            crate::modules::account::get_data_dir()
                .unwrap()
                .join("workbuddy.json"),
            b"{",
        )
        .unwrap();
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(|Json(body): Json<Value>| async move { Json(body) }),
            )
            .layer(axum::middleware::from_fn(broken_config));
        for (model, status) in [
            ("custom-wb", StatusCode::SERVICE_UNAVAILABLE),
            ("gemini-2.5-flash", StatusCode::OK),
        ] {
            let body = json!({"model":model,"messages":[{"role":"user","content":"hello"}]});
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/chat/completions")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            let data = to_bytes(response.into_body(), 4096).await.unwrap();
            let result: Value = serde_json::from_slice(&data).unwrap();
            if status == StatusCode::OK {
                assert_eq!(result, body);
            } else {
                assert_eq!(result["error"]["code"], "workbuddy_configuration_invalid");
            }
        }
    }
}
