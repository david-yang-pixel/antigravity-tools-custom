//! ChatGPT (Codex backend) WebSocket relay. A Codex `/v1/responses` WebSocket whose first
//! generating turn names a ChatGPT publication is relayed to the upstream Codex WebSocket
//! for the rest of its lifetime. Each turn leases an account (sticky for the connection,
//! released between turns); the upstream socket stays bound to its account between turns.
//! A turn that fails before output with a failover-eligible error, or whose account is
//! unavailable, moves to another account (at most 3 per turn); a turn continuing a response
//! held by another account is rebuilt from the per-connection transcript.
pub mod transcript;
mod upstream;

use crate::modules::model_registry::PublishedModel;
use crate::modules::native_providers::{
    accounts::{self, Lease},
    chatgpt::{client, errors, payload},
    http::{Failure, Penalty},
    now, Provider,
};
use crate::proxy::{
    middleware::model_registry::resolve_public_model, server::AppState,
    sticky_config::AccountSchedulingPolicy,
};
use axum::{
    extract::ws::{CloseFrame, Message, WebSocket},
    http::HeaderMap,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};
use tokio::time::Instant;
use transcript::{Rebuild, Transcript, TRANSCRIPT_LIMIT};
use upstream::{Incoming, Refused, Upstream};

/// Accounts one turn may try.
const MAX_ACCOUNTS: usize = 3;
/// Reconnects of one account's socket after transport failures while no turn is in flight.
const MAX_RECONNECTS: usize = 3;
const BACKOFF: Duration = Duration::from_millis(200);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
/// WebSocket sessions are master-key only.
const SCOPE: &str = "administrator";
/// Response ids remembered with the account holding their upstream state.
const MAX_OWNERS: usize = 4096;
/// Client frames held while a turn is in flight; later ones are refused.
const MAX_QUEUED: usize = 16;

/// Liveness and idle bounds of a relayed session.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// An upstream socket idle this long is pinged before the next turn.
    pub ping_after: Duration,
    /// How long a ping waits for its pong.
    pub pong_timeout: Duration,
    /// A client idle this long between turns is closed (code 1000); an upstream silent this
    /// long mid-turn is abandoned.
    pub idle: Duration,
}
impl Timing {
    pub const DEFAULT: Self = Self {
        ping_after: Duration::from_secs(20),
        pong_timeout: Duration::from_secs(5),
        idle: Duration::from_secs(300),
    };
    fn current() -> Self {
        #[cfg(test)]
        if let Some(timing) = *TEST_TIMING.lock().unwrap_or_else(|e| e.into_inner()) {
            return timing;
        }
        Self::DEFAULT
    }
}

/// Replaced only by tests, which hold the shared env lock and restore `None` afterwards.
#[cfg(test)]
static TEST_TIMING: std::sync::Mutex<Option<Timing>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(crate) fn set_test_timing(timing: Option<Timing>) {
    *TEST_TIMING.lock().unwrap_or_else(|e| e.into_inner()) = timing;
}

/// A turn's `input` as items (a string is one user message).
fn input_items(frame: &Value) -> Vec<Value> {
    match frame.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({"type":"message","role":"user","content":text})]
        }
        _ => Vec::new(),
    }
}

/// The first generating frame of a session whose `generate:false` prewarm was answered
/// locally (`prewarm` is that frame and the local response id). A frame continuing the
/// local response gets the prewarm input prepended and the local id removed, so the
/// upstream receives the whole conversation; any other frame is returned unchanged.
pub fn after_prewarm(prewarm: Option<(&Value, &str)>, mut frame: Value) -> Value {
    let Some((prewarm, id)) = prewarm else {
        return frame;
    };
    if frame.get("previous_response_id").and_then(Value::as_str) != Some(id) {
        return frame;
    }
    let mut input = input_items(prewarm);
    input.extend(input_items(&frame));
    if let Some(object) = frame.as_object_mut() {
        object.remove("previous_response_id");
        object.insert("input".into(), Value::Array(input));
        for key in ["model", "instructions", "tools", "tool_choice"] {
            if let Some(value) = prewarm.get(key).filter(|_| !object.contains_key(key)) {
                object.insert(key.into(), value.clone());
            }
        }
    }
    frame
}

