//! Native account dispatch for Qoder/TRAE. Admission, billing and monitor middleware
//! have already run. One shared Chat codec/relay serves all three client protocols.
use super::{
    chat_codec as codec,
    chat_relay::{Events, Relay},
    error_response, published_request,
};
use crate::modules::native_providers::{
    self as native, accounts, http::Failure, Account, Provider,
};
use crate::proxy::{
    session_manager::SessionManager,
    sticky_config::{AccountSchedulingPolicy, SchedulingMode},
};
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::Response,
};
use eventsource_stream::Eventsource;
use futures::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};

type NativeEvents = BoxStream<'static, Result<String, Failure>>;

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
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(provider) = Provider::from_source(&published.source_id) else {
        return next.run(request).await;
    };
    let Some(protocol) = codec::Protocol::for_path(request.uri().path()) else {
        return failed(Failure::new(
            400,
            "This source supports Chat Completions, HTTP Responses and Messages",
        ));
    };
    let (parts, body) = request.into_parts();
    let limit = std::env::var("ABV_MAX_BODY_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50 * 1024 * 1024);
    let bytes = match to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) => return failed(Failure::new(413, "Request exceeds configured body limit")),
    };
    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return failed(Failure::new(400, "Expected a JSON request")),
    };
    let streaming = payload["stream"] == true;
    let namespaced = codec::namespaced_tools(&payload);
    let payload = match codec::request_for(protocol, payload, dialect(provider)) {
        Ok(p) => super::openai_compat::prepare(p, &published.native_model),
        Err(e) => return failed(Failure::new(400, e)),
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
    let session = SessionManager::account_session_key(scope, &payload);
    let policy = AccountSchedulingPolicy::live(&state.token_manager, None).await;
    let (events, account) = match open(
        provider,
        &published.native_model,
        payload,
        Duration::from_secs(state.request_timeout.clamp(1, 3600)),
        &session,
        &policy,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return failed(e),
    };
    let mut headers = HeaderMap::new();
    for (key, value) in [
        ("x-upstream-provider", provider.key().to_string()),
        (
            "x-account-email",
            format!("{}@{}", account.id, provider.key()),
        ),
        ("x-mapped-model", published.native_model),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(key, value);
        }
    }
    Relay {
        protocol,
        alias: published.public_id,
        namespaced,
        headers,
        error_code: "native_upstream_failed",
    }
    .run(events, streaming)
    .await
}
/// Request features each provider's wire payload actually carries. Qoder rebuilds its
/// own request without a structured-output field, so `response_format` / Responses
/// `text.format` must be rejected up front instead of silently running unconstrained.
/// TRAE forwards the Chat body, including `response_format`.
fn dialect(provider: Provider) -> codec::Dialect {
    codec::Dialect {
        structured_output: provider == Provider::Trae,
    }
}
fn failed(e: Failure) -> Response {
    let status = match e.status {
        400 | 404 | 413 | 422 | 504 => StatusCode::from_u16(e.status).unwrap(),
        429 => StatusCode::TOO_MANY_REQUESTS,
        503 => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    };
    error_response(status, "native_upstream_failed", &e.message)
}
async fn send(
    account: &Account,
    model: &str,
    payload: Value,
    timeout: Duration,
) -> Result<reqwest::Response, Failure> {
    match account.provider {
        Provider::Qoder => native::qoder::chat(account, model, payload, timeout).await,
        Provider::Trae => native::trae::chat(account, model, payload, timeout).await,
    }
}
fn normalize(provider: Provider, response: reqwest::Response) -> NativeEvents {
    let stream = async_stream::stream! {
        let mut events = response.bytes_stream().eventsource();
        let mut done = false; let mut finished = false; let mut has_tools = false;
        while let Some(event) = events.next().await {
            let event = match event { Ok(event) => event, Err(_) => { yield Err(Failure::new(502, "Provider stream connection interrupted")); return; } };
            if event.data.len() > 8 * 1024 * 1024 { yield Err(Failure::new(502, "Provider stream event exceeds size limit")); return; }
            match native::wire::event(provider, &event.event, &event.data) {
                Ok(Some(mut data)) => {
                    if data == "[DONE]" { done = true; yield Ok(data); break; }
                    if let Ok(mut value) = serde_json::from_str::<Value>(&data) {
                        has_tools |= value.pointer("/choices/0/delta/tool_calls").and_then(Value::as_array).is_some_and(|calls| !calls.is_empty());
                        if provider == Provider::Trae && event.event == "done" && serde_json::from_str::<Value>(&event.data).ok().is_some_and(|v| native::text(&v, &["finish_reason"]).is_empty()) {
                            value["choices"][0]["finish_reason"] = json!(if has_tools { "tool_calls" } else { "stop" }); data = value.to_string();
                        }
                        if value.pointer("/choices/0/finish_reason").is_some_and(|v| !v.is_null()) { finished = true; }
                    }
                    yield Ok(data);
                },
                Ok(None) => {}, Err(error) => { yield Err(error); return; },
            }
        }
        if !done {
            if finished { yield Ok("[DONE]".into()); } else { yield Err(Failure::new(502, "Provider stream ended before completion")); }
        }
    };
    stream.boxed()
}
async fn open(
    provider: Provider,
    model: &str,
    payload: Value,
    timeout: Duration,
    session: &str,
    policy: &AccountSchedulingPolicy,
) -> Result<(Events, Account), Failure> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut excluded = HashSet::new();
    let mut last = None;
    let mut failover_policy = policy.clone();
    failover_policy.sticky.max_wait_seconds = 0;
    for _ in 0..5 {
        let admission = if excluded.is_empty() {
            policy
        } else {
            &failover_policy
        };
        let mut lease = match accounts::scheduling::acquire(
            provider, model, session, admission, &excluded, deadline,
        )
        .await
        {
            Ok(a) => a,
            Err(e) => return Err(last.unwrap_or(e)),
        };
        let id = lease.account.id.clone();
        excluded.insert(id.clone());
        let refresh_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
        lease.account =
            match tokio::time::timeout(refresh_budget, accounts::fresh(provider, &id, false)).await
            {
                Ok(Ok(a)) => a,
                Ok(Err(e)) => {
                    lease.failure(e.status);
                    last = Some(e);
                    continue;
                }
                Err(_) => {
                    return Err(Failure::new(
                        504,
                        "Account refresh exceeded the request timeout",
                    ))
                }
            };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Failure::new(504, "Native upstream request timed out"));
        }
        let mut response = send(&lease.account, model, payload.clone(), remaining).await;
        if response
            .as_ref()
            .is_ok_and(|r| matches!(r.status().as_u16(), 401 | 403))
        {
            let refresh_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(refresh_budget, accounts::fresh(provider, &id, true)).await {
                Ok(Ok(account)) => {
                    lease.account = account;
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(Failure::new(504, "Native upstream request timed out"));
                    }
                    response = send(&lease.account, model, payload.clone(), remaining).await;
                }
                Ok(Err(error)) => response = Err(error),
                Err(_) => {
                    return Err(Failure::new(
                        504,
                        "Account refresh exceeded the request timeout",
                    ))
                }
            }
        }
        let retry_after = response
            .as_ref()
            .ok()
            .and_then(|r| r.headers().get("retry-after"))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let response = match response {
            Ok(r) if r.status().is_success() => r,
            result => {
                let error = match result {
                    Ok(r) => Failure::new(
                        r.status().as_u16(),
                        format!("{} returned HTTP {}", provider.name(), r.status().as_u16()),
                    ),
                    Err(e) => e,
                };
                if matches!(error.status, 400 | 413 | 422) {
                    return Err(error);
                }
                lease.failure_after(error.status, retry_after);
                // Cache First keeps a rate-limited conversation on its account;
                // its next request observes the same global bounded wait policy.
                if error.status == 429 && policy.sticky.mode == SchedulingMode::CacheFirst {
                    return Err(error);
                }
                last = Some(error);
                continue;
            }
        };
        let mut events = normalize(provider, response);
        let first = match events.next().await {
            Some(Ok(first)) if first != "[DONE]" => first,
            Some(Err(e)) => {
                lease.failure(e.status);
                if e.status == 429 && policy.sticky.mode == SchedulingMode::CacheFirst {
                    return Err(e);
                }
                last = Some(e);
                continue;
            }
            _ => {
                lease.failure(502);
                last = Some(Failure::new(502, "Provider returned an empty completion"));
                continue;
            }
        };
        let account = lease.account.clone();
        let guarded = async_stream::stream! {
            yield Ok(first);
            while let Some(event) = events.next().await {
                match event {
                    Ok(data) => { if data == "[DONE]" { lease.success(); } yield Ok(data); },
                    Err(e) => { lease.failure(e.status); yield Err(e.to_string()); return; },
                }
            }
        };
        return Ok((guarded.boxed(), account));
    }
    Err(last
        .unwrap_or_else(|| Failure::new(429, "All eligible accounts are temporarily unavailable")))
}

