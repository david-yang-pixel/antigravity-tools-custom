use super::transcript::{Rebuild, Transcript, TRANSCRIPT_LIMIT};
use serde_json::{json, Value};

fn user(text: &str) -> Value {
    json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
}
fn assistant(text: &str) -> Value {
    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
}
fn reasoning(cipher: &str) -> Value {
    json!({"type":"reasoning","summary":[],"encrypted_content":cipher})
}

/// Turn 1 (`u1` → `r1: a1`) and turn 2 continuing `r1` (`u2` → `r2: a2`).
fn two_turns(limit: usize) -> Transcript {
    let mut t = Transcript::new(limit);
    t.begin(None);
    t.record_request(&[user("u1")]);
    t.record_response("r1", &[assistant("a1")]);
    t.begin(Some("r1"));
    t.record_request(&[user("u2")]);
    t.record_response("r2", &[assistant("a2")]);
    t
}

#[test]
fn rebuild_includes_history_up_to_reference_plus_new_input() {
    let t = two_turns(TRANSCRIPT_LIMIT);
    assert_eq!(
        t.rebuild("r1", &[user("u3")]),
        Rebuild::Full(vec![user("u1"), assistant("a1"), user("u3")])
    );
    assert_eq!(
        t.rebuild("r2", &[user("u3")]),
        Rebuild::Full(vec![
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2"),
            user("u3")
        ])
    );
    // A branch from r1 replaces what followed it.
    let mut t = two_turns(TRANSCRIPT_LIMIT);
    t.begin(Some("r1"));
    t.record_request(&[user("u2b")]);
    t.record_response("r2b", &[assistant("a2b")]);
    assert_eq!(
        t.rebuild("r2b", &[]),
        Rebuild::Full(vec![
            user("u1"),
            assistant("a1"),
            user("u2b"),
            assistant("a2b")
        ])
    );
    assert_eq!(t.rebuild("r2", &[]), Rebuild::Unknown);
}

#[test]
fn rebuild_strips_encrypted_reasoning_and_compaction() {
    let call = json!({"type":"function_call","call_id":"c1","name":"ls","arguments":"{}"});
    let output = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
    let mut t = Transcript::new(TRANSCRIPT_LIMIT);
    t.begin(None);
    t.record_request(&[
        user("u1"),
        json!({"type":"compaction","encrypted_content":"cmp"}),
    ]);
    t.record_response("r1", &[reasoning("enc-1"), call.clone()]);
    let Rebuild::Full(items) = t.rebuild("r1", &[reasoning("enc-2"), output.clone()]) else {
        panic!("expected a full rebuild");
    };
    assert_eq!(items, vec![user("u1"), call, output]);
    assert!(!Value::Array(items)
        .to_string()
        .contains("encrypted_content"));
}

#[test]
fn rebuild_unknown_id_is_unknown() {
    let t = two_turns(TRANSCRIPT_LIMIT);
    assert_eq!(t.rebuild("resp_elsewhere", &[user("u3")]), Rebuild::Unknown);
    // A turn continuing an unknown response is not recorded.
    let mut t = two_turns(TRANSCRIPT_LIMIT);
    t.begin(Some("resp_elsewhere"));
    t.record_request(&[user("u3")]);
    t.record_response("r3", &[assistant("a3")]);
    assert_eq!(t.rebuild("r3", &[]), Rebuild::Unknown);
    // A full turn starts a new conversation.
    let mut t = two_turns(TRANSCRIPT_LIMIT);
    t.begin(None);
    t.record_request(&[user("x")]);
    t.record_response("r9", &[assistant("y")]);
    assert_eq!(t.rebuild("r1", &[]), Rebuild::Unknown);
    assert_eq!(
        t.rebuild("r9", &[]),
        Rebuild::Full(vec![user("x"), assistant("y")])
    );
}

#[test]
fn rebuild_orphaned_output_is_refused() {
    let orphan = json!({"type":"function_call_output","call_id":"c9","output":"ok"});
    let t = two_turns(TRANSCRIPT_LIMIT);
    assert_eq!(t.rebuild("r2", &[orphan.clone()]), Rebuild::Orphaned);
    let mut t = Transcript::new(TRANSCRIPT_LIMIT);
    t.begin(None);
    t.record_request(&[user("u1"), orphan]);
    t.record_response("r1", &[assistant("a1")]);
    assert_eq!(t.rebuild("r1", &[user("u2")]), Rebuild::Orphaned);
    let custom = json!({"type":"custom_tool_call_output","call_id":"c8","output":"ok"});
    assert_eq!(t.rebuild("r1", &[custom]), Rebuild::Orphaned);
}

#[test]
fn memory_bound_drops_oldest_and_reports_dropped() {
    let item = user(&"x".repeat(100));
    let size = item.to_string().len();
    let mut t = Transcript::new(size * 3);
    t.begin(None);
    t.record_request(&[item.clone()]);
    t.record_response("r1", &[item.clone()]);
    assert!(matches!(t.rebuild("r1", &[]), Rebuild::Full(v) if v.len() == 2));
    t.begin(Some("r1"));
    t.record_request(&[item.clone()]);
    t.record_response("r2", &[item.clone()]);
    assert_eq!(t.len(), 3, "the oldest item is dropped above the limit");
    assert_eq!(t.rebuild("r2", &[user("next")]), Rebuild::Dropped);
    assert_eq!(t.rebuild("r1", &[]), Rebuild::Dropped);
    // A fresh conversation is whole again.
    t.begin(None);
    t.record_request(&[user("u")]);
    t.record_response("r3", &[assistant("a")]);
    assert_eq!(
        t.rebuild("r3", &[]),
        Rebuild::Full(vec![user("u"), assistant("a")])
    );
}

