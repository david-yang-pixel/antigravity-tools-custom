//! Native account dispatch (Qoder, ChatGPT). Admission, billing and monitor middleware
//! have already run. Chat-wire providers share one Chat codec/relay for all three client
//! protocols; ChatGPT speaks Responses, bridged to Chat/Messages clients and passed
//! through to Responses clients.
#[path = "native_chatgpt.rs"]
mod chatgpt;
use super::{
    chat_codec as codec,
    chat_relay::{Events, Relay},
    error_response, published_request,
    responses_relay::ResponsesRelay,
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

/// How one exchange's upstream answer is normalized and when it counts as output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WireMode {
    /// Chat chunks from a Chat-wire provider.
    Chat,
    /// Responses events converted to Chat chunks.
    ResponsesToChat,
    /// Responses events relayed as they are.
    Passthrough,
    /// One JSON answer (compaction).
    Unary,
}
impl WireMode {
    fn carries_output(self) -> fn(&str) -> bool {
        match self {
            Self::Chat | Self::ResponsesToChat => carries_output,
            Self::Passthrough => chatgpt::carries_event,
            Self::Unary => |_| true,
        }
    }
}
/// Everything an attempt sends besides the body.
#[derive(Clone, Copy)]
struct Call<'a> {
    mode: WireMode,
    /// Client headers; providers forward only their own allow-list.
    forwarded: &'a HeaderMap,
    /// Tenant scope and conversation key of provider-side session ids.
    scope: &'a str,
    conversation: &'a str,
}
/// The upstream body, wire mode and conversation keys of one client request.
struct Prepared {
    body: Value,
    mode: WireMode,
    /// Scheduler affinity key.
    session: String,
    /// Key of provider-side session ids (ChatGPT: `prompt_cache_key`, else `session`).
    conversation: String,
}
/// Why `open` gave up: the classified failure and, when the upstream answered with its own
/// error object, that object for clients that speak the upstream protocol.
struct Refusal {
    failure: Failure,
    upstream: Option<Value>,
}
impl From<Failure> for Refusal {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            upstream: None,
        }
    }
}
/// Accounts one ChatGPT request may try when answers are classified as failover.
const MAX_CHATGPT_ACCOUNTS: usize = 3;
/// Leases one Qoder request may take (unchanged).
const MAX_QODER_ATTEMPTS: usize = 5;
const COMPACT_PATH: &str = "/responses/compact";

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
    let protocol = codec::Protocol::for_path(request.uri().path());
    let compact = request.uri().path() == COMPACT_PATH && provider == Provider::ChatGpt;
    if protocol.is_none() && !compact {
        return failed(Failure::new(
            400,
            "This source supports Chat Completions, HTTP Responses and Messages",
        ));
    }
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
    let scope = parts
        .extensions
        .get::<crate::proxy::middleware::auth::UserTokenIdentity>()
        .map(|identity| identity.token_id.as_str())
        .unwrap_or("administrator");
    let prepared = match provider {
        Provider::Qoder => {
            let protocol = protocol.expect("checked above");
            let payload = match codec::request_for(protocol, payload, dialect(provider)) {
                Ok(p) => super::openai_compat::prepare(p, &published.native_model),
                Err(e) => return failed(Failure::new(400, e)),
            };
            if let Err(e) = representable(provider, &payload) {
                return failed(e);
            }
            let session = SessionManager::account_session_key(scope, &payload);
            Prepared {
                body: payload,
                mode: WireMode::Chat,
                conversation: session.clone(),
                session,
            }
        }
        Provider::ChatGpt => {
            match chatgpt::prepare(protocol, payload, &published.native_model, scope) {
                Ok(prepared) => prepared,
                Err(e) => return failed(e),
            }
        }
    };
    let capture = parts
        .extensions
        .get::<crate::proxy::monitor::UpstreamRequestBodyHolder>()
        .cloned();
    if let Some(capture) = &capture {
        capture.set_value(&prepared.body);
    }
    let policy = AccountSchedulingPolicy::live(&state.token_manager, None).await;
    let targets: Vec<String> = published
        .native_models()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let call = Call {
        mode: prepared.mode,
        forwarded: &parts.headers,
        scope,
        conversation: &prepared.conversation,
    };
    let (events, account, native_model, upstream) = match open(
        provider,
        &targets,
        prepared.body,
        Duration::from_secs(state.request_timeout.clamp(1, 3600)),
        &prepared.session,
        &policy,
        capture.as_ref(),
        call,
    )
    .await
    {
        Ok(v) => v,
        Err(refusal) => return refused(refusal, prepared.mode),
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
    if let Some(turn) = upstream.get("x-codex-turn-state") {
        headers.insert("x-codex-turn-state", turn.clone());
    }
    let error_code = "native_upstream_failed";
    match (prepared.mode, protocol) {
        (WireMode::Chat | WireMode::ResponsesToChat, Some(protocol)) => {
            Relay {
                protocol,
                alias: published.public_id,
                namespaced,
                headers,
                error_code,
            }
            .run(events, streaming)
            .await
        }
        (WireMode::Unary, _) => {
            ResponsesRelay {
                alias: published.public_id,
                headers,
                error_code,
            }
            .unary(events)
            .await
        }
        _ => {
            ResponsesRelay {
                alias: published.public_id,
                headers,
                error_code,
            }
            .run(events, streaming)
            .await
        }
    }
}
/// Request features each provider's wire payload actually carries. Qoder rebuilds its
/// own request without a structured-output field, so `response_format` / Responses
/// `text.format` must be rejected up front instead of silently running unconstrained.
fn dialect(provider: Provider) -> codec::Dialect {
    match provider {
        Provider::Qoder => codec::Dialect {
            structured_output: false,
        },
        // The Responses wire carries `text.format`.
        Provider::ChatGpt => codec::Dialect::OPENAI_COMPATIBLE,
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
        // The Responses wire carries every tool_choice and parallel_tool_calls.
        Provider::ChatGpt => return Ok(()),
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
fn client_status(status: u16) -> StatusCode {
    match status {
        400 | 404 | 413 | 422 | 504 => StatusCode::from_u16(status).unwrap(),
        429 => StatusCode::TOO_MANY_REQUESTS,
        503 => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    }
}
fn failed(e: Failure) -> Response {
    error_response(
        client_status(e.status),
        "native_upstream_failed",
        &e.message,
    )
}
/// A Responses passthrough client receives the upstream's own error object (with the
/// mapped status); every other client the generic error.
fn refused(refusal: Refusal, mode: WireMode) -> Response {
    match refusal.upstream {
        Some(mut error) if mode == WireMode::Passthrough => {
            if error.get("message").is_none() {
                error["message"] = json!(refusal.failure.message);
            }
            axum::response::IntoResponse::into_response((
                client_status(refusal.failure.status),
                axum::Json(json!({ "error": error })),
            ))
        }
        _ => failed(refusal.failure),
    }
}
async fn send(
    account: &Account,
    model: &str,
    payload: Value,
    timeout: Duration,
    call: Call<'_>,
) -> Result<reqwest::Response, DispatchFailure> {
    let deadline = tokio::time::Instant::now() + timeout;
    let remaining = || deadline.saturating_duration_since(tokio::time::Instant::now());
    match account.provider {
        Provider::Qoder => {
            let client = tokio::time::timeout_at(deadline, native::http::client_for(account))
                .await
                .map_err(|_| Failure::new(504, "Qoder egress resolution timed out"))??;
            let remaining = remaining();
            if remaining.is_zero() {
                return Err(Failure::new(504, "Qoder request timed out before dispatch").into());
            }
            native::qoder::chat(&client, account, model, payload, remaining).await
        }
        Provider::ChatGpt => {
            let client = tokio::time::timeout_at(deadline, native::http::plain_client_for(account))
                .await
                .map_err(|_| Failure::new(504, "ChatGPT egress resolution timed out"))??;
            let remaining = remaining();
            if remaining.is_zero() {
                return Err(Failure::new(504, "ChatGPT request timed out before dispatch").into());
            }
            let endpoint = if call.mode == WireMode::Unary {
                native::chatgpt::client::Endpoint::Compact
            } else {
                native::chatgpt::client::Endpoint::Responses
            };
            let session = native::chatgpt::client::session_id(
                call.scope,
                &account.credentials.chatgpt_account_id,
                call.conversation,
            );
            native::chatgpt::client::send(
                &client,
                account,
                payload,
                endpoint,
                &session,
                call.forwarded,
                remaining,
            )
            .await
        }
    }
}
/// The normalizer for one exchange's wire mode.
fn events_for(provider: Provider, mode: WireMode, response: reqwest::Response) -> NativeEvents {
    match mode {
        WireMode::Chat => normalize(provider, response),
        WireMode::ResponsesToChat => chatgpt::chat_chunks(response),
        WireMode::Passthrough => chatgpt::raw_events(response),
        WireMode::Unary => chatgpt::unary(response),
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
async fn until_output(
    events: &mut NativeEvents,
    carries_output: fn(&str) -> bool,
) -> Result<Vec<String>, Failure> {
    let mut prelude = Vec::new();
    buffer_until_output(events, carries_output, &mut prelude)
        .await
        .map(|()| prelude)
}
/// `until_output` into a caller's buffer, which keeps what arrived before a failure.
async fn buffer_until_output(
    events: &mut NativeEvents,
    carries_output: fn(&str) -> bool,
    prelude: &mut Vec<String>,
) -> Result<(), Failure> {
    while let Some(event) = events.next().await {
        let chunk = event?;
        if chunk == "[DONE]" {
            break;
        }
        let output = carries_output(&chunk);
        prelude.push(chunk);
        if output {
            return Ok(());
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
/// Extra accounts tried after connection failures. An unclassified upstream answer,
/// including 401, 403, 429 and a stream error, stays on the account that received it:
/// replaying the same request elsewhere spreads one rejection across the pool. Only a
/// provider classification (`Failure::failover`, ChatGPT) moves an answer elsewhere.
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
/// Charges a failed attempt: ChatGPT by its classification, Qoder by status as before.
fn charge(lease: &mut accounts::Lease, error: &Failure) {
    match lease.account.provider {
        Provider::Qoder => lease.failure(error.status),
        Provider::ChatGpt => lease.settle(error),
    }
}
#[allow(clippy::too_many_arguments)]
async fn open(
    provider: Provider,
    targets: &[String],
    payload: Value,
    timeout: Duration,
    session: &str,
    policy: &AccountSchedulingPolicy,
    capture: Option<&crate::proxy::monitor::UpstreamRequestBodyHolder>,
    call: Call<'_>,
) -> Result<(Events, Account, String, HeaderMap), Refusal> {
    // `dispatch` captured the body stamped with the primary ID.
    let mut captured = targets.first().cloned().unwrap_or_default();
    let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut excluded = HashSet::new();
    let mut last = None;
    let mut failover_policy = policy.clone();
    failover_policy.sticky.max_wait_seconds = 0;
    let mut connection_failovers = 0;
    let attempts = match provider {
        Provider::Qoder => MAX_QODER_ATTEMPTS,
        Provider::ChatGpt => MAX_CHATGPT_ACCOUNTS,
    };
    for _ in 0..attempts {
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
            Err(e) => return Err(last.unwrap_or_else(|| e.into())),
        };
        let id = lease.account.id.clone();
        excluded.insert(id.clone());
        let refresh_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
        lease.account =
            match tokio::time::timeout(refresh_budget, accounts::fresh(provider, &id, false)).await
            {
                Ok(Ok(a)) => a,
                Ok(Err(e)) => {
                    charge(&mut lease, &e);
                    last = Some(e.into());
                    continue;
                }
                Err(_) => {
                    return Err(
                        Failure::new(504, "Account refresh exceeded the request timeout").into(),
                    )
                }
            };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Failure::new(504, "Native upstream request timed out").into());
        }
        let model = lease.model.clone();
        let attempt = attempt_payload(&payload, &model);
        super::recapture(capture, &mut captured, &model, &attempt);
        let mut response = send(&lease.account, &model, attempt.clone(), remaining, call).await;
        // ChatGPT reads a 403 before refreshing: a Cloudflare/HTML challenge is a route
        // problem, so the account is neither refreshed nor charged and another one serves.
        if provider == Provider::ChatGpt {
            response = match response {
                Ok(r) => match chatgpt::screen(r).await {
                    Ok(r) => Ok(r),
                    Err(route) => {
                        lease.settle(&route);
                        if !route.failover {
                            return Err(route.into());
                        }
                        tracing::warn!(
                            "ChatGPT route answered HTTP 403 before output; trying another account"
                        );
                        last = Some(route.into());
                        continue;
                    }
                },
                failed => failed,
            };
        }
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
                        return Err(Failure::new(504, "Native upstream request timed out").into());
                    }
                    response = send(&lease.account, &model, attempt.clone(), remaining, call).await;
                }
                Ok(Err(error)) => {
                    // The upstream already answered 401/403 for this account.
                    charge(&mut lease, &error);
                    // ChatGPT classifies every 401/403 as another account's request.
                    if provider == Provider::ChatGpt {
                        last = Some(error.into());
                        continue;
                    }
                    return Err(error.into());
                }
                Err(_) => {
                    return Err(
                        Failure::new(504, "Account refresh exceeded the request timeout").into(),
                    )
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
                let (error, reached, upstream) = match result {
                    Ok(r) => {
                        let (error, upstream) = match provider {
                            Provider::Qoder => (
                                Failure::new(
                                    r.status().as_u16(),
                                    format!(
                                        "{} returned HTTP {}",
                                        provider.name(),
                                        r.status().as_u16()
                                    ),
                                ),
                                None,
                            ),
                            Provider::ChatGpt => chatgpt::rejection(&lease.account, r).await,
                        };
                        (error, Reached::Upstream, upstream)
                    }
                    Err(e) => {
                        let (error, reached) = dispatch_failure(e, upstream_answered);
                        (error, reached, None)
                    }
                };
                let refusal = |failure| Refusal { failure, upstream };
                if provider == Provider::ChatGpt {
                    lease.settle(&error);
                }
                if error.failover {
                    tracing::warn!(
                        "{} account answered HTTP {} before output; trying another account",
                        provider.name(),
                        error.status
                    );
                    last = Some(refusal(error));
                    continue;
                }
                if matches!(error.status, 400 | 413 | 422) {
                    return Err(refusal(error));
                }
                if provider == Provider::Qoder {
                    lease.failure_after(error.status, retry_after);
                }
                if !allow_failover(reached, &mut connection_failovers) {
                    return Err(refusal(error));
                }
                tracing::warn!(
                    "{} account attempt failed at {:?}; trying another account",
                    provider.name(),
                    reached
                );
                last = Some(refusal(error));
                continue;
            }
        };
        let upstream = response.headers().clone();
        let mut events = events_for(provider, call.mode, response);
        // Nothing reaches the client until generation starts, so a failure before then
        // is still returned as a typed error instead of a broken stream.
        let mut prelude = Vec::new();
        if let Err(e) =
            buffer_until_output(&mut events, call.mode.carries_output(), &mut prelude).await
        {
            charge(&mut lease, &e);
            // A ChatGPT failure event names the upstream's own error.
            let refusal = Refusal {
                upstream: match provider {
                    Provider::Qoder => None,
                    Provider::ChatGpt => prelude.last().and_then(|e| chatgpt::event_error(e)),
                },
                failure: e,
            };
            // Unclassified: another account would receive the same request.
            if !refusal.failure.failover {
                return Err(refusal);
            }
            tracing::warn!(
                "{} stream failed before output; trying another account",
                provider.name()
            );
            last = Some(refusal);
            continue;
        }
        let account = lease.account.clone();
        let guarded = async_stream::stream! {
            for chunk in prelude { yield Ok(chunk); }
            while let Some(event) = events.next().await {
                match event {
                    Ok(data) => { if data == "[DONE]" { lease.success(); } yield Ok(data); },
                    Err(e) => { charge(&mut lease, &e); yield Err(e.to_string()); return; },
                }
            }
        };
        return Ok((guarded.boxed(), account, model, upstream));
    }
    Err(last.unwrap_or_else(|| {
        Failure::new(429, "All eligible accounts are temporarily unavailable").into()
    }))
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
    let (payload, mode) = match provider {
        Provider::Qoder => (
            json!({"model":model,"messages":[{"role":"system","content":system},{"role":"user","content":user}],"stream":true,"max_tokens":64}),
            WireMode::Chat,
        ),
        Provider::ChatGpt => (
            chatgpt::probe_body(model, system, user).map_err(|e| e.to_string())?,
            WireMode::Passthrough,
        ),
    };
    let conversation = uuid::Uuid::new_v4().to_string();
    let forwarded = HeaderMap::new();
    let call = Call {
        mode,
        forwarded: &forwarded,
        scope: "probe",
        conversation: &conversation,
    };
    let response = match send(&a, model, payload, Duration::from_secs(25), call).await {
        Ok(response) => response,
        Err(error) => {
            let error = Failure::from(error);
            charge(&mut lease, &error);
            return Err(error.to_string());
        }
    };
    if !response.status().is_success() {
        match provider {
            Provider::Qoder => lease.failure(response.status().as_u16()),
            Provider::ChatGpt => lease.settle(&chatgpt::rejection(&a, response).await.0),
        }
        return Ok((
            "failed".into(),
            "上游调用失败；已有验证证据保留。".into(),
            Some(a.id),
        ));
    }
    let mut events = events_for(provider, mode, response);
    let mut text = false;
    let mut complete = false;
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                charge(&mut lease, &error);
                return Err(error.to_string());
            }
        };
        if event == "[DONE]" {
            complete |= mode == WireMode::Chat;
            break;
        }
        match mode {
            WireMode::Passthrough => chatgpt::probe_evidence(&event, &mut text, &mut complete),
            _ => {
                if let Ok(v) = serde_json::from_str::<Value>(&event) {
                    text |= v
                        .pointer("/choices/0/delta/content")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.trim().is_empty());
                }
            }
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
    fn monitor_capture_follows_the_native_id_actually_sent() {
        let capture = crate::proxy::monitor::UpstreamRequestBodyHolder::new();
        let mut captured = "shared-model".to_owned();
        let original = json!({"model":"shared-model","messages":[]});
        crate::proxy::upstream::recapture(Some(&capture), &mut captured, "shared-model", &original);
        assert_eq!(
            capture.take(),
            None,
            "same ID keeps the dispatch-time capture"
        );
        let attempt = attempt_payload(&original, "shared-model-sg");
        crate::proxy::upstream::recapture(
            Some(&capture),
            &mut captured,
            "shared-model-sg",
            &attempt,
        );
        let body: Value = serde_json::from_str(&capture.take().unwrap()).unwrap();
        assert_eq!(body["model"], "shared-model-sg");
        assert_eq!(captured, "shared-model-sg");
        crate::proxy::upstream::recapture(None, &mut captured, "shared-model", &original);
        assert_eq!(captured, "shared-model");
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
            let forwarded = HeaderMap::new();
            let call = Call {
                mode: WireMode::Chat,
                forwarded: &forwarded,
                scope: "test",
                conversation: "test",
            };
            let error = send(&a, "model", json!({}), Duration::from_secs(1), call)
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
        assert_eq!(
            until_output(&mut events, carries_output)
                .await
                .unwrap_err()
                .status,
            503
        );
        // Leading role-only and usage chunks are kept, in order, ahead of the first output.
        let mut events = normalize(
            Provider::Qoder,
            response("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"),
        );
        let prelude = until_output(&mut events, carries_output).await.unwrap();
        assert_eq!(prelude.len(), 3);
        assert!(prelude[2].contains("\"hi\""));
        assert_eq!(events.next().await.unwrap().unwrap(), "[DONE]");
        // A stream that finishes without any output is an empty completion.
        let mut events = normalize(
            Provider::Qoder,
            response("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"),
        );
        assert_eq!(
            until_output(&mut events, carries_output)
                .await
                .unwrap_err()
                .status,
            502
        );
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
