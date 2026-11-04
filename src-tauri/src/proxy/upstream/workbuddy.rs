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

use super::chat_codec as codec;
use super::{error_response as error, published_request};

use crate::modules::workbuddy::{self, Settings};
use workbuddy::accounts::scheduling::Policy;

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
    let (request, published) = match published_request(request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if published.source_id != crate::modules::model_registry::WORKBUDDY_SOURCE {
        return next.run(request).await;
    }
    let settings = match workbuddy::load() {
        Ok(settings) => settings,
        Err(_) => return configuration_error(),
    };
    let policy = Policy::live(&state.token_manager, settings.preferred_account_id.clone()).await;
    dispatch(
        settings,
        published,
        state.request_timeout,
        policy,
        request,
        next,
    )
    .await
}

fn configuration_error() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "workbuddy_configuration_invalid",
        "Repair the WorkBuddy upstream configuration in admin settings.",
    )
}

fn is_inference_path(path: &str) -> bool {
    crate::proxy::middleware::model_registry::is_inference_path(path)
}

async fn dispatch(
    settings: Settings,
    published: crate::modules::model_registry::PublishedModel,
    timeout: u64,
    policy: Policy,
    request: Request,
    next: Next,
) -> Response {
    // The registry binding is authoritative. Legacy settings.models aliases must
    // never steal an AG name or rewrite an explicitly published WB native ID.
    if published.source_id != crate::modules::model_registry::WORKBUDDY_SOURCE {
        return next.run(request).await;
    }
    let path = request.uri().path().to_owned();
    let alias = published.public_id;
    let target = published.native_model;
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
    let namespaced = codec::namespaced_tools(&payload);
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
        timeout,
        payload,
        streaming,
        &alias,
        &target,
        protocol,
        &namespaced,
        scope,
        policy,
    )
    .await
}

/// Extra accounts tried only when the first attempt never received an HTTP response.
const MAX_CONNECTION_FAILOVERS: usize = 1;

/// An attempt that failed before any byte reached the client. `retry` marks
/// failures another account may resolve; request-shape and deadline failures are final.
struct Failure {
    response: Response,
    retry: bool,
}

fn retry(status: StatusCode, code: &str, message: &str) -> Failure {
    Failure {
        response: error(status, code, message),
        retry: true,
    }
}

fn fatal(status: StatusCode, code: &str, message: &str) -> Failure {
    Failure {
        response: error(status, code, message),
        retry: false,
    }
}

/// Another account cannot fix a request the upstream rejected for its shape or size.
fn retryable_status(status: StatusCode) -> bool {
    !matches!(status.as_u16(), 400 | 413 | 422)
}

/// Tencent's channel check: the request content (in practice the opening of a system
/// message) matched its list of third-party agent prompts. Retrying elsewhere or
/// refreshing the account cannot change it.
const CHANNEL_REJECTED_CODE: i64 = 11128;
const CHANNEL_REJECTED_MESSAGE: &str = "WorkBuddy rejected this request with code 11128 (channel check). The upstream flags system messages that open like a known third-party agent prompt; the gateway already adds a neutral first line to common agent openings. Change how the system prompt begins and retry. This is not an account, quota or network problem, and another account would give the same result.";