fn error_event(status: u16, code: &str, message: &str) -> Value {
    let kind = if (400..500).contains(&status) && status != 429 {
        "invalid_request_error"
    } else {
        "server_error"
    };
    json!({"type":"error","status":status,"error":{"type":kind,"code":code,"message":message}})
}

/// The upstream's own failure event when there is one, else a relay error event.
fn failure_event(failure: &Failure, upstream: Option<Value>) -> Value {
    upstream
        .unwrap_or_else(|| error_event(failure.status, "native_upstream_failed", &failure.message))
}

/// A turn lost to the connection (drop or stall) after it was sent: not the account's fault,
/// so the account is not cooled down and the client's retry can use it again.
fn transport_fault(status: u16, message: &str) -> Failure {
    Failure::classified(status, message, Penalty::Neutral, false)
}

fn upstream_error(event: &Value) -> &Value {
    event
        .get("error")
        .filter(|e| e.is_object())
        .or_else(|| event.pointer("/response/error"))
        .unwrap_or(&Value::Null)
}

fn previous_not_found(event: &Value) -> bool {
    upstream_error(event)["code"] == "previous_response_not_found"
}

fn invalid_encrypted_content(event: &Value) -> bool {
    let error = upstream_error(event);
    let message = error["message"].as_str().unwrap_or("").to_lowercase();
    error["code"] == "invalid_encrypted_content"
        || (message.contains("encrypted content") && message.contains("could not be verified"))
}

enum FromClient {
    Frame(Value),
    Invalid(String),
    Skip,
    Gone,
}
fn from_client(message: Option<Result<Message, axum::Error>>) -> FromClient {
    let text = match message {
        Some(Ok(Message::Text(text))) => text,
        Some(Ok(Message::Binary(bytes))) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return FromClient::Skip,
        },
        Some(Ok(Message::Ping(_) | Message::Pong(_))) => return FromClient::Skip,
        _ => return FromClient::Gone,
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(frame) => FromClient::Frame(frame),
        Err(e) => FromClient::Invalid(format!("Invalid JSON: {e}")),
    }
}

/// How one exchange on the bound socket ended.
enum Outcome {
    /// The turn ended (completed, or failed after output) and the client was told.
    Done,
    ClientGone,
    /// The socket ended before answering anything: it was stale.
    Stale,
    /// The upstream refused the turn before any output.
    Refused {
        failure: Failure,
        prelude: Vec<Value>,
        event: Value,
    },
}

struct Relay {
    client: WebSocket,
    state: AppState,
    timing: Timing,
    /// Public id that frames without a model inherit.
    public: String,
    /// Scheduler affinity key: `ws:<connection uuid>`.
    session: String,
    /// Key of the upstream `session_id`: the client's `prompt_cache_key`, else `session`.
    conversation: String,
    upstream: Option<Upstream>,
    /// Response id → account holding its upstream state.
    owners: HashMap<String, String>,
    transcript: Transcript,
    /// Frames the client sent while a turn was in flight.
    queued: VecDeque<Value>,
    /// The client's handshake headers.
    forwarded: HeaderMap,
    /// The bound socket refused a turn with a bare `error` event, which the upstream often
    /// follows with a `response.failed` for the same request.
    trailing_failure: bool,
}

/// Relays a Codex WebSocket whose first generating frame resolved to `published` (a
/// ChatGPT publication) until either side closes.
/// `headers` are the client's handshake headers; only the Codex allow-list reaches the
/// upstream.
pub async fn run(
    socket: WebSocket,
    headers: HeaderMap,
    first: Value,
    published: PublishedModel,
    state: AppState,
) {
    let id = uuid::Uuid::new_v4();
    let session = format!("ws:{id}");
    let conversation = first
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|k| !k.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| session.clone());
    tracing::info!("Codex responses websocket: relaying to ChatGPT");
    let mut relay = Relay {
        client: socket,
        state,
        timing: Timing::current(),
        public: published.public_id,
        session,
        conversation,
        upstream: None,
        owners: HashMap::new(),
        transcript: Transcript::new(TRANSCRIPT_LIMIT),
        queued: VecDeque::new(),
        forwarded: headers,
        trailing_failure: false,
    };
    let mut next = Some(first);
    loop {
        let frame = match next.take().or_else(|| relay.queued.pop_front()) {
            Some(frame) => frame,
            None => match relay.idle().await {
                Some(frame) => frame,
                None => break,
            },
        };
        if !relay.turn(frame).await {
            break;
        }
    }
    if let Some(upstream) = relay.upstream.take() {
        upstream.close().await;
    }
    tracing::info!("Codex responses websocket: ChatGPT relay closed");
}

