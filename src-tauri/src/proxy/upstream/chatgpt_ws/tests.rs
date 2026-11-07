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
        Ping { account: String },
    }

    /// What one upstream frame gets back.
    enum Step {
        Events(Vec<Value>),
        /// Events, then the upstream closes the socket.
        Close(Vec<Value>),
    }

    /// `account` is the ChatGPT account id; `nth` counts earlier handshakes (or frames) of
    /// that account; `first` says whether it is the first account that connected.
    struct Call<'a> {
        account: &'a str,
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
                    Seen::Ping { .. } => false,
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
            account: &account,
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
                        account: &account,
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
                Message::Ping(_) => mock.seen.lock().unwrap().push(Seen::Ping {
                    account: account.clone(),
                }),
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
            ping_after: Duration::from_millis(200),
            pong_timeout: Duration::from_secs(5),
            idle: Duration::from_secs(300),
        }));
        let mut client = connect().await;
        client.turn(create(vec![user("u1")])).await;
        client.turn(continue_from("resp_0", vec![user("u2")])).await;
        assert!(
            !server.seen().iter().any(|s| matches!(s, Seen::Ping { .. })),
            "a busy socket is not pinged"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        client.turn(continue_from("resp_1", vec![user("u3")])).await;
        let seen = server.seen();
        let ping = seen
            .iter()
            .position(|s| matches!(s, Seen::Ping { .. }))
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
}