/// Deliberately one account and one inference attempt. The registry owns user consent,
/// timeout and evidence; discovery and login never invoke this function.
pub async fn probe(
    provider: Provider,
    model: &str,
    account_id: Option<&str>,
    system: &str,
    user: &str,
) -> Result<(String, String, Option<String>), String> {
    let excluded: HashSet<String> = native::store::all(provider)?
        .into_iter()
        .filter(|a| account_id.is_some_and(|id| id != a.id))
        .map(|a| a.id)
        .collect();
    let mut lease = accounts::select_for_probe(provider, model, &excluded, account_id)
        .map_err(|e| e.to_string())?;
    let a = accounts::fresh(provider, &lease.account.id, false)
        .await
        .map_err(|e| e.to_string())?;
    let payload = json!({"model":model,"messages":[{"role":"system","content":system},{"role":"user","content":user}],"stream":true,"max_tokens":64});
    let response = match send(&a, model, payload, Duration::from_secs(25)).await {
        Ok(response) => response,
        Err(error) => {
            lease.failure(error.status);
            return Err(error.to_string());
        }
    };
    if !response.status().is_success() {
        lease.failure(response.status().as_u16());
        return Ok((
            "failed".into(),
            "上游调用失败；已有验证证据保留。".into(),
            Some(a.id),
        ));
    }
    let mut events = normalize(provider, response);
    let mut text = false;
    let mut complete = false;
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                lease.failure(error.status);
                return Err(error.to_string());
            }
        };
        if event == "[DONE]" {
            complete = true;
            break;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&event) {
            text |= v
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty());
        }
    }
    if text && complete {
        lease.success();
        native::store::update(provider, &a.id, Some(&a.revision), |a| {
            a.verified_models.insert(model.into(), native::now());
        })?;
        Ok((
            "verified".into(),
            "账号基础文本调用通过；思考、视觉和工具能力未验证。".into(),
            Some(a.id),
        ))
    } else {
        Ok((
            "failed".into(),
            "上游未返回完整文本；已有验证证据保留。".into(),
            Some(a.id),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_session_identity_is_shared_across_protocols_and_isolates_users() {
        let requests = [
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"conversation start"}]}),
            ),
            (
                codec::Protocol::Responses,
                json!({"input":"conversation start"}),
            ),
            (
                codec::Protocol::Messages,
                json!({"messages":[{"role":"user","content":"conversation start"}]}),
            ),
        ];
        let mut identities = Vec::new();
        for (protocol, value) in requests {
            let canonical =
                codec::request_for(protocol, value, codec::Dialect::OPENAI_COMPATIBLE).unwrap();
            identities.push(SessionManager::account_session_key("user-a", &canonical));
            assert_ne!(
                identities.last().unwrap(),
                &SessionManager::account_session_key("user-b", &canonical)
            );
        }
        assert!(identities.iter().all(|id| id == &identities[0]));
    }
    fn response(body: &str) -> reqwest::Response {
        axum::http::Response::builder()
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::from(body.to_owned()))
            .unwrap()
            .into()
    }
    #[tokio::test]
    async fn trae_empty_done_keeps_tool_finish_reason_and_final_usage() {
        let body = "event: output\ndata: {\"tool_calls\":[{\"id\":\"call-1\",\"function_call\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}\n\nevent: done\ndata: {}\n\nevent: token_usage\ndata: {\"prompt_tokens\":9,\"completion_tokens\":2}\n\n";
        let events = normalize(Provider::Trae, response(body))
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok));
        let values: Vec<Value> = events
            .iter()
            .filter_map(|v| serde_json::from_str(v.as_ref().unwrap()).ok())
            .collect();
        assert!(values
            .iter()
            .any(|v| v.pointer("/choices/0/finish_reason") == Some(&json!("tool_calls"))));
        assert!(values
            .iter()
            .any(|v| v.pointer("/usage/prompt_tokens") == Some(&json!(9))));
        assert_eq!(events.last().unwrap().as_ref().unwrap(), "[DONE]");
    }
    #[test]
    fn qoder_rejects_structured_output_it_cannot_forward() {
        let chat = json!({"messages":[{"role":"user","content":"hi"}],"response_format":{"type":"json_object"}});
        let responses = json!({"input":"hi","text":{"format":{"type":"json_object"}}});
        for (protocol, body) in [
            (codec::Protocol::Chat, chat),
            (codec::Protocol::Responses, responses),
        ] {
            assert!(codec::request_for(protocol, body.clone(), dialect(Provider::Qoder)).is_err());
            let trae = codec::request_for(protocol, body, dialect(Provider::Trae)).unwrap();
            assert_eq!(trae["response_format"]["type"], "json_object");
        }
    }
    #[tokio::test]
    async fn truncated_stream_and_pre_generation_provider_errors_stay_failures() {
        let events = normalize(
            Provider::Trae,
            response("event: output\ndata: {\"response\":\"partial\"}\n\n"),
        )
        .collect::<Vec<_>>()
        .await;
        assert!(events.last().unwrap().is_err());
        let events = normalize(
            Provider::Qoder,
            response("data: {\"statusCodeValue\":429,\"body\":\"rate limited\"}\n\n"),
        )
        .collect::<Vec<_>>()
        .await;
        assert_eq!(events[0].as_ref().unwrap_err().status, 429);
    }
}
