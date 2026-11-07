//! ChatGPT (Codex backend) side of native dispatch: request bodies, upstream answer
//! classification and the Responses-event normalizers `open()` selects by wire mode.
use super::{NativeEvents, Prepared, WireMode};
use crate::modules::native_providers::{
    chatgpt::{client, errors, payload},
    http::{Failure, Penalty},
    now, store, Account, Provider,
};
use crate::proxy::{
    session_manager::SessionManager,
    upstream::{chat_codec as codec, openai_compat, responses_relay, responses_wire},
};
use eventsource_stream::Eventsource;
use futures::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};

/// Error bodies are read only this far for classification.
const MAX_ERROR_BODY: usize = 1024 * 1024;
const MAX_EVENT: usize = 8 * 1024 * 1024;

/// Scheduler key of a Responses body: the shared canonical key when the body converts to
/// Chat, else the first user item (Codex items the Chat codec does not model).
fn responses_session(scope: &str, body: &Value) -> String {
    if let Ok(chat) = codec::request_for(
        codec::Protocol::Responses,
        body.clone(),
        super::dialect(Provider::ChatGpt),
    ) {
        return SessionManager::account_session_key(scope, &chat);
    }
    let first = match body.get("input") {
        Some(Value::String(text)) => json!({"role":"user","content":text}),
        Some(Value::Array(items)) => items
            .iter()
            .find(|i| i["role"] == "user")
            .cloned()
            .unwrap_or(Value::Null),
        _ => Value::Null,
    };
    SessionManager::account_session_key(scope, &json!({"messages":[first]}))
}

/// `protocol` is `None` for `/responses/compact`.
pub(super) fn prepare(
    protocol: Option<codec::Protocol>,
    request: Value,
    model: &str,
    scope: &str,
) -> Result<Prepared, Failure> {
    let cache_key = request
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|k| !k.trim().is_empty())
        .map(str::to_owned);
    let (body, mode, session) = match protocol {
        None => {
            let session = responses_session(scope, &request);
            (
                payload::prepare(request, model, payload::Mode::Compact)?,
                WireMode::Unary,
                session,
            )
        }
        Some(codec::Protocol::Responses) => {
            let session = responses_session(scope, &request);
            (
                payload::prepare(request, model, payload::Mode::Http)?,
                WireMode::Passthrough,
                session,
            )
        }
        Some(protocol) => {
            let chat = codec::request_for(protocol, request, super::dialect(Provider::ChatGpt))
                .map_err(|e| Failure::new(400, e))?;
            let chat = openai_compat::prepare(chat, model);
            super::representable(Provider::ChatGpt, &chat)?;
            let session = SessionManager::account_session_key(scope, &chat);
            let responses = responses_wire::from_chat(chat).map_err(|e| Failure::new(400, e))?;
            (
                payload::prepare(responses, model, payload::Mode::Http)?,
                WireMode::ResponsesToChat,
                session,
            )
        }
    };
    Ok(Prepared {
        body,
        mode,
        conversation: cache_key.unwrap_or_else(|| session.clone()),
        session,
    })
}

/// The minimal text request an explicit model probe sends.
pub(super) fn probe_body(model: &str, system: &str, user: &str) -> Result<Value, Failure> {
    payload::prepare(
        json!({"model":model,"instructions":system,
            "input":[{"type":"message","role":"user","content":user}]}),
        model,
        payload::Mode::Http,
    )
}

async fn read_capped(mut response: reqwest::Response) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_ERROR_BODY - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if bytes.len() >= MAX_ERROR_BODY {
            break;
        }
    }
    bytes
}

/// Stores the plan a usage-limit answer reports. Best effort: a newer authorization wins.
fn remember_plan(account: &Account, body: &str) {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return;
    };
    let plan = [&json["error"]["plan_type"], &json["plan_type"]]
        .into_iter()
        .find_map(|v| v.as_str().map(str::trim).filter(|p| !p.is_empty()));
    if let Some(plan) = plan.filter(|p| *p != account.credentials.user_type) {
        let _ = store::update(
            account.provider,
            &account.id,
            Some(&account.revision),
            |a| {
                a.credentials.user_type = plan.to_owned();
            },
        );
    }
}

/// Classifies a non-2xx answer from at most 1 MiB of its body, returned with the
/// upstream's own error object when the body carries one.
pub(in crate::proxy::upstream) async fn rejection(
    account: &Account,
    response: reqwest::Response,
) -> (Failure, Option<Value>) {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = String::from_utf8_lossy(&read_capped(response).await).into_owned();
    if status == 429 {
        remember_plan(account, &body);
    }
    let upstream = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|json| json.get("error").filter(|e| e.is_object()).cloned());
    (errors::classify(status, &headers, &body, now()), upstream)
}