#[test]
fn rejected_digests_strip_matching_items_and_cap_at_512() {
    let mut t = Transcript::new(TRANSCRIPT_LIMIT);
    t.begin(None);
    t.record_request(&[user("u1")]);
    t.record_response("r1", &[reasoning("enc-x"), assistant("a1")]);
    t.begin(Some("r1"));
    t.record_request(&[user("u2")]);
    t.record_response("r2", &[assistant("a2")]);
    assert_eq!(t.len(), 5);
    t.reject(&[reasoning("enc-x"), user("no cipher")]);
    assert_eq!(t.len(), 4, "rejected ciphertext leaves the transcript");
    assert_eq!(
        t.rebuild("r1", &[]),
        Rebuild::Full(vec![user("u1"), assistant("a1")])
    );
    assert_eq!(
        t.rebuild("r2", &[]),
        Rebuild::Full(vec![
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2")
        ])
    );
    let mut later = vec![reasoning("enc-x"), reasoning("enc-y"), user("u3")];
    t.strip_rejected(&mut later);
    assert_eq!(later, vec![reasoning("enc-y"), user("u3")]);
    // Recorded later turns drop rejected ciphertexts too.
    t.begin(Some("r2"));
    t.record_request(&[reasoning("enc-x"), user("u3")]);
    assert_eq!(t.len(), 5);

    let mut t = Transcript::new(TRANSCRIPT_LIMIT);
    let rejected: Vec<Value> = (0..513).map(|n| reasoning(&format!("enc-{n}"))).collect();
    t.reject(&rejected);
    let mut items = vec![reasoning("enc-0"), reasoning("enc-1"), reasoning("enc-512")];
    t.strip_rejected(&mut items);
    assert_eq!(
        items,
        vec![reasoning("enc-0")],
        "the oldest digest is evicted"
    );
}

/// The relay against a local mock of the Codex WebSocket upstream, driven through the
/// public `/v1/responses` WebSocket handler.
mod relay {
    use super::super::{set_test_timing, Timing};
    use super::{assistant, reasoning, user};
    use crate::modules::native_providers::{
        chatgpt::endpoints, now, store, tests::Sandbox, Account, Model, Provider, Realm,
    };
    use axum::{
        extract::{
            ws::{Message, WebSocket, WebSocketUpgrade},
            State,
        },
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    const PUBLIC: &str = "chatgpt-ws";
    const NATIVE: &str = "gpt-test";
    const AG_PUBLIC: &str = "ws-antigravity";

    #[derive(Clone, Debug)]
    enum Seen {
        Handshake { account: String, headers: HeaderMap },
        Frame { account: String, body: Value },
        Ping,
    }

    /// What one upstream frame gets back.
    enum Step {
        Events(Vec<Value>),
        /// Events, then the upstream closes the socket.
        Close(Vec<Value>),
        /// Events after a pause.
        After(Duration, Vec<Value>),
    }

    /// `nth` counts earlier handshakes (or frames) of the calling account; `first` says
    /// whether it is the first account that connected.
    struct Call {
        nth: usize,
        first: bool,
    }
    type Handshake = Box<dyn Fn(&Call) -> Option<(u16, Value)> + Send + Sync>;
    type Reply = Box<dyn Fn(&Call, &Value) -> Step + Send + Sync>;

    struct Mock {
        seen: Mutex<Vec<Seen>>,
        first: Mutex<Option<String>>,
        handshake: Handshake,
        reply: Reply,
        accept_headers: Vec<(&'static str, &'static str)>,
    }
    impl Mock {
        fn count(&self, account: &str, frames: bool) -> usize {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|s| match s {
                    Seen::Handshake { account: a, .. } => !frames && a == account,
                    Seen::Frame { account: a, .. } => frames && a == account,
                    Seen::Ping => false,
                })
                .count()
        }
        fn first(&self, account: &str) -> bool {
            self.first
                .lock()
                .unwrap()
                .get_or_insert_with(|| account.to_owned())
                == account
        }
    }