/// Reads a bounded, redacted prefix of a rejected response so the upstream's reason
/// reaches the log, plus the result code `client::chat` keeps in the body.
async fn rejection_detail(
    upstream: rquest::Response,
    deadline: tokio::time::Instant,
) -> (String, Option<i64>) {
    let read = async {
        let mut body = Vec::new();
        let mut chunks = upstream.bytes_stream();
        while let Some(Ok(chunk)) = chunks.next().await {
            body.extend_from_slice(&chunk);
            if body.len() >= 4096 {
                break;
            }
        }
        body
    };
    let wait = deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .min(std::time::Duration::from_secs(2));
    match tokio::time::timeout(wait, read).await {
        Ok(body) if body.is_empty() => ("<empty body>".into(), None),
        Ok(body) => (
            crate::proxy::upstream::client::sanitize_error_for_log(
                String::from_utf8_lossy(&body).trim(),
            ),
            serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| workbuddy::client::upstream_error_code(&value)),
        ),
        Err(_) => ("<body not received>".into(), None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward(
    timeout: u64,
    payload: Value,
    streaming: bool,
    alias: &str,
    target: &str,
    protocol: codec::Protocol,
    namespaced: &codec::Namespaced,
    scope: &str,
    policy: Policy,
) -> Response {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(timeout.clamp(1, 3600));
    let session = workbuddy::client::session_key(scope, &payload);
    // A model the upstream is rate-limiting across accounts is held here, before it can
    // spend another account. The probe guard (if any) lives until this request ends.
    let probe = match workbuddy::accounts::scheduling::admit_model(target, &policy) {
        Ok(probe) => probe,
        Err(limited) => return model_limited(target, &limited),
    };
    let admission = if probe.is_some() {
        policy.for_probe()
    } else {
        policy.clone()
    };
    let mut lease =
        match workbuddy::accounts::scheduling::acquire(target, &session, &admission, timeout).await
        {
            Ok(lease) => lease,
            Err(message) => {
                // Every account cooling for this model is a rate limit, not an outage.
                if let Some(limited) =
                    workbuddy::accounts::scheduling::hold_cooling_model(target, &policy)
                {
                    return model_limited(target, &limited);
                }
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "workbuddy_account_unavailable",
                    &message,
                );
            }
        };
    let _probe = probe;
    let mut tried = Vec::new();
    let mut failovers = 0usize;
    loop {
        tried.push(lease.account.id.clone());
        let failure = match attempt(
            lease, &payload, streaming, alias, target, protocol, namespaced, scope, &policy,
            deadline,
        )
        .await
        {
            Ok(response) => return response,
            Err(failure) => failure,
        };
        // Only the response of the attempt that ends the loop reaches monitor and
        // settlement, so a failed-then-served request bills once.
        // An HTTP response — including 401, 403, 429 and channel rejection — stays
        // on the account that received it.
        if !failure.retry
            || failovers >= MAX_CONNECTION_FAILOVERS
            || tokio::time::Instant::now() >= deadline
        {
            return failure.response;
        }
        failovers += 1;
        lease = match workbuddy::accounts::scheduling::try_acquire_excluding(
            target, &session, &policy, &tried,
        ) {
            Ok(Some(next)) => next,
            _ => return failure.response,
        };
        tracing::warn!(
            "WorkBuddy account failed before generation (HTTP {}); retrying on another account",
            failure.response.status().as_u16()
        );
    }
}

/// One account's attempt. Every `Err` is returned before a byte reaches the client;
/// once output exists the attempt commits, so a retry can never duplicate output.
#[allow(clippy::too_many_arguments)]
async fn attempt(
    lease: workbuddy::accounts::Lease,
    payload: &Value,
    streaming: bool,
    alias: &str,
    target: &str,
    protocol: codec::Protocol,
    namespaced: &codec::Namespaced,
    scope: &str,
    policy: &Policy,
    deadline: tokio::time::Instant,
) -> Result<Response, Failure> {
    let account = match workbuddy::accounts::fresh_before(&lease.account.id, deadline).await {
        Ok(Ok(account)) if account.enabled => account,
        Ok(Ok(_)) => {
            return Err(fatal(
                StatusCode::SERVICE_UNAVAILABLE,
                "workbuddy_account_disabled",
                "WorkBuddy account was disabled",
            ))
        }
        Ok(Err(message)) => {
            lease.failure("Account authorization failed", 30);
            return Err(fatal(
                StatusCode::SERVICE_UNAVAILABLE,
                "workbuddy_authorization_failed",
                &message,
            ));
        }
        Err(_) => {
            return Err(fatal(
                StatusCode::GATEWAY_TIMEOUT,
                "workbuddy_timeout",
                "WorkBuddy request timed out while preparing account authorization",
            ));
        }
    };
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(fatal(
            StatusCode::GATEWAY_TIMEOUT,
            "workbuddy_timeout",
            "WorkBuddy request timed out before generation",
        ));
    }
    let upstream =
        match workbuddy::client::chat(&account, target, payload.clone(), scope, deadline).await {
            Ok(response) => response,
            Err(error) => {
                return Err(match error {
                    workbuddy::client::ChatError::Connection(message) => {
                        lease.failure("Upstream connection failed", 10);
                        retry(StatusCode::BAD_GATEWAY, "workbuddy_unavailable", &message)
                    }
                    workbuddy::client::ChatError::Request(message) => fatal(
                        StatusCode::BAD_REQUEST,
                        "workbuddy_request_unsupported",
                        &message,
                    ),
                    workbuddy::client::ChatError::Account(message) => {
                        lease.failure("Account data cannot form a request", 60);
                        fatal(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "workbuddy_account_invalid",
                            &message,
                        )
                    }
                    workbuddy::client::ChatError::Timeout(message) => {
                        fatal(StatusCode::GATEWAY_TIMEOUT, "workbuddy_timeout", &message)
                    }
                });
            }
        };
    if !upstream.status().is_success() {
        let status = upstream.status();
        let retry_after = upstream
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok());
        let (detail, code) = rejection_detail(upstream, deadline).await;
        tracing::warn!(
            "WorkBuddy rejected model {} on account {} with HTTP {}: {}",
            target,
            lease.account.id,
            status.as_u16(),
            detail
        );
        // A channel rejection is about the request content, whatever HTTP status
        // carries it: it must not cool the account for every other caller.
        if code == Some(CHANNEL_REJECTED_CODE) {
            return Err(fatal(
                StatusCode::BAD_REQUEST,
                "workbuddy_channel_rejected",
                CHANNEL_REJECTED_MESSAGE,
            ));
        }
        // A rate limit is about this model (its quota or upstream throttling): it cools
        // only this model on this account and counts toward the model-wide circuit.
        // Like every HTTP response it stays on this account rather than walking the pool.
        if status == StatusCode::TOO_MANY_REQUESTS {
            let signature =
                code.map_or_else(|| "no result code".to_owned(), |c| format!("code {c}"));
            let minimum = retry_after.filter(|s| *s > 0).unwrap_or(60);
            let cooled = lease.model_failure(
                &format!("Upstream HTTP 429 for {target} ({signature})"),
                minimum,
            );
            let held = workbuddy::accounts::scheduling::record_model_limit(
                target,
                &lease.account.id,
                &signature,
                minimum,
                policy,
            );
            let wait = held.unwrap_or(cooled).max(minimum);
            let mut response = error(
                StatusCode::TOO_MANY_REQUESTS,
                "workbuddy_upstream_rate_limited",
                &format!("WorkBuddy is rate-limiting model {target} (HTTP 429, {signature}). This is a model quota or upstream rate limit, not an authorization problem; retry after {wait}s or use another model."),
            );
            set_retry_after(&mut response, wait);
            return Err(Failure {
                response,
                retry: false,
            });
        }
        // A request rejected for its own shape says nothing about the account, so it
        // must not cool the account down for every other caller.
        if retryable_status(status) {
            lease.failure(
                &format!("Upstream HTTP {}", status.as_u16()),
                retry_after.unwrap_or(15),
            );
        }
        return Err(Failure {
            response: error(
                StatusCode::BAD_GATEWAY,
                "workbuddy_upstream_rejected",
                &format!("WorkBuddy returned HTTP {}; refresh account authorization or check model availability", status.as_u16()),
            ),
            retry: false,
        });
    }
    // Only a rate limit opens the model circuit, so any accepted response closes it.
    workbuddy::accounts::scheduling::model_accepted(target);
    if !upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        lease.failure("Invalid upstream response type", 15);
        return Err(fatal(
            StatusCode::BAD_GATEWAY,
            "workbuddy_invalid_response",
            "WorkBuddy did not return a model event stream; check account authorization",
        ));
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
    let mut output = codec::Output::new(protocol, alias).with_namespaced(namespaced.clone());
    if streaming {
        // Inspect provider errors before committing HTTP 200. Shared billing must
        // see a failed status when no generation has started (including per-call prices).
        let mut initial = output.start();
        while !output.has_output() {
            let event = match events.next().await {
                Some(Ok(event)) if event.data != "[DONE]" => event,
                _ => {
                    lease.failure("Upstream ended before generation", 15);
                    return Err(fatal(
                        StatusCode::BAD_GATEWAY,
                        "workbuddy_empty_response",
                        "WorkBuddy ended before producing a completion",
                    ));
                }
            };
            if event.data.trim().is_empty() {
                continue;
            }
            let value = match serde_json::from_str(&event.data) {
                Ok(value) => value,
                Err(_) => {
                    lease.failure("Invalid upstream event", 15);
                    return Err(fatal(
                        StatusCode::BAD_GATEWAY,
                        "workbuddy_invalid_response",
                        "Invalid WorkBuddy event",
                    ));
                }
            };
            match output.push(value) {
                Ok(frames) => initial.extend(frames),
                Err(message) => {
                    lease.failure("Upstream rejected the completion", 15);
                    return Err(fatal(
                        StatusCode::BAD_GATEWAY,
                        "workbuddy_upstream_rejected",
                        &message,
                    ));
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
        return Ok(decorate(
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header("x-accel-buffering", "no")
                .body(Body::from_stream(
                    body.map(|item: Result<bytes::Bytes, std::io::Error>| item),
                ))
                .unwrap(),
            target,
            &account,
        ));
    }
    // The upstream already accepted the request. A later stream error stays here.
    let failed = |code: &str, message: &str| Failure {
        response: error(StatusCode::BAD_GATEWAY, code, message),
        retry: false,
    };
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(_) => {
                lease.failure("Upstream stream interrupted", 15);
                return Err(failed(
                    "workbuddy_stream_interrupted",
                    "WorkBuddy stream interrupted",
                ));
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
                return Err(failed(
                    "workbuddy_invalid_response",
                    "Invalid WorkBuddy event",
                ));
            }
        };
        if let Err(message) = output.push(value) {
            lease.failure("Upstream rejected generation", 15);
            return Err(failed("workbuddy_invalid_response", &message));
        }
    }
    match output.end() {
        Ok((value, _)) => {
            lease.success();
            Ok(decorate(Json(value).into_response(), target, &account))
        }
        Err(message) => {
            lease.failure("Incomplete upstream generation", 15);
            Err(failed("workbuddy_incomplete_response", &message))
        }
    }
}