/// The bound socket's next message; pending while none is bound.
async fn upstream_message(upstream: &mut Option<Upstream>) -> Incoming {
    match upstream.as_mut() {
        Some(upstream) => upstream.next().await,
        None => std::future::pending().await,
    }
}

impl Relay {
    async fn reply(&mut self, event: Value) -> bool {
        self.client
            .send(Message::Text(event.to_string()))
            .await
            .is_ok()
    }

    /// Waits for the client's next frame between turns, watching the upstream socket.
    async fn idle(&mut self) -> Option<Value> {
        enum Step {
            Client(FromClient),
            Upstream(Incoming),
            Idle,
        }
        let deadline = Instant::now() + self.timing.idle;
        loop {
            let step = tokio::select! {
                message = self.client.recv() => Step::Client(from_client(message)),
                incoming = upstream_message(&mut self.upstream) => Step::Upstream(incoming),
                () = tokio::time::sleep_until(deadline) => Step::Idle,
            };
            match step {
                Step::Client(FromClient::Frame(frame)) => return Some(frame),
                Step::Client(FromClient::Invalid(message)) => {
                    if !self.reply(error_event(400, "invalid_json", &message)).await {
                        return None;
                    }
                }
                Step::Client(FromClient::Skip) => {}
                Step::Client(FromClient::Gone) => return None,
                // Reconnected on the next turn.
                Step::Upstream(Incoming::Closed) => self.upstream = None,
                // Leftovers of a finished request.
                Step::Upstream(Incoming::Event(_)) => self.trailing_failure = false,
                Step::Idle => {
                    let _ = self
                        .client
                        .send(Message::Close(Some(CloseFrame {
                            code: 1000,
                            reason: "idle timeout".into(),
                        })))
                        .await;
                    return None;
                }
            }
        }
    }

    /// One client turn. Returns false once the client is gone.
    async fn turn(&mut self, frame: Value) -> bool {
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "response.create" {
            let message = format!("unsupported websocket request type: {kind}");
            return self
                .reply(error_event(400, "unsupported_request_type", &message))
                .await;
        }
        let model = frame
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .unwrap_or(&self.public)
            .to_owned();
        // Only the publication's ChatGPT targets are served on this connection.
        let published = match resolve_public_model(&model)
            .map(|p| p.for_source(Provider::ChatGpt.source()))
        {
            Ok(Some(p)) => p,
            Ok(None) => {
                return self
                    .reply(error_event(
                        400,
                        "model_source_mismatch",
                        "This WebSocket session serves ChatGPT models; open a new WebSocket connection for another source",
                    ))
                    .await
            }
            Err((status, message)) => {
                return self
                    .reply(error_event(status.as_u16(), "model_not_published", &message))
                    .await
            }
        };
        self.public = published.public_id.clone();
        let previous = frame
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let mut input = input_items(&frame);
        self.transcript.strip_rejected(&mut input);
        self.transcript.begin(previous.as_deref());
        self.transcript.record_request(&input);
        let mut frame = frame;
        frame["input"] = Value::Array(input.clone());
        self.dispatch(frame, previous, input, published).await
    }

