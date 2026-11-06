//! Native account dispatch (Qoder). Admission, billing and monitor middleware
//! have already run. One shared Chat codec/relay serves all three client protocols.
use super::{
    chat_codec as codec,
    chat_relay::{Events, Relay},
    error_response, published_request,
};
use crate::modules::native_providers::{
    self as native, accounts,
    http::{DispatchFailure, Failure},
    Account, Provider,
};
use crate::proxy::{session_manager::SessionManager, sticky_config::AccountSchedulingPolicy};
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
    if let Err(e) = representable(provider, &payload) {
        return failed(e);
    }
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
    let targets: Vec<String> = published
        .native_models()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let (events, account, native_model) = match open(
        provider,
        &targets,
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
        ("x-mapped-model", native_model),
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
fn dialect(provider: Provider) -> codec::Dialect {
    match provider {
        Provider::Qoder => codec::Dialect {
            structured_output: false,
        },
    }
}
/// Request constraints a provider's wire payload cannot carry are request errors (400)
/// raised before account selection, never silently dropped. Qoder has no tool-selection
/// or parallel-call field: it honors `auto` and `none` (by offering no tools), not
/// `required`, a forced function, or `parallel_tool_calls: false` (also produced from
/// Messages `disable_parallel_tool_use`) while tools are offered.
fn representable(provider: Provider, chat: &Value) -> Result<(), Failure> {
    match provider {
        Provider::Qoder => {}
    }
    let choice = chat.get("tool_choice");
    if choice.is_some_and(|choice| choice != "auto" && choice != "none") {
        return Err(Failure::new(
            400,
            "This source supports tool_choice auto or none only",
        ));
    }
    let offers_tools = choice.is_none_or(|choice| choice != "none")
        && chat["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty());
    if offers_tools && chat["parallel_tool_calls"] == false {
        return Err(Failure::new(
            400,
            "This source cannot restrict parallel tool calls; omit parallel_tool_calls=false",
        ));
    }
    Ok(())
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
) -> Result<reqwest::Response, DispatchFailure> {
    let deadline = tokio::time::Instant::now() + timeout;
    match account.provider {
        Provider::Qoder => {
            let client = tokio::time::timeout_at(deadline, native::http::client_for(account))
                .await
                .map_err(|_| Failure::new(504, "Qoder egress resolution timed out"))??;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(Failure::new(504, "Qoder request timed out before dispatch").into());
            }
            native::qoder::chat(&client, account, model, payload, remaining).await
        }
    }
}
fn normalize(provider: Provider, response: reqwest::Response) -> NativeEvents {
    let stream = async_stream::stream! {
        let mut events = response.bytes_stream().eventsource();
        let mut done = false; let mut finished = false;
        while let Some(event) = events.next().await {
            let event = match event { Ok(event) => event, Err(_) => { yield Err(Failure::new(502, "Provider stream connection interrupted")); return; } };
            if event.data.len() > 8 * 1024 * 1024 { yield Err(Failure::new(502, "Provider stream event exceeds size limit")); return; }
            match native::wire::event(provider, &event.data) {
                Ok(Some(data)) => {
                    if data == "[DONE]" { done = true; yield Ok(data); break; }
                    if let Ok(value) = serde_json::from_str::<Value>(&data) {
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
/// Whether a normalized Chat chunk carries completion output: text, reasoning, tool
/// calls or a finish reason. Role-only and usage-only chunks do not. Unrecognized
/// chunks count as output so they are never withheld from the client.
fn carries_output(chunk: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(chunk) else {
        return true;
    };
    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        return value.get("usage").is_none();
    };
    choices.iter().any(|choice| {
        let delta = &choice["delta"];
        !choice["finish_reason"].is_null()
            || ["content", "reasoning_content"]
                .iter()
                .any(|key| delta[*key].as_str().is_some_and(|s| !s.is_empty()))
            || delta["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
    })
}

/// Buffers leading non-output chunks and returns them with the first output chunk. A
/// stream error or an end of stream before any output is returned as a typed failure.
async fn until_output(events: &mut NativeEvents) -> Result<Vec<String>, Failure> {
    let mut prelude = Vec::new();
    while let Some(event) = events.next().await {
        let chunk = event?;
        if chunk == "[DONE]" {
            break;
        }
        let output = carries_output(&chunk);
        prelude.push(chunk);
        if output {
            return Ok(prelude);
        }
        if prelude.len() > 64 {
            return Err(Failure::new(
                502,
                "Provider stream produced no completion output",
            ));
        }
    }
    Err(Failure::new(502, "Provider returned an empty completion"))
}
/// How far a failed attempt got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reached {
    /// Inference was not sent: refresh, routing or request preparation failed first.
    Nothing,
    /// Transport was invoked but no HTTP response arrived; delivery is uncertain.
    Connection,
    /// The upstream answered (an HTTP error, or a stream error before output).
    Upstream,
}
/// Extra accounts tried after connection failures. An upstream answer, including 401,
/// 403, 429 and a stream error, stays on the account that received it: replaying the
/// same request elsewhere spreads one rejection across the pool.
const MAX_CONNECTION_FAILOVERS: usize = 1;
fn allow_failover(reached: Reached, connection_failovers: &mut usize) -> bool {
    match reached {
        Reached::Nothing => true,
        Reached::Connection if *connection_failovers < MAX_CONNECTION_FAILOVERS => {
            *connection_failovers += 1;
            true
        }
        Reached::Connection | Reached::Upstream => false,
    }
}
fn dispatch_failure(error: DispatchFailure, upstream_answered: bool) -> (Failure, Reached) {
    let (error, reached) = match error {
        DispatchFailure::Preparation(error) => (error, Reached::Nothing),
        DispatchFailure::Connection(error) => (error, Reached::Connection),
    };
    // A failed same-account authorization retry must not erase the original
    // upstream rejection and allow a replay on another account.
    (
        error,
        if upstream_answered {
            Reached::Upstream
        } else {
            reached
        },
    )
}
/// The request body for one attempt names the native ID leased for that account.
fn attempt_payload(payload: &Value, model: &str) -> Value {
    let mut attempt = payload.clone();
    attempt["model"] = json!(model);
    attempt
}
async fn open(
    provider: Provider,
    targets: &[String],
    payload: Value,
    timeout: Duration,
    session: &str,
    policy: &AccountSchedulingPolicy,
) -> Result<(Events, Account, String), Failure> {
    let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut excluded = HashSet::new();
    let mut last = None;
    let mut failover_policy = policy.clone();
    failover_policy.sticky.max_wait_seconds = 0;
    let mut connection_failovers = 0;
    for _ in 0..5 {
        let admission = if excluded.is_empty() {
            policy
        } else {
            &failover_policy
        };
        let mut lease = match accounts::scheduling::acquire(
            provider, &targets, session, admission, &excluded, deadline,
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
        let model = lease.model.clone();
        let attempt = attempt_payload(&payload, &model);
        let mut response = send(&lease.account, &model, attempt.clone(), remaining).await;
        let upstream_answered = response
            .as_ref()
            .is_ok_and(|r| matches!(r.status().as_u16(), 401 | 403));
        if upstream_answered {
            let refresh_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(refresh_budget, accounts::fresh(provider, &id, true)).await {
                Ok(Ok(account)) => {
                    lease.account = account;
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(Failure::new(504, "Native upstream request timed out"));
                    }
                    response = send(&lease.account, &model, attempt.clone(), remaining).await;
                }
                Ok(Err(error)) => {
                    // The upstream already answered 401/403 for this account.
                    lease.failure(error.status);
                    return Err(error);
                }
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
                let (error, reached) = match result {
                    Ok(r) => (
                        Failure::new(
                            r.status().as_u16(),
                            format!("{} returned HTTP {}", provider.name(), r.status().as_u16()),
                        ),
                        Reached::Upstream,
                    ),
                    Err(e) => dispatch_failure(e, upstream_answered),
                };
                if matches!(error.status, 400 | 413 | 422) {
                    return Err(error);
                }
                lease.failure_after(error.status, retry_after);
                if !allow_failover(reached, &mut connection_failovers) {
                    return Err(error);
                }
                tracing::warn!(
                    "{} account attempt failed at {:?}; trying another account",
                    provider.name(),
                    reached
                );
                last = Some(error);
                continue;
            }
        };
        let mut events = normalize(provider, response);
        // Nothing reaches the client until generation starts, so a failure before then
        // is still returned as a typed error instead of a broken stream.
        let prelude = match until_output(&mut events).await {
            Ok(prelude) => prelude,
            Err(e) => {
                // The upstream answered; another account would receive the same request.
                lease.failure(e.status);
                return Err(e);
            }
        };
        let account = lease.account.clone();
        let guarded = async_stream::stream! {
            for chunk in prelude { yield Ok(chunk); }
            while let Some(event) = events.next().await {
                match event {
                    Ok(data) => { if data == "[DONE]" { lease.success(); } yield Ok(data); },
                    Err(e) => { lease.failure(e.status); yield Err(e.to_string()); return; },
                }
            }
        };
        return Ok((guarded.boxed(), account, model));
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
            let error = Failure::from(error);
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
    fn attempt_payload_names_the_leased_native_id_without_touching_the_original() {
        let original = json!({"model":"shared-model","messages":[]});
        let attempt = attempt_payload(&original, "shared-model-sg");
        assert_eq!(attempt["model"], "shared-model-sg");
        assert_eq!(attempt["messages"], original["messages"]);
        assert_eq!(original["model"], "shared-model");
    }
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
    #[test]
    fn qoder_rejects_structured_output_it_cannot_forward() {
        let chat = json!({"messages":[{"role":"user","content":"hi"}],"response_format":{"type":"json_object"}});
        let responses = json!({"input":"hi","text":{"format":{"type":"json_object"}}});
        for (protocol, body) in [
            (codec::Protocol::Chat, chat),
            (codec::Protocol::Responses, responses),
        ] {
            assert!(codec::request_for(protocol, body, dialect(Provider::Qoder)).is_err());
        }
    }
    #[test]
    fn qoder_rejects_tool_choices_it_cannot_forward() {
        let tools =
            json!([{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]);
        let requests = [
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":tools,"tool_choice":"required"}),
                false,
            ),
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":tools,"tool_choice":{"type":"function","function":{"name":"read"}}}),
                false,
            ),
            (
                codec::Protocol::Messages,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"read","input_schema":{"type":"object"}}],"tool_choice":{"type":"any"}}),
                false,
            ),
            (
                codec::Protocol::Messages,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"read","input_schema":{"type":"object"}}],"tool_choice":{"type":"none"}}),
                true,
            ),
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":tools,"tool_choice":"auto"}),
                true,
            ),
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":tools}),
                true,
            ),
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":tools,"parallel_tool_calls":false}),
                false,
            ),
            (
                codec::Protocol::Messages,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"read","input_schema":{"type":"object"}}],"tool_choice":{"type":"auto","disable_parallel_tool_use":true}}),
                false,
            ),
            // No tools are offered, so there is nothing to restrict.
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":tools,"tool_choice":"none","parallel_tool_calls":false}),
                true,
            ),
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"hi"}],"parallel_tool_calls":false}),
                true,
            ),
            (
                codec::Protocol::Chat,
                json!({"messages":[{"role":"user","content":"hi"}],"tools":tools,"parallel_tool_calls":true}),
                true,
            ),
        ];
        for (protocol, body, accepted) in requests {
            let chat = codec::request_for(protocol, body, dialect(Provider::Qoder)).unwrap();
            assert_eq!(
                representable(Provider::Qoder, &chat).is_ok(),
                accepted,
                "{chat}"
            );
        }
    }
    #[test]
    fn only_unsent_and_connection_failures_move_to_another_account() {
        let mut used = 0;
        for _ in 0..3 {
            assert!(allow_failover(Reached::Nothing, &mut used));
        }
        assert_eq!(used, 0);
        assert!(allow_failover(Reached::Connection, &mut used));
        assert_eq!(used, MAX_CONNECTION_FAILOVERS);
        assert!(allow_failover(Reached::Nothing, &mut used));
        assert!(!allow_failover(Reached::Connection, &mut used));
        assert!(!allow_failover(Reached::Upstream, &mut 0));
    }
    #[tokio::test]
    async fn route_failures_stay_unsent_and_preserve_the_connection_retry_allowance() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        std::fs::write(
            crate::modules::account::get_data_dir()
                .unwrap()
                .join("workbuddy_egress.json"),
            "invalid egress settings",
        )
        .unwrap();
        let mut used = 0;
        // Both accounts fail route resolution before HTTP or the UMID helper.
        for _ in 0..2 {
            let a = Account::new(Provider::Qoder, native::Realm::Cn);
            let error = send(&a, "model", json!({}), Duration::from_secs(1))
                .await
                .unwrap_err();
            let (error, reached) = dispatch_failure(error, false);
            assert!(error.message.contains("egress"));
            assert_eq!(reached, Reached::Nothing);
            assert!(allow_failover(reached, &mut used));
            assert_eq!(used, 0);
        }
        // A later account can still use the one connection failover if needed.
        assert!(allow_failover(Reached::Connection, &mut used));
    }
    #[test]
    fn authorization_retry_failure_does_not_replay_an_upstream_rejection() {
        for error in [
            DispatchFailure::Preparation(Failure::new(503, "route unavailable")),
            DispatchFailure::Connection(Failure::new(502, "connection failed")),
        ] {
            let (_, reached) = dispatch_failure(error, true);
            assert_eq!(reached, Reached::Upstream);
            assert!(!allow_failover(reached, &mut 0));
        }
    }
    #[tokio::test]
    async fn failures_before_generation_output_are_typed_errors() {
        // A role-only chunk, then an upstream error: nothing has been generated yet.
        let mut events = normalize(
            Provider::Qoder,
            response("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\ndata: {\"statusCodeValue\":503,\"body\":\"busy\"}\n\n"),
        );
        assert_eq!(until_output(&mut events).await.unwrap_err().status, 503);
        // Leading role-only and usage chunks are kept, in order, ahead of the first output.
        let mut events = normalize(
            Provider::Qoder,
            response("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"),
        );
        let prelude = until_output(&mut events).await.unwrap();
        assert_eq!(prelude.len(), 3);
        assert!(prelude[2].contains("\"hi\""));
        assert_eq!(events.next().await.unwrap().unwrap(), "[DONE]");
        // A stream that finishes without any output is an empty completion.
        let mut events = normalize(
            Provider::Qoder,
            response("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"),
        );
        assert_eq!(until_output(&mut events).await.unwrap_err().status, 502);
        // A finish reason alone (e.g. after tool output) counts as output.
        assert!(carries_output(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#
        ));
    }
    #[tokio::test]
    async fn truncated_stream_and_pre_generation_provider_errors_stay_failures() {
        let events = normalize(
            Provider::Qoder,
            response("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n"),
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