/// The upstream's own error object carried by a raw failure event.
pub(super) fn event_error(data: &str) -> Option<Value> {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|event| responses_relay::failure_error(&event))
}

/// Parsed upstream SSE data frames. Interruption, oversize and invalid JSON end the stream
/// with a failure; a `[DONE]` sentinel ends it silently.
fn upstream_events(response: reqwest::Response) -> BoxStream<'static, Result<Value, Failure>> {
    async_stream::stream! {
        let mut events = response.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let Ok(event) = event else {
                yield Err(Failure::new(502, "ChatGPT stream connection interrupted"));
                return;
            };
            if event.data.len() > MAX_EVENT {
                yield Err(Failure::new(502, "ChatGPT stream event exceeds size limit"));
                return;
            }
            let data = event.data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                return;
            }
            match serde_json::from_str::<Value>(data) {
                Ok(value) => yield Ok(value),
                Err(_) => {
                    yield Err(Failure::new(502, "ChatGPT returned an invalid stream event"));
                    return;
                }
            }
        }
    }
    .boxed()
}

/// A stream event that ends the attempt as a classified failure.
fn failure_of(event: &Value) -> Option<Failure> {
    match responses_wire::event_type(event) {
        Some("error" | "response.failed") => Some(errors::classify_event(event, now())),
        // A cancellation says nothing about the account.
        Some("response.cancelled") => Some(Failure::classified(
            502,
            "ChatGPT response was cancelled",
            Penalty::Neutral,
            false,
        )),
        _ => None,
    }
}

fn ended() -> Failure {
    Failure::new(502, "ChatGPT stream ended before completion")
}

/// Responses events as Chat chunks, ending with `[DONE]` after the terminal event.
pub(super) fn chat_chunks(response: reqwest::Response) -> NativeEvents {
    let mut upstream = upstream_events(response);
    async_stream::stream! {
        let mut chunks = responses_wire::ChatChunks::new();
        while let Some(event) = upstream.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => { yield Err(error); return; }
            };
            if let Some(error) = failure_of(&event) {
                yield Err(error);
                return;
            }
            match chunks.push(&event) {
                Ok(out) => {
                    for chunk in out {
                        let done = chunk == "[DONE]";
                        yield Ok(chunk);
                        if done { return; }
                    }
                }
                Err(message) => { yield Err(Failure::new(502, message)); return; }
            }
        }
        yield Err(ended());
    }
    .boxed()
}

/// Raw Responses events (JSON text), ending with `[DONE]` after the terminal event. An
/// upstream `error`/`response.failed` event is yielded as it is, then as its failure, so a
/// Responses client receives the upstream's own failure.
pub(super) fn raw_events(response: reqwest::Response) -> NativeEvents {
    let mut upstream = upstream_events(response);
    async_stream::stream! {
        while let Some(event) = upstream.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => { yield Err(error); return; }
            };
            if let Some(error) = failure_of(&event) {
                if responses_relay::failure_error(&event).is_some() {
                    yield Ok(event.to_string());
                }
                yield Err(error);
                return;
            }
            let terminal = matches!(
                responses_wire::event_type(&event),
                Some("response.completed" | "response.done" | "response.incomplete")
            );
            yield Ok(event.to_string());
            if terminal {
                yield Ok("[DONE]".into());
                return;
            }
        }
        yield Err(ended());
    }
    .boxed()
}

/// A unary JSON answer (at most 32 MiB) as one event followed by `[DONE]`.
pub(super) fn unary(response: reqwest::Response) -> NativeEvents {
    async_stream::stream! {
        let bytes = match client::read_bytes(response).await {
            Ok(bytes) => bytes,
            Err(error) => { yield Err(error); return; }
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => {
                yield Ok(value.to_string());
                yield Ok("[DONE]".into());
            }
            _ => yield Err(Failure::new(502, "ChatGPT returned an invalid JSON response")),
        }
    }
    .boxed()
}

/// Whether a raw Responses event is client-visible output.
pub(super) fn carries_event(data: &str) -> bool {
    serde_json::from_str::<Value>(data).map_or(true, |v| responses_wire::carries_output(&v))
}

/// Probe evidence from raw events: non-empty text and a completed response.
pub(super) fn probe_evidence(data: &str, text: &mut bool, complete: &mut bool) {
    let Ok(event) = serde_json::from_str::<Value>(data) else {
        return;
    };
    match responses_wire::event_type(&event) {
        Some("response.output_text.delta") => {
            *text |= event["delta"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty());
        }
        Some("response.completed" | "response.done") => *complete = true,
        _ => {}
    }
}