    struct Upstream {
        mock: Arc<Mock>,
        _reset: Reset,
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            endpoints::set_test(None);
            set_test_timing(None);
        }
    }
    impl Upstream {
        fn seen(&self) -> Vec<Seen> {
            self.mock.seen.lock().unwrap().clone()
        }
        fn frames(&self) -> Vec<(String, Value)> {
            self.seen()
                .into_iter()
                .filter_map(|s| match s {
                    Seen::Frame { account, body } => Some((account, body)),
                    _ => None,
                })
                .collect()
        }
        fn handshakes(&self) -> Vec<(String, HeaderMap)> {
            self.seen()
                .into_iter()
                .filter_map(|s| match s {
                    Seen::Handshake { account, headers } => Some((account, headers)),
                    _ => None,
                })
                .collect()
        }
    }

    async fn handshake(
        State(mock): State<Arc<Mock>>,
        ws: WebSocketUpgrade,
        headers: HeaderMap,
    ) -> Response {
        let account = headers
            .get("chatgpt-account-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let call = Call {
            nth: mock.count(&account, false),
            first: mock.first(&account),
        };
        let rejected = (mock.handshake)(&call);
        mock.seen.lock().unwrap().push(Seen::Handshake {
            account: account.clone(),
            headers,
        });
        if let Some((status, body)) = rejected {
            return (
                StatusCode::from_u16(status).unwrap(),
                [("content-type", "application/json")],
                body.to_string(),
            )
                .into_response();
        }
        let extra = mock.accept_headers.clone();
        let mut response = ws.on_upgrade(move |socket| serve(mock, account, socket));
        for (k, v) in extra {
            response.headers_mut().insert(k, v.parse().unwrap());
        }
        response
    }

    async fn serve(mock: Arc<Mock>, account: String, mut socket: WebSocket) {
        while let Some(Ok(message)) = socket.recv().await {
            match message {
                Message::Text(text) => {
                    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let call = Call {
                        nth: mock.count(&account, true),
                        first: mock.first(&account),
                    };
                    let step = (mock.reply)(&call, &body);
                    mock.seen.lock().unwrap().push(Seen::Frame {
                        account: account.clone(),
                        body,
                    });
                    let (events, close) = match step {
                        Step::Events(events) => (events, false),
                        Step::Close(events) => (events, true),
                        Step::After(pause, events) => {
                            tokio::time::sleep(pause).await;
                            (events, false)
                        }
                    };
                    for event in events {
                        if socket.send(Message::Text(event.to_string())).await.is_err() {
                            return;
                        }
                    }
                    if close {
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    }
                }
                Message::Ping(_) => mock.seen.lock().unwrap().push(Seen::Ping),
                Message::Close(_) => return,
                _ => {}
            }
        }
    }

    async fn upstream(handshake_rule: Handshake, reply: Reply) -> Upstream {
        upstream_with(handshake_rule, reply, Vec::new()).await
    }
    async fn upstream_with(
        handshake_rule: Handshake,
        reply: Reply,
        accept_headers: Vec<(&'static str, &'static str)>,
    ) -> Upstream {
        let mock = Arc::new(Mock {
            seen: Mutex::new(Vec::new()),
            first: Mutex::new(None),
            handshake: handshake_rule,
            reply,
            accept_headers,
        });
        let app = Router::new()
            .route("/backend-api/codex/responses", get(handshake))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        endpoints::set_test(Some(endpoints::Endpoints {
            auth: format!("http://{addr}"),
            backend: format!("http://{addr}/backend-api"),
        }));
        Upstream {
            mock,
            _reset: Reset,
        }
    }
    fn accept_all() -> Handshake {
        Box::new(|_| None)
    }

    fn chatgpt_account(n: usize) -> Account {
        let mut a = Account::new(Provider::ChatGpt, Realm::Global);
        a.uid = format!("acc-{n}:u{n}");
        a.nickname = format!("user{n}@example.com");
        a.credentials.access_token = format!("access-{n}");
        a.credentials.refresh_token = format!("refresh-{n}");
        a.credentials.chatgpt_account_id = format!("acc-{n}");
        a.credentials.user_type = "plus".into();
        a.expires_at = now() + 7200;
        a.models = vec![Model {
            id: NATIVE.into(),
            name: "GPT test".into(),
            ..Default::default()
        }];
        store::upsert(a, None).unwrap()
    }

    /// `count` ChatGPT accounts behind `PUBLIC`, plus a legacy Antigravity publication.
    fn setup(count: usize) -> Vec<Account> {
        use crate::modules::model_registry as registry;
        let accounts = (0..count).map(chatgpt_account).collect();
        let legacy = vec![AG_PUBLIC.to_string()];
        let mapping = std::collections::HashMap::new();
        let view = registry::view(&legacy, &mapping).unwrap();
        let mut published = view.published_models.clone();
        published.push(registry::PublishedModel {
            id: format!("new:{PUBLIC}"),
            public_id: PUBLIC.into(),
            display_name: PUBLIC.into(),
            source_id: Provider::ChatGpt.source().into(),
            native_model: NATIVE.into(),
            extra_native_models: Vec::new(),
            enabled: true,
            legacy: false,
        });
        registry::save(
            registry::SaveModelRegistryRequest {
                revision: view.revision,
                ignored_candidate_ids: vec![],
                allow_unverified: true,
                published_models: published,
            },
            &legacy,
            &mapping,
        )
        .unwrap();
        accounts
    }

    struct Client {
        socket: reqwest_websocket::WebSocket,
    }
    async fn connect() -> Client {
        connect_with(&[]).await
    }
    /// A client whose upgrade request carries `headers`.
    async fn connect_with(headers: &[(&str, &str)]) -> Client {
        use reqwest_websocket::RequestBuilderExt;
        let monitor = Arc::new(crate::proxy::monitor::ProxyMonitor::disabled_for_test());
        let app = Router::new()
            .route(
                "/v1/responses",
                get(crate::proxy::handlers::openai::handle_responses_websocket),
            )
            .with_state(crate::proxy::server::test_app_state(monitor));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let socket = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("ws://{addr}/v1/responses"))
            .headers(
                headers
                    .iter()
                    .map(|(k, v)| {
                        (
                            reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                            v.parse().unwrap(),
                        )
                    })
                    .collect(),
            )
            .upgrade()
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();
        Client { socket }
    }
    impl Client {
        async fn send(&mut self, frame: Value) {
            self.socket
                .send(reqwest_websocket::Message::Text(frame.to_string()))
                .await
                .unwrap();
        }
        /// The next message, or `None` when the socket closed.
        async fn next(&mut self) -> Option<reqwest_websocket::Message> {
            tokio::time::timeout(Duration::from_secs(10), self.socket.next())
                .await
                .expect("relay answered in time")
                .and_then(Result::ok)
        }
        /// Events up to and including a terminal or error event.
        async fn turn(&mut self, frame: Value) -> Vec<Value> {
            self.send(frame).await;
            let mut events = Vec::new();
            loop {
                match self.next().await {
                    Some(reqwest_websocket::Message::Text(text)) => {
                        let event: Value = serde_json::from_str(&text).unwrap();
                        let end = matches!(
                            event["type"].as_str(),
                            Some(
                                "response.completed"
                                    | "response.done"
                                    | "response.incomplete"
                                    | "response.failed"
                                    | "error"
                            )
                        );
                        events.push(event);
                        if end {
                            return events;
                        }
                    }
                    Some(_) => {}
                    None => panic!("relay closed the socket; events so far: {events:?}"),
                }
            }
        }
    }

    fn turn_events(id: &str, output: Vec<Value>) -> Vec<Value> {
        vec![
            json!({"type":"response.created","response":{"id":id,"model":NATIVE,"status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hi"}),
            json!({"type":"response.completed","response":{"id":id,"model":NATIVE,"status":"completed","output":output,
                "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}),
        ]
    }
    fn answer(id: &str, text: &str) -> Vec<Value> {
        turn_events(id, vec![assistant(text)])
    }
    fn error_event(code: &str, message: &str) -> Value {
        json!({"type":"error","status":400,"error":{"type":"invalid_request_error","code":code,"message":message}})
    }
    fn create(input: Vec<Value>) -> Value {
        json!({"type":"response.create","model":PUBLIC,"instructions":"Be brief.","input":input})
    }
    fn continue_from(previous: &str, input: Vec<Value>) -> Value {
        let mut frame = create(input);
        frame["previous_response_id"] = json!(previous);
        frame
    }
    fn completed(events: &[Value]) -> &Value {
        events
            .iter()
            .find(|e| e["type"] == "response.completed")
            .unwrap_or_else(|| panic!("no completed event in {events:?}"))
    }
    /// Input items without the ids the payload rules strip.
    fn texts(body: &Value) -> Vec<(String, String)> {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                let text = item["content"][0]["text"]
                    .as_str()
                    .or_else(|| item["encrypted_content"].as_str())
                    .unwrap_or("")
                    .to_string();
                let kind = item["role"]
                    .as_str()
                    .or_else(|| item["type"].as_str())
                    .unwrap_or("")
                    .to_string();
                (kind, text)
            })
            .collect()
    }
    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn ws_relays_turns_and_rewrites_model() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| Step::Events(answer(&format!("resp_{}", call.nth), "Hi"))),
        )
        .await;
        let mut client = connect().await;
        let mut first = create(vec![user("hi")]);
        first["background"] = json!(true);
        let events = client.turn(first).await;
        assert!(events
            .iter()
            .any(|e| e["type"] == "response.output_text.delta"));
        for event in events.iter().filter(|e| e.get("response").is_some()) {
            assert_eq!(event["response"]["model"], PUBLIC, "{event}");
        }
        let events = client
            .turn(continue_from("resp_0", vec![user("more")]))
            .await;
        assert_eq!(completed(&events)["response"]["id"], "resp_1");
        assert_eq!(completed(&events)["response"]["model"], PUBLIC);

        let frames = server.frames();
        assert_eq!(frames.len(), 2);
        for (_, body) in &frames {
            assert_eq!(body["type"], "response.create");
            assert_eq!(body["model"], NATIVE);
            assert_eq!(body["store"], false);
            assert_eq!(body["stream"], true);
            assert!(body.get("background").is_none());
        }
        let handshakes = server.handshakes();
        assert_eq!(handshakes.len(), 1, "one upstream socket serves both turns");
        let headers = &handshakes[0].1;
        assert_eq!(headers["openai-beta"], "responses_websockets=2026-02-06");
        assert_eq!(headers["authorization"], "Bearer access-0");
        assert!(headers.contains_key("session_id"));
        assert!(headers["user-agent"]
            .to_str()
            .unwrap()
            .starts_with("codex_cli_rs/"));
    }

    #[tokio::test]
    async fn ws_keeps_previous_response_id_on_same_account() {
        let _sandbox = Sandbox::new();
        setup(2);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| Step::Events(answer(&format!("resp_{}", call.nth), "a"))),
        )
        .await;
        let mut client = connect().await;
        client.turn(create(vec![user("u1")])).await;
        client.turn(continue_from("resp_0", vec![user("u2")])).await;
        let frames = server.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].0, frames[1].0,
            "the same account serves both turns"
        );
        assert_eq!(frames[1].1["previous_response_id"], "resp_0");
        assert_eq!(texts(&frames[1].1), pairs(&[("user", "u2")]));
        assert_eq!(server.handshakes().len(), 1);
    }

    #[tokio::test]
    async fn ws_switch_rebuilds_full_input_without_previous_id() {
        let _sandbox = Sandbox::new();
        setup(2);
        let server = upstream(
            Box::new(|call| {
                (call.first && call.nth == 1).then(|| {
                    (
                        429,
                        json!({"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":3600}}),
                    )
                })
            }),
            Box::new(|call, _| {
                if call.first {
                    Step::Close(turn_events(
                        "resp_A1",
                        vec![reasoning("enc-A"), assistant("a1")],
                    ))
                } else {
                    Step::Events(answer("resp_B1", "b1"))
                }
            }),
        )
        .await;
        let mut client = connect().await;
        client
            .turn(create(vec![user("u1"), reasoning("enc-client")]))
            .await;
        let events = client
            .turn(continue_from("resp_A1", vec![user("u2")]))
            .await;
        assert_eq!(completed(&events)["response"]["id"], "resp_B1");
        assert_eq!(completed(&events)["response"]["model"], PUBLIC);
        assert!(!events.iter().any(|e| e["type"] == "error"), "{events:?}");

        let frames = server.frames();
        assert_eq!(frames.len(), 2);
        assert_ne!(
            frames[0].0, frames[1].0,
            "the turn moved to another account"
        );
        let moved = &frames[1].1;
        assert!(moved.get("previous_response_id").is_none(), "{moved}");
        assert!(!moved.to_string().contains("encrypted_content"), "{moved}");
        assert_eq!(
            texts(moved),
            pairs(&[("user", "u1"), ("assistant", "a1"), ("user", "u2")])
        );
        let handshakes = server.handshakes();
        assert_eq!(handshakes.len(), 3, "{handshakes:?}");
    }

    #[tokio::test]
    async fn ws_unknown_previous_id_forwarded_unchanged() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|_, _| {
                Step::Events(vec![error_event(
                    "previous_response_not_found",
                    "Previous response with id 'resp_elsewhere' not found.",
                )])
            }),
        )
        .await;
        let mut client = connect().await;
        let events = client
            .turn(continue_from("resp_elsewhere", vec![user("u2")]))
            .await;
        let last = events.last().unwrap();
        assert_eq!(last["type"], "error");
        assert_eq!(last["error"]["code"], "previous_response_not_found");
        let frames = server.frames();
        assert_eq!(frames.len(), 1, "an unknown reference is not replayed");
        assert_eq!(frames[0].1["previous_response_id"], "resp_elsewhere");
        assert_eq!(texts(&frames[0].1), pairs(&[("user", "u2")]));
    }

    #[tokio::test]
    async fn ws_previous_response_not_found_replays_once() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| match call.nth {
                0 => Step::Events(answer("resp_1", "a1")),
                1 => Step::Events(vec![error_event(
                    "previous_response_not_found",
                    "Previous response with id 'resp_1' not found.",
                )]),
                _ => Step::Events(answer("resp_2", "a2")),
            }),
        )
        .await;
        let mut client = connect().await;
        client.turn(create(vec![user("u1")])).await;
        let events = client.turn(continue_from("resp_1", vec![user("u2")])).await;
        assert!(!events.iter().any(|e| e["type"] == "error"), "{events:?}");
        assert_eq!(completed(&events)["response"]["id"], "resp_2");
        let frames = server.frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1].1["previous_response_id"], "resp_1");
        let replay = &frames[2].1;
        assert!(replay.get("previous_response_id").is_none());
        assert_eq!(
            texts(replay),
            pairs(&[("user", "u1"), ("assistant", "a1"), ("user", "u2")])
        );
        assert_eq!(server.handshakes().len(), 1);
    }

    #[tokio::test]
    async fn ws_invalid_encrypted_content_strips_and_retries_once() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| match call.nth {
                0 => Step::Events(vec![error_event(
                    "invalid_encrypted_content",
                    "The encrypted content for item rs_1 could not be verified.",
                )]),
                n => Step::Events(answer(&format!("resp_{n}"), "a")),
            }),
        )
        .await;
        let mut client = connect().await;
        let events = client
            .turn(create(vec![
                user("u1"),
                reasoning("enc-bad"),
                assistant("a0"),
                user("u2"),
            ]))
            .await;
        assert!(!events.iter().any(|e| e["type"] == "error"), "{events:?}");
        client
            .turn(create(vec![
                user("u1"),
                reasoning("enc-bad"),
                assistant("a0"),
                user("u2"),
                reasoning("enc-good"),
                assistant("a1"),
                user("u3"),
            ]))
            .await;
        let frames = server.frames();
        assert_eq!(frames.len(), 3);
        assert!(frames[0].1.to_string().contains("enc-bad"));
        assert!(!frames[1].1.to_string().contains("enc-bad"));
        assert_eq!(
            texts(&frames[1].1),
            pairs(&[("user", "u1"), ("assistant", "a0"), ("user", "u2")])
        );
        let later = frames[2].1.to_string();
        assert!(
            !later.contains("enc-bad"),
            "rejected digests are stripped later"
        );
        assert!(later.contains("enc-good"));
    }

    #[tokio::test]
    async fn ws_idle_socket_pings_before_turn() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| Step::Events(answer(&format!("resp_{}", call.nth), "a"))),
        )
        .await;
        set_test_timing(Some(Timing {
            ping_after: Duration::from_millis(500),
            pong_timeout: Duration::from_secs(5),
            idle: Duration::from_secs(300),
        }));
        let mut client = connect().await;
        client.turn(create(vec![user("u1")])).await;
        client.turn(continue_from("resp_0", vec![user("u2")])).await;
        assert!(
            !server.seen().iter().any(|s| matches!(s, Seen::Ping)),
            "a busy socket is not pinged"
        );
        tokio::time::sleep(Duration::from_millis(800)).await;
        client.turn(continue_from("resp_1", vec![user("u3")])).await;
        let seen = server.seen();
        let ping = seen
            .iter()
            .position(|s| matches!(s, Seen::Ping))
            .expect("an idle socket is pinged before the turn");
        let third = seen
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, Seen::Frame { .. }))
            .nth(2)
            .unwrap()
            .0;
        assert!(ping < third, "{seen:?}");
        assert_eq!(
            server.handshakes().len(),
            1,
            "the answered ping keeps the socket"
        );
    }

    #[tokio::test]
    async fn ws_client_idle_closes_after_300s() {
        let _sandbox = Sandbox::new();
        setup(1);
        let _server = upstream(
            accept_all(),
            Box::new(|call, _| Step::Events(answer(&format!("resp_{}", call.nth), "a"))),
        )
        .await;
        set_test_timing(Some(Timing {
            ping_after: Duration::from_secs(20),
            pong_timeout: Duration::from_secs(5),
            idle: Duration::from_millis(300),
        }));
        let mut client = connect().await;
        client.turn(create(vec![user("u1")])).await;
        let started = std::time::Instant::now();
        let close = loop {
            match client.next().await {
                Some(reqwest_websocket::Message::Close { code, .. }) => break code,
                Some(_) => continue,
                None => panic!("socket ended without a close frame"),
            }
        };
        assert_eq!(close, reqwest_websocket::CloseCode::Normal);
        assert!(started.elapsed() >= Duration::from_millis(250));
    }

    #[tokio::test]
    async fn ws_handshake_quota_headers_recorded() {
        let _sandbox = Sandbox::new();
        let accounts = setup(1);
        let _server = upstream_with(
            accept_all(),
            Box::new(|_, _| Step::Events(answer("resp_1", "a"))),
            vec![
                ("x-codex-primary-used-percent", "40"),
                ("x-codex-primary-window-minutes", "300"),
                ("x-codex-primary-reset-after-seconds", "600"),
                ("x-codex-secondary-used-percent", "10"),
                ("x-codex-secondary-window-minutes", "10080"),
                ("x-codex-secondary-reset-after-seconds", "86400"),
            ],
        )
        .await;
        let mut client = connect().await;
        client.turn(create(vec![user("u1")])).await;
        let quota = store::get(Provider::ChatGpt, &accounts[0].id)
            .unwrap()
            .quota
            .expect("handshake quota headers are recorded");
        let short = quota.packs.iter().find(|p| p.name == "5h").unwrap();
        assert_eq!(short.used, Some(40.0));
        let long = quota.packs.iter().find(|p| p.name == "7d").unwrap();
        assert_eq!(long.used, Some(10.0));
    }

    #[tokio::test]
    async fn ws_handshake_rejection_body_is_read_under_the_deadline() {
        use super::super::upstream::{connect, Refused};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _sandbox = Sandbox::new();
        let account = chatgpt_account(0);
        // A 403 whose body never finishes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let head = "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{\"error\"";
            socket.write_all(head.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        endpoints::set_test(Some(endpoints::Endpoints {
            auth: format!("http://{addr}"),
            backend: format!("http://{addr}/backend-api"),
        }));
        let _reset = Reset;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            connect(&account, "session", &HeaderMap::new(), deadline),
        )
        .await
        .expect("the rejection body is read under the request deadline");
        match result {
            Err(Refused::Answered(failure, _)) => assert_eq!(failure.status, 403),
            Err(Refused::Transport(failure)) => panic!("transport failure: {failure:?}"),
            Ok(_) => panic!("a 403 handshake must not upgrade"),
        }
    }

    #[tokio::test]
    async fn ws_antigravity_session_rejects_chatgpt_frame() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|_, _| Step::Events(answer("resp_1", "a"))),
        )
        .await;
        let mut client = connect().await;
        // An Antigravity turn (it fails without Google accounts; the session is still
        // Antigravity's).
        let events = client
            .turn(json!({"type":"response.create","model":AG_PUBLIC,"input":[user("hi")]}))
            .await;
        assert!(!events.is_empty());
        let events = client.turn(create(vec![user("u2")])).await;
        let last = events.last().unwrap();
        assert_eq!(last["type"], "error", "{events:?}");
        assert!(
            last["error"]["message"]
                .as_str()
                .unwrap()
                .contains("new WebSocket connection"),
            "{last}"
        );
        assert!(server.seen().is_empty(), "no upstream connection is opened");
    }

    #[tokio::test]
    async fn ws_local_prewarm_continues_into_relay() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|_, _| Step::Events(answer("resp_1", "a1"))),
        )
        .await;
        let mut client = connect().await;
        let mut prewarm = create(vec![user("u0")]);
        prewarm["generate"] = json!(false);
        let events = client.turn(prewarm).await;
        let prewarm_id = completed(&events)["response"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(server.seen().is_empty(), "the prewarm is answered locally");
        let events = client
            .turn(continue_from(&prewarm_id, vec![user("u1")]))
            .await;
        assert_eq!(completed(&events)["response"]["id"], "resp_1");
        let frames = server.frames();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].1.get("previous_response_id").is_none());
        assert!(frames[0].1.get("generate").is_none());
        assert_eq!(
            texts(&frames[0].1),
            pairs(&[("user", "u0"), ("user", "u1")])
        );
    }

    #[tokio::test]
    async fn ws_mid_turn_drop_sends_error_and_next_turn_reconnects() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| {
                if call.nth == 0 {
                    let mut events = answer("resp_0", "a");
                    events.pop();
                    Step::Close(events)
                } else {
                    Step::Events(answer("resp_1", "b"))
                }
            }),
        )
        .await;
        let mut client = connect().await;
        let events = client.turn(create(vec![user("u1")])).await;
        assert!(events
            .iter()
            .any(|e| e["type"] == "response.output_text.delta"));
        let last = events.last().unwrap();
        assert_eq!(last["type"], "error");
        assert_eq!(last["error"]["code"], "upstream_disconnected");
        let events = client.turn(create(vec![user("u1")])).await;
        assert_eq!(completed(&events)["response"]["id"], "resp_1");
        assert_eq!(server.handshakes().len(), 2);
    }

    #[tokio::test]
    async fn ws_pre_output_quota_event_moves_turn() {
        let _sandbox = Sandbox::new();
        setup(2);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| {
                if call.first {
                    Step::Events(vec![json!({"type":"error","error":{
                        "type":"usage_limit_reached","code":"usage_limit_reached",
                        "message":"The usage limit has been reached","resets_in_seconds":3600}})])
                } else {
                    Step::Events(answer("resp_B", "b"))
                }
            }),
        )
        .await;
        let mut client = connect().await;
        let events = client.turn(create(vec![user("u1")])).await;
        assert!(!events.iter().any(|e| e["type"] == "error"), "{events:?}");
        assert_eq!(completed(&events)["response"]["id"], "resp_B");
        let frames = server.frames();
        assert_eq!(frames.len(), 2);
        assert_ne!(frames[0].0, frames[1].0);
        assert_eq!(texts(&frames[1].1), pairs(&[("user", "u1")]));
    }

    #[tokio::test]
    async fn ws_validation_error_is_not_replayed() {
        let _sandbox = Sandbox::new();
        setup(2);
        let server = upstream(
            accept_all(),
            Box::new(|_, _| {
                Step::Events(vec![error_event(
                    "context_length_exceeded",
                    "Your input exceeds the context window of this model.",
                )])
            }),
        )
        .await;
        let mut client = connect().await;
        let events = client.turn(create(vec![user("u1")])).await;
        let last = events.last().unwrap();
        assert_eq!(last["error"]["code"], "context_length_exceeded");
        assert_eq!(server.frames().len(), 1);
        assert_eq!(server.handshakes().len(), 1);
    }

    #[tokio::test]
    async fn ws_failover_tries_at_most_three_accounts() {
        let _sandbox = Sandbox::new();
        setup(4);
        let server = upstream(
            Box::new(|_| {
                Some((
                    429,
                    json!({"error":{"type":"rate_limit_exceeded","message":"Slow down"}}),
                ))
            }),
            Box::new(|_, _| Step::Events(answer("resp_1", "a"))),
        )
        .await;
        let mut client = connect().await;
        let events = client.turn(create(vec![user("u1")])).await;
        let last = events.last().unwrap();
        assert_eq!(last["type"], "error");
        assert_eq!(last["status"], 429);
        assert_eq!(last["error"]["message"], "Slow down");
        let accounts: std::collections::HashSet<String> =
            server.handshakes().into_iter().map(|(a, _)| a).collect();
        assert_eq!(server.handshakes().len(), 3);
        assert_eq!(accounts.len(), 3);
    }

    #[tokio::test]
    async fn ws_trailing_failed_event_does_not_spoil_replay() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| match call.nth {
                0 => Step::Events(answer("resp_1", "a1")),
                1 => Step::Events(vec![
                    error_event(
                        "previous_response_not_found",
                        "Previous response with id 'resp_1' not found.",
                    ),
                    json!({"type":"response.failed","response":{"id":"resp_x","status":"failed",
                        "error":{"code":"previous_response_not_found","message":"Previous response with id 'resp_1' not found."}}}),
                ]),
                _ => Step::Events(answer("resp_2", "a2")),
            }),
        )
        .await;
        let mut client = connect().await;
        client.turn(create(vec![user("u1")])).await;
        let events = client.turn(continue_from("resp_1", vec![user("u2")])).await;
        assert!(!events.iter().any(|e| e["type"] == "error"), "{events:?}");
        assert_eq!(completed(&events)["response"]["id"], "resp_2");
        assert_eq!(server.frames().len(), 3);
    }

    #[tokio::test]
    async fn ws_trailing_failed_event_after_output_does_not_refuse_queued_frame() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| match call.nth {
                0 => {
                    let mut events = answer("resp_1", "a1");
                    events.truncate(3);
                    events.push(
                        json!({"type":"error","error":{"code":"server_error","message":"boom"}}),
                    );
                    events.push(
                        json!({"type":"response.failed","response":{"id":"resp_1","status":"failed",
                        "error":{"code":"server_error","message":"boom"}}}),
                    );
                    Step::After(Duration::from_millis(300), events)
                }
                _ => Step::Events(answer("resp_2", "a2")),
            }),
        )
        .await;
        let mut client = connect().await;
        client.send(create(vec![user("u1")])).await;
        // Queued while the first turn is in flight.
        let events = client.turn(create(vec![user("u2")])).await;
        assert_eq!(events.last().unwrap()["type"], "error", "{events:?}");
        let mut second = Vec::new();
        loop {
            match client.next().await {
                Some(reqwest_websocket::Message::Text(text)) => {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    let end = matches!(
                        event["type"].as_str(),
                        Some("response.completed" | "response.failed" | "error")
                    );
                    second.push(event);
                    if end {
                        break;
                    }
                }
                Some(_) => {}
                None => panic!("relay closed the socket; events so far: {second:?}"),
            }
        }
        assert_eq!(completed(&second)["response"]["id"], "resp_2", "{second:?}");
        assert_eq!(server.frames().len(), 2);
    }

    #[tokio::test]
    async fn ws_empty_terminal_output_is_rebuilt_from_output_items() {
        let _sandbox = Sandbox::new();
        setup(2);
        let message = json!({"type":"message","id":"msg_1","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"a1"}]});
        let call = json!({"type":"function_call","id":"fc_1","call_id":"c1","name":"ls","arguments":"{}","status":"completed"});
        let first_turn = vec![
            json!({"type":"response.created","response":{"id":"resp_A1","model":NATIVE,"status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"a1"}),
            json!({"type":"response.output_item.done","output_index":0,"item":message}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"c1","name":"ls","arguments":""}}),
            json!({"type":"response.output_item.done","output_index":1,"item":call}),
            json!({"type":"response.output_item.done","output_index":1,"item":call}),
            json!({"type":"response.completed","response":{"id":"resp_A1","model":NATIVE,"status":"completed","output":[]}}),
        ];
        let server = upstream(
            Box::new(|call| {
                (call.first && call.nth == 1).then(|| {
                    (
                        429,
                        json!({"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":3600}}),
                    )
                })
            }),
            Box::new(move |call, _| {
                if call.first {
                    Step::Close(first_turn.clone())
                } else {
                    Step::Events(answer("resp_B1", "b1"))
                }
            }),
        )
        .await;
        let mut client = connect().await;
        client.turn(create(vec![user("u1")])).await;
        let output = json!({"type":"function_call_output","call_id":"c1","output":"files"});
        let events = client.turn(continue_from("resp_A1", vec![output])).await;
        assert!(!events.iter().any(|e| e["type"] == "error"), "{events:?}");
        let frames = server.frames();
        assert_eq!(frames.len(), 2);
        let moved = &frames[1].1;
        assert!(moved.get("previous_response_id").is_none());
        let kinds: Vec<&str> = moved["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap_or("message"))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "message",
                "message",
                "function_call",
                "function_call_output"
            ],
            "{moved}"
        );
        assert_eq!(moved["input"][1]["content"][0]["text"], "a1");
        assert_eq!(moved["input"][2]["call_id"], "c1");
    }

    #[tokio::test]
    async fn ws_mid_turn_stall_ends_despite_client_pings() {
        let _sandbox = Sandbox::new();
        let accounts = setup(1);
        let mut partial = answer("resp_0", "a");
        partial.pop();
        let _server = upstream(
            accept_all(),
            Box::new(move |_, _| Step::Events(partial.clone())),
        )
        .await;
        set_test_timing(Some(Timing {
            ping_after: Duration::from_secs(20),
            pong_timeout: Duration::from_secs(5),
            idle: Duration::from_millis(400),
        }));
        let mut client = connect().await;
        client.send(create(vec![user("u1")])).await;
        let started = std::time::Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let error = loop {
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "client pings kept a stalled turn alive"
            );
            tokio::select! {
                message = client.socket.next() => {
                    if let Some(Ok(reqwest_websocket::Message::Text(text))) = message {
                        let event: Value = serde_json::from_str(&text).unwrap();
                        if event["type"] == "error" {
                            break event;
                        }
                    }
                }
                _ = tick.tick() => {
                    client
                        .socket
                        .send(reqwest_websocket::Message::Ping(reqwest_websocket::Bytes::from_static(b"k")))
                        .await
                        .unwrap();
                }
            }
        };
        assert_eq!(error["error"]["code"], "upstream_timeout");
        let released = std::time::Instant::now();
        while crate::modules::native_providers::accounts::runtime(&accounts[0].id).in_flight != 0 {
            assert!(
                released.elapsed() < Duration::from_secs(2),
                "the lease was not released"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn ws_forwards_client_handshake_allow_list() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|_, _| Step::Events(answer("resp_1", "a"))),
        )
        .await;
        let mut client = connect_with(&[
            ("x-codex-turn-state", "turn-abc"),
            ("accept-language", "fr-FR"),
            ("authorization", "Bearer client-key"),
            ("cookie", "session=secret"),
        ])
        .await;
        client.turn(create(vec![user("u1")])).await;
        let handshakes = server.handshakes();
        let headers = &handshakes[0].1;
        assert_eq!(headers["x-codex-turn-state"], "turn-abc");
        assert_eq!(headers["accept-language"], "fr-FR");
        assert_eq!(headers["authorization"], "Bearer access-0");
        assert!(headers.get("cookie").is_none());
    }

    #[tokio::test]
    async fn ws_queued_frames_are_capped() {
        let _sandbox = Sandbox::new();
        setup(1);
        let server = upstream(
            accept_all(),
            Box::new(|call, _| {
                let events = answer(&format!("resp_{}", call.nth), "a");
                if call.nth == 0 {
                    Step::After(Duration::from_millis(1500), events)
                } else {
                    Step::Events(events)
                }
            }),
        )
        .await;
        let mut client = connect().await;
        client.send(create(vec![user("first")])).await;
        for n in 0..18 {
            client.send(create(vec![user(&format!("q{n}"))])).await;
        }
        let (mut completed_turns, mut refused) = (0, 0);
        let started = std::time::Instant::now();
        while completed_turns < 17 {
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "turns: {completed_turns}"
            );
            match client.next().await {
                Some(reqwest_websocket::Message::Text(text)) => {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    match event["type"].as_str() {
                        Some("response.completed") => completed_turns += 1,
                        Some("error") => {
                            assert_eq!(event["error"]["code"], "too_many_queued_frames", "{event}");
                            refused += 1;
                        }
                        _ => {}
                    }
                }
                Some(_) => {}
                None => panic!("socket closed"),
            }
        }
        assert_eq!(refused, 2);
        assert_eq!(server.frames().len(), 17);
    }

    #[tokio::test]
    async fn ws_unknown_reference_not_found_is_not_failed_over() {
        let _sandbox = Sandbox::new();
        setup(2);
        let server = upstream(
            accept_all(),
            Box::new(|_, _| {
                Step::Events(vec![json!({"type":"error","error":{
                    "code":"previous_response_not_found",
                    "message":"Previous response with id 'resp_elsewhere' not found."}})])
            }),
        )
        .await;
        let mut client = connect().await;
        let events = client
            .turn(continue_from("resp_elsewhere", vec![user("u2")]))
            .await;
        let last = events.last().unwrap();
        assert_eq!(last["error"]["code"], "previous_response_not_found");
        assert_eq!(server.frames().len(), 1, "one account is tried");
        assert_eq!(server.handshakes().len(), 1);
    }
}