    async fn dispatch(
        &mut self,
        frame: Value,
        previous: Option<String>,
        input: Vec<Value>,
        published: PublishedModel,
    ) -> bool {
        let alias = published.public_id.clone();
        let targets = published.native_models();
        let policy = AccountSchedulingPolicy::live(&self.state.token_manager, None).await;
        let mut failover_policy = policy.clone();
        failover_policy.sticky.max_wait_seconds = 0;
        let deadline =
            Instant::now() + Duration::from_secs(self.state.request_timeout.clamp(1, 3600));
        let mut excluded = HashSet::new();
        let mut last: Option<(Failure, Option<Value>)> = None;
        // Replay from the transcript even on the account holding the reference.
        let mut full = false;
        let (mut replayed, mut stripped) = (false, false);
        'accounts: for _ in 0..MAX_ACCOUNTS {
            let admission = if excluded.is_empty() {
                &policy
            } else {
                &failover_policy
            };
            let mut lease = match accounts::scheduling::acquire(
                Provider::ChatGpt,
                &targets,
                &self.session,
                admission,
                &excluded,
                deadline,
            )
            .await
            {
                Ok(lease) => lease,
                Err(e) => {
                    let (failure, event) = last.unwrap_or((e, None));
                    return self.reply(failure_event(&failure, event)).await;
                }
            };
            let id = lease.account.id.clone();
            excluded.insert(id.clone());
            lease.account = match tokio::time::timeout_at(
                deadline,
                accounts::fresh(Provider::ChatGpt, &id, false),
            )
            .await
            {
                Ok(Ok(account)) => account,
                Ok(Err(e)) => {
                    lease.settle(&e);
                    last = Some((e, None));
                    continue;
                }
                Err(_) => {
                    let failure = Failure::new(504, "Account refresh exceeded the request timeout");
                    return self.reply(failure_event(&failure, None)).await;
                }
            };
            let mut stale = false;
            loop {
                let moved = previous
                    .as_ref()
                    .and_then(|p| self.owners.get(p))
                    .is_some_and(|owner| *owner != id);
                let mut body = frame.clone();
                if let Some(reference) = previous.as_deref().filter(|_| moved || full) {
                    match self.transcript.rebuild(reference, &input) {
                        Rebuild::Full(items) => {
                            body["input"] = Value::Array(items);
                            if let Some(object) = body.as_object_mut() {
                                object.remove("previous_response_id");
                            }
                        }
                        // Never seen here: forwarded unchanged.
                        Rebuild::Unknown => {}
                        Rebuild::Orphaned => {
                            return self
                                .reply(error_event(
                                    400,
                                    "orphaned_tool_output",
                                    "This turn cannot be replayed on another ChatGPT account: a tool output has no matching call in the connection history",
                                ))
                                .await
                        }
                        Rebuild::Dropped => {
                            return self
                                .reply(error_event(
                                    400,
                                    "history_unavailable",
                                    "This turn cannot be replayed on another ChatGPT account: the connection history exceeded its memory bound",
                                ))
                                .await
                        }
                    }
                }
                if stripped {
                    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
                        items.retain(|item| !transcript::encrypted_state(item));
                    }
                }
                let mut body = match payload::prepare(body, &lease.model, payload::Mode::WebSocket)
                {
                    Ok(body) => body,
                    Err(e) => return self.reply(failure_event(&e, None)).await,
                };
                body["type"] = json!("response.create");
                if let Err((failure, event)) = self.ensure(&mut lease, deadline).await {
                    lease.settle(&failure);
                    if failure.failover {
                        tracing::warn!(
                            "ChatGPT WebSocket handshake failed (HTTP {}); trying another account",
                            failure.status
                        );
                        last = Some((failure, event));
                        continue 'accounts;
                    }
                    return self.reply(failure_event(&failure, event)).await;
                }
                match self
                    .exchange(&mut lease, body.to_string(), &alias, deadline)
                    .await
                {
                    Outcome::Done => return true,
                    Outcome::ClientGone => return false,
                    Outcome::Stale if !stale => stale = true,
                    Outcome::Stale => {
                        let failure = Failure {
                            failover: true,
                            ..Failure::new(502, "ChatGPT WebSocket closed before answering")
                        };
                        lease.settle(&failure);
                        last = Some((failure, None));
                        continue 'accounts;
                    }
                    Outcome::Refused {
                        mut failure,
                        prelude,
                        event,
                    } => {
                        let replayable = previous.as_deref().is_some_and(|reference| {
                            self.transcript.rebuild(reference, &[]) != Rebuild::Unknown
                        });
                        if previous_not_found(&event) && replayable && !replayed {
                            tracing::info!("ChatGPT upstream lost the previous response; replaying the turn from the transcript");
                            (replayed, full) = (true, true);
                            continue;
                        }
                        if previous_not_found(&event) {
                            // Another account cannot hold a reference this one lost.
                            failure.failover = false;
                            failure.penalty = Some(Penalty::Neutral);
                        }
                        if invalid_encrypted_content(&event) && !stripped {
                            tracing::info!("ChatGPT rejected encrypted reasoning; retrying the turn without it");
                            if let Some(items) = body["input"].as_array() {
                                self.transcript.reject(items);
                            }
                            (stripped, full) = (true, true);
                            continue;
                        }
                        lease.settle(&failure);
                        if failure.failover {
                            tracing::warn!(
                                "ChatGPT WebSocket turn failed before output (HTTP {}); trying another account",
                                failure.status
                            );
                            last = Some((failure, Some(event)));
                            continue 'accounts;
                        }
                        for event in prelude {
                            if !self.reply(event).await {
                                return false;
                            }
                        }
                        return self.reply(event).await;
                    }
                }
            }
        }
        let (failure, event) = last.unwrap_or_else(|| {
            (
                Failure::new(429, "All eligible accounts are temporarily unavailable"),
                None,
            )
        });
        self.reply(failure_event(&failure, event)).await
    }

    /// Binds a live socket for the lease's account: a socket of another account is closed,
    /// an idle one is pinged first, and a missing or failed one is (re)opened. A 401/403
    /// answer gets one forced refresh and handshake retry.
    async fn ensure(
        &mut self,
        lease: &mut Lease,
        deadline: Instant,
    ) -> Result<(), (Failure, Option<Value>)> {
        if self
            .upstream
            .as_ref()
            .is_some_and(|u| u.account != lease.account.id)
        {
            if let Some(upstream) = self.upstream.take() {
                upstream.close().await;
            }
        }
        if let Some(upstream) = self.upstream.as_mut() {
            if upstream.idle_for() >= self.timing.ping_after
                && !upstream.ping(self.timing.pong_timeout).await
            {
                tracing::info!("ChatGPT WebSocket missed its pong; reconnecting");
                self.upstream = None;
            }
        }
        if self.upstream.is_some() {
            return Ok(());
        }
        let session = client::session_id(
            SCOPE,
            &lease.account.credentials.chatgpt_account_id,
            &self.conversation,
        );
        let (mut refreshed, mut reconnects, mut backoff) = (false, 0, BACKOFF);
        loop {
            match upstream::connect(&lease.account, &session, &self.forwarded, deadline).await {
                Ok(upstream) => {
                    self.upstream = Some(upstream);
                    self.trailing_failure = false;
                    return Ok(());
                }
                Err(Refused::Transport(failure)) => {
                    if reconnects >= MAX_RECONNECTS || Instant::now() + backoff >= deadline {
                        return Err((failure, None));
                    }
                    reconnects += 1;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
                // Only an unauthorized credential is worth a rotation: a 403 is classified
                // and follows the failover policy, like the HTTP path.
                Err(Refused::Answered(failure, _)) if !refreshed && failure.status == 401 => {
                    refreshed = true;
                    match tokio::time::timeout_at(
                        deadline,
                        accounts::fresh(Provider::ChatGpt, &lease.account.id, true),
                    )
                    .await
                    {
                        Ok(Ok(account)) => lease.account = account,
                        // The upstream already rejected this account's credential.
                        Ok(Err(e)) => {
                            return Err((
                                Failure {
                                    failover: true,
                                    ..e
                                },
                                None,
                            ))
                        }
                        Err(_) => {
                            return Err((
                                Failure::new(504, "Account refresh exceeded the request timeout"),
                                None,
                            ))
                        }
                    }
                }
                Err(Refused::Answered(failure, error)) => {
                    let event = error
                        .map(|error| json!({"type":"error","status":failure.status,"error":error}));
                    return Err((failure, event));
                }
            }
        }
    }

    /// Sends one `response.create` on the bound socket and relays its events. Events before
    /// the first output are held back so a refusal can still move the turn.
    async fn exchange(
        &mut self,
        lease: &mut Lease,
        text: String,
        alias: &str,
        deadline: Instant,
    ) -> Outcome {
        enum Step {
            Upstream(Option<Incoming>),
            Client(FromClient),
        }
        let Some(upstream) = self.upstream.as_mut() else {
            return Outcome::Stale;
        };
        if !upstream.send(text).await {
            self.upstream = None;
            return Outcome::Stale;
        }
        let mut prelude: Vec<Value> = Vec::new();
        // The turn's events, for rebuilding an empty terminal output.
        let mut events: Vec<Value> = Vec::new();
        let mut output = false;
        // Only upstream events count as progress: client traffic never extends a stall.
        let mut last_upstream = Instant::now();
        loop {
            let wait = if output {
                last_upstream + self.timing.idle
            } else {
                deadline
            };
            let Some(upstream) = self.upstream.as_mut() else {
                return Outcome::Stale;
            };
            let step = tokio::select! {
                incoming = tokio::time::timeout_at(wait, upstream.next()) => Step::Upstream(incoming.ok()),
                message = self.client.recv() => Step::Client(from_client(message)),
            };
            let mut event = match step {
                Step::Client(FromClient::Frame(frame)) => {
                    if self.queued.len() < MAX_QUEUED {
                        self.queued.push_back(frame);
                    } else if !self
                        .reply(error_event(
                            400,
                            "too_many_queued_frames",
                            "Too many frames sent while a turn is in flight; this frame was dropped",
                        ))
                        .await
                    {
                        return Outcome::ClientGone;
                    }
                    continue;
                }
                Step::Client(FromClient::Invalid(message)) => {
                    tracing::warn!("Codex websocket frame ignored mid-turn: {message}");
                    continue;
                }
                Step::Client(FromClient::Skip) => continue,
                Step::Client(FromClient::Gone) => return Outcome::ClientGone,
                Step::Upstream(None) => {
                    self.upstream = None;
                    let message = if output {
                        "ChatGPT stopped sending events mid-turn"
                    } else {
                        "ChatGPT produced no output before the request timeout"
                    };
                    lease.settle(&transport_fault(504, message));
                    return self
                        .finish(error_event(504, "upstream_timeout", message))
                        .await;
                }
                Step::Upstream(Some(Incoming::Closed)) => {
                    self.upstream = None;
                    if !output && prelude.is_empty() {
                        return Outcome::Stale;
                    }
                    let message = "ChatGPT connection closed mid-turn; retry the turn";
                    lease.settle(&transport_fault(502, message));
                    return self
                        .finish(error_event(502, "upstream_disconnected", message))
                        .await;
                }
                Step::Upstream(Some(Incoming::Event(event))) => {
                    last_upstream = Instant::now();
                    event
                }
            };
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if std::mem::take(&mut self.trailing_failure)
                && kind == "response.failed"
                && prelude.is_empty()
            {
                // The second half of the refusal this socket already reported.
                continue;
            }
            super::responses_relay::rename(&mut event, alias);
            if matches!(kind.as_str(), "error" | "response.failed") {
                let failure = errors::classify_event(&event, now());
                if !output {
                    self.trailing_failure = kind == "error";
                    return Outcome::Refused {
                        failure,
                        prelude,
                        event,
                    };
                }
                // After output too, a bare `error` is often followed by its `response.failed`.
                self.trailing_failure = kind == "error";
                lease.settle(&failure);
                return self.finish(event).await;
            }
            let terminal = matches!(
                kind.as_str(),
                "response.completed"
                    | "response.done"
                    | "response.incomplete"
                    | "response.cancelled"
            );
            events.push(event.clone());
            if terminal && kind != "response.cancelled" {
                self.remember(&events, &lease.account.id);
            }
            if output {
                if !self.reply(event).await {
                    return Outcome::ClientGone;
                }
            } else {
                output = terminal || super::responses_wire::carries_output(&event);
                prelude.push(event);
                if output {
                    for event in std::mem::take(&mut prelude) {
                        if !self.reply(event).await {
                            return Outcome::ClientGone;
                        }
                    }
                }
            }
            if terminal {
                if kind == "response.cancelled" {
                    // A cancellation says nothing about the account.
                    lease.settle(&Failure::classified(
                        502,
                        "ChatGPT response was cancelled",
                        Penalty::Neutral,
                        false,
                    ));
                } else {
                    lease.success();
                }
                return Outcome::Done;
            }
        }
    }

    async fn finish(&mut self, event: Value) -> Outcome {
        if self.reply(event).await {
            Outcome::Done
        } else {
            Outcome::ClientGone
        }
    }

    /// Records a completed response in the transcript, owned by `account`. An empty
    /// terminal output is rebuilt from the turn's output items (else its deltas).
    fn remember(&mut self, events: &[Value], account: &str) {
        let Ok(response) = super::responses_wire::aggregate(events) else {
            return;
        };
        let Some(id) = response["id"].as_str().filter(|id| !id.is_empty()) else {
            return;
        };
        let output = response["output"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        self.transcript.record_response(id, output);
        if self.owners.len() >= MAX_OWNERS {
            self.owners.clear();
        }
        self.owners.insert(id.to_owned(), account.to_owned());
    }
}

#[cfg(test)]
mod tests;