fn set_retry_after(response: &mut Response, seconds: i64) {
    if let Ok(value) = seconds.max(1).to_string().parse() {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
}

/// The local answer while a model's circuit is open: no account is spent on it.
fn model_limited(
    target: &str,
    limited: &workbuddy::accounts::scheduling::ModelLimited,
) -> Response {
    let mut response = error(
        StatusCode::TOO_MANY_REQUESTS,
        "workbuddy_model_rate_limited",
        &format!(
            "WorkBuddy is rate-limiting model {target} across {} account(s) ({}). The gateway is holding requests for this model instead of spending more accounts; retry after {}s or use another model.",
            limited.accounts, limited.reason, limited.retry_after
        ),
    );
    set_retry_after(&mut response, limited.retry_after);
    response
}

/// Token Stats key (`X-Account-Email`). Identity is realm + uid only.
///
/// Re-authorization replaces the nickname (`accounts::upsert` matches on
/// realm+uid and stores the new nickname). Token Stats groups by the full
/// header value, so the nickname stays on the account record and is not part
/// of this key. The uid is percent-encoded so separators such as `+` and `@`
/// cannot alias two accounts (an empty nickname plus uid `Ada+42` used to
/// collide with nickname `Ada` plus uid `42`).
fn account_stats_key(account: &workbuddy::accounts::Account) -> String {
    let realm = match account.realm {
        workbuddy::accounts::Realm::Cn => "cn",
        workbuddy::accounts::Realm::Global => "global",
    };
    format!("{}@workbuddy-{realm}", encode_stats_uid(&account.uid))
}

/// RFC 3986 unreserved bytes stay literal. Every other byte is `%HH` (uppercase),
/// which is injective and a valid header value even when the uid contains `+` or `@`.
fn encode_stats_uid(uid: &str) -> String {
    let mut out = String::with_capacity(uid.len());
    for byte in uid.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

fn decorate(
    mut response: Response,
    target: &str,
    account: &workbuddy::accounts::Account,
) -> Response {
    response.headers_mut().insert(
        "x-mapped-model",
        target.parse().expect("validated model id"),
    );
    response
        .headers_mut()
        .insert("x-upstream-provider", "workbuddy".parse().unwrap());
    // Mirror Antigravity's X-Account-Email so token_stats.record_usage can attribute usage.
    if let Ok(value) = account_stats_key(account).parse() {
        response.headers_mut().insert("x-account-email", value);
    }
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
        let (request, published) = match published_request(request).await {
            Ok(value) => value,
            Err(response) => return response,
        };
        if published.source_id == crate::modules::model_registry::WORKBUDDY_SOURCE {
            configuration_error()
        } else {
            next.run(request).await
        }
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
        crate::modules::model_registry::view(
            &["custom-wb".into(), "gemini-2.5-flash".into()],
            &Default::default(),
        )
        .unwrap();
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
