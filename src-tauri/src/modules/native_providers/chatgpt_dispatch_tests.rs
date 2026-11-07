//! ChatGPT requests driven through the public middleware stack against a local mock.
use super::super::{accounts, chatgpt::endpoints, now, store, Account, Model, Provider, Realm};
use super::Sandbox;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const PUBLIC: &str = "chatgpt-chat";
const NATIVE: &str = "gpt-test";

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    content_type: &'static str,
    body: String,
}
fn sse(events: &[Value]) -> Reply {
    let body = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    Reply {
        status: 200,
        headers: Vec::new(),
        content_type: "text/event-stream",
        body,
    }
}
fn reply(status: u16, body: Value) -> Reply {
    Reply {
        status,
        headers: Vec::new(),
        content_type: "application/json",
        body: body.to_string(),
    }
}
fn with_header(mut r: Reply, key: &str, value: &str) -> Reply {
    r.headers.push((key.into(), value.into()));
    r
}

type Seen = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;
struct Upstream {
    seen: Seen,
    _reset: ResetEndpoints,
}
struct ResetEndpoints;
impl Drop for ResetEndpoints {
    fn drop(&mut self) {
        endpoints::set_test(None);
    }
}
impl Upstream {
    fn requests(&self) -> Vec<(String, HeaderMap, Value)> {
        self.seen.lock().unwrap().clone()
    }
}
/// Answers request N with `replies[N]`, repeating the last reply afterwards.
async fn upstream(replies: Vec<Reply>) -> Upstream {
    use axum::{body::Bytes, http::Uri, response::IntoResponse};
    let seen: Seen = Default::default();
    let log = seen.clone();
    let replies = Arc::new(replies);
    let app = Router::new().fallback(move |uri: Uri, h: HeaderMap, b: Bytes| {
        let (log, replies) = (log.clone(), replies.clone());
        async move {
            let index = {
                let mut log = log.lock().unwrap();
                log.push((
                    uri.path().to_string(),
                    h,
                    serde_json::from_slice(&b).unwrap_or(Value::Null),
                ));
                log.len() - 1
            };
            let r = replies[index.min(replies.len() - 1)].clone();
            let mut response = (
                StatusCode::from_u16(r.status).unwrap(),
                [("content-type", r.content_type)],
                r.body,
            )
                .into_response();
            for (k, v) in r.headers {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    axum::http::HeaderValue::from_str(&v).unwrap(),
                );
            }
            response
        }
    });
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
        seen,
        _reset: ResetEndpoints,
    }
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

fn publish(models: &[(&str, Provider, &str)]) {
    use crate::modules::model_registry as registry;
    let mapping = std::collections::HashMap::new();
    let view = registry::view(&[], &mapping).unwrap();
    registry::save(
        registry::SaveModelRegistryRequest {
            revision: view.revision,
            ignored_candidate_ids: vec![],
            allow_unverified: true,
            published_models: models
                .iter()
                .map(|(public, provider, native)| registry::PublishedModel {
                    id: format!("new:{public}"),
                    public_id: (*public).into(),
                    display_name: (*public).into(),
                    source_id: provider.source().into(),
                    native_model: (*native).into(),
                    extra_native_models: Vec::new(),
                    enabled: true,
                    legacy: false,
                })
                .collect(),
        },
        &[],
        &mapping,
    )
    .unwrap();
}

/// `count` ChatGPT accounts serving `gpt-test`, published as `chatgpt-chat`.
fn setup(count: usize) -> Vec<Account> {
    let accounts = (0..count).map(chatgpt_account).collect();
    publish(&[(PUBLIC, Provider::ChatGpt, NATIVE)]);
    accounts
}

fn app() -> Router {
    let monitor = Arc::new(crate::proxy::monitor::ProxyMonitor::disabled_for_test());
    Router::new()
        .fallback(|| async { StatusCode::IM_A_TEAPOT })
        .layer(axum::middleware::from_fn_with_state(
            crate::proxy::server::test_app_state(monitor),
            crate::proxy::upstream::native::route,
        ))
        .layer(axum::middleware::from_fn(
            crate::proxy::middleware::model_registry::model_registry_middleware,
        ))
}

async fn call(
    path: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::post(path).header("content-type", "application/json");
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = app()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn created() -> Value {
    json!({"type":"response.created","response":{"id":"resp_1","model":NATIVE,"status":"in_progress","output":[]}})
}
fn delta(text: &str) -> Value {
    json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":text})
}
fn completed(text: &str) -> Value {
    json!({"type":"response.completed","response":{"id":"resp_1","model":NATIVE,"status":"completed",
        "output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":text}]}],
        "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}})
}
fn hello() -> Reply {
    sse(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}),
        delta("Hel"),
        delta("lo"),
        completed("Hello"),
    ])
}
fn chat(stream: bool) -> Value {
    json!({"model":PUBLIC,"stream":stream,"max_tokens":16,
        "messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"hi"}]})
}
/// `(event name, data)` frames of an SSE body.
fn frames(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .map(|frame| {
            let mut name = String::new();
            let mut data = Value::Null;
            for line in frame.lines() {
                if let Some(v) = line.strip_prefix("event: ") {
                    name = v.to_string();
                } else if let Some(v) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(v).unwrap_or(json!(v));
                }
            }
            (name, data)
        })
        .collect()
}
fn account_of(headers: &HeaderMap) -> String {
    headers["chatgpt-account-id"].to_str().unwrap().to_string()
}
fn by_chatgpt_id(accounts: &[Account], id: &str) -> Account {
    accounts
        .iter()
        .find(|a| a.credentials.chatgpt_account_id == id)
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn chatgpt_chat_client_streams_and_bills() {
    let _sandbox = Sandbox::new();
    setup(1);
    let server = upstream(vec![hello()]).await;
    let (status, headers, body) = call("/v1/chat/completions", chat(true), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-upstream-provider"], "chatgpt");
    assert_eq!(headers["x-mapped-model"], NATIVE);
    assert!(headers["x-account-email"]
        .to_str()
        .unwrap()
        .ends_with("@chatgpt"));
    let text: String = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(String::from)
        })
        .collect();
    assert_eq!(text, "Hello");
    assert!(body.contains("[DONE]"));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let (path, _, upstream_body) = &requests[0];
    assert_eq!(path, "/backend-api/codex/responses");
    assert_eq!(upstream_body["store"], false);
    assert_eq!(upstream_body["stream"], true);
    assert_eq!(upstream_body["model"], NATIVE);
    assert!(upstream_body["instructions"]
        .as_str()
        .is_some_and(|s| s.contains("Be brief.")));
    for key in ["max_tokens", "max_output_tokens", "max_completion_tokens"] {
        assert!(upstream_body.get(key).is_none(), "{key}");
    }
}

#[tokio::test]
async fn chatgpt_messages_client_non_stream() {
    let _sandbox = Sandbox::new();
    setup(1);
    let server = upstream(vec![hello()]).await;
    let (status, _, body) = call(
        "/v1/messages",
        json!({"model":PUBLIC,"stream":false,"max_tokens":16,"messages":[{"role":"user","content":"hi"}]}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["type"], "message");
    assert_eq!(value["content"][0]["text"], "Hello");
    assert_eq!(server.requests()[0].2["stream"], true);
}

#[tokio::test]
async fn chatgpt_responses_passthrough_streams_raw_events() {
    let _sandbox = Sandbox::new();
    setup(1);
    let events = vec![
        created(),
        json!({"type":"response.in_progress","response":{"id":"resp_1","model":NATIVE,"status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}),
        delta("Hel"),
        delta("lo"),
        completed("Hello"),
    ];
    let server = upstream(vec![sse(&events)]).await;
    let request = json!({"model":PUBLIC,"stream":true,"input":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
        {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"enc-abc"},
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"earlier"}]},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"again"}]}
    ]});
    let (status, headers, body) = call("/v1/responses", request, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["content-type"], "text/event-stream");
    let received = frames(&body);
    let names: Vec<&str> = received.iter().map(|(n, _)| n.as_str()).collect();
    let expected: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(names, expected);
    for (name, data) in &received {
        assert_eq!(data["type"], name.as_str());
        if let Some(response) = data.get("response") {
            assert_eq!(response["model"], PUBLIC, "{name}");
        }
    }
    let input = &server.requests()[0].2["input"];
    let reasoning = input
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "reasoning")
        .unwrap();
    assert_eq!(reasoning["encrypted_content"], "enc-abc");
    assert!(reasoning.get("id").is_none());
}

#[tokio::test]
async fn chatgpt_responses_non_stream_aggregates() {
    let _sandbox = Sandbox::new();
    setup(1);
    let _server = upstream(vec![hello()]).await;
    let (status, _, body) = call(
        "/v1/responses",
        json!({"model":PUBLIC,"input":"hi","max_output_tokens":16}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["model"], PUBLIC);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(value["usage"]["input_tokens"], 3);
}

#[tokio::test]
async fn chatgpt_compact_returns_upstream_json() {
    let _sandbox = Sandbox::new();
    let account = chatgpt_account(0);
    let mut qoder = Account::new(Provider::Qoder, Realm::Cn);
    qoder.uid = "qoder-user".into();
    qoder.credentials.access_token = "qoder-access".into();
    qoder.expires_at = now() + 7200;
    qoder.models = vec![Model {
        id: "shared-model".into(),
        name: "Shared".into(),
        ..Default::default()
    }];
    store::upsert(qoder, None).unwrap();
    publish(&[
        (PUBLIC, Provider::ChatGpt, NATIVE),
        ("qoder-chat", Provider::Qoder, "shared-model"),
    ]);
    let compacted = json!({"id":"cmp_1","object":"response.compaction","model":NATIVE,
        "output":[{"type":"compaction","encrypted_content":"opaque"}],"usage":{"input_tokens":9,"output_tokens":1}});
    let server = upstream(vec![with_header(
        reply(200, compacted.clone()),
        "x-codex-turn-state",
        "turn-compact",
    )])
    .await;
    let (status, headers, body) = call(
        "/responses/compact",
        json!({"model":PUBLIC,"input":[{"type":"message","role":"user","content":"long history"}],"instructions":"Summarize.","stream":true,"store":true}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-codex-turn-state"], "turn-compact");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["model"], PUBLIC);
    assert_eq!(value["output"], compacted["output"]);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let (path, upstream_headers, upstream_body) = &requests[0];
    assert_eq!(path, "/backend-api/codex/responses/compact");
    assert_eq!(upstream_headers["accept"], "application/json");
    assert!(upstream_body.get("store").is_none());
    assert!(upstream_body.get("stream").is_none());
    assert_eq!(upstream_body["model"], NATIVE);
    // The unary exchange settles its lease as a success.
    assert_eq!(accounts::runtime(&account.id).successes, 1);
    // Other native sources keep refusing compaction before any account is used.
    let (status, _, _) = call(
        "/responses/compact",
        json!({"model":"qoder-chat","input":"long history"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn chatgpt_fails_over_429_before_output() {
    let _sandbox = Sandbox::new();
    let accounts = setup(2);
    let server = upstream(vec![
        reply(
            429,
            json!({"error":{"type":"usage_limit_reached","message":"The usage limit has been reached",
                "plan_type":"pro","resets_in_seconds":600}}),
        ),
        hello(),
    ])
    .await;
    let (status, _, body) = call("/v1/chat/completions", chat(true), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Hel"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let (first, second) = (account_of(&requests[0].1), account_of(&requests[1].1));
    assert_ne!(first, second);
    let limited = by_chatgpt_id(&accounts, &first);
    assert!(accounts::runtime(&limited.id).cooldown_until >= now() + 599);
    // The plan reported with the limit replaces the stored one.
    assert_eq!(
        store::get(Provider::ChatGpt, &limited.id)
            .unwrap()
            .credentials
            .user_type,
        "pro"
    );
}

#[tokio::test]
async fn chatgpt_fails_over_pre_output_overloaded_event() {
    let _sandbox = Sandbox::new();
    setup(2);
    let server = upstream(vec![
        sse(&[
            created(),
            json!({"type":"error","error":{"code":"server_is_overloaded","message":"Our servers are currently overloaded"}}),
        ]),
        hello(),
    ])
    .await;
    let (status, _, body) = call("/v1/chat/completions", chat(false), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["choices"][0]["message"]["content"], "Hello");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_ne!(account_of(&requests[0].1), account_of(&requests[1].1));
}

#[tokio::test]
async fn chatgpt_stops_after_three_accounts() {
    let _sandbox = Sandbox::new();
    setup(4);
    let server = upstream(vec![reply(
        503,
        json!({"error":{"message":"upstream unavailable"}}),
    )])
    .await;
    let (status, _, body) = call("/v1/chat/completions", chat(true), &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["error"]["code"], "native_upstream_failed");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let distinct: std::collections::HashSet<String> =
        requests.iter().map(|(_, h, _)| account_of(h)).collect();
    assert_eq!(distinct.len(), 3);
}

#[tokio::test]
async fn chatgpt_context_length_is_not_replayed() {
    let _sandbox = Sandbox::new();
    setup(2);
    let server = upstream(vec![reply(
        400,
        json!({"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window"}}),
    )])
    .await;
    let (status, _, body) = call("/v1/chat/completions", chat(true), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("context window"));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn chatgpt_post_output_failure_is_not_replayed() {
    let _sandbox = Sandbox::new();
    setup(2);
    let server = upstream(vec![
        sse(&[
            created(),
            delta("par"),
            json!({"type":"response.failed","response":{"id":"resp_1","model":NATIVE,"status":"failed",
                "error":{"code":"server_error","message":"boom"}}}),
        ]),
        hello(),
    ])
    .await;
    let (status, _, body) = call(
        "/v1/responses",
        json!({"model":PUBLIC,"stream":true,"input":"hi"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let received = frames(&body);
    assert!(received
        .iter()
        .any(|(n, d)| n == "response.output_text.delta" && d["delta"] == "par"));
    let (name, data) = received.last().unwrap();
    assert_eq!(name, "response.failed");
    assert_eq!(data["response"]["model"], PUBLIC);
    // The upstream's own failure is forwarded as-is, not replaced by a synthesized one.
    assert_eq!(data["response"]["error"]["code"], "server_error");
    assert_eq!(data["response"]["error"]["message"], "boom");
    assert_eq!(
        received
            .iter()
            .filter(|(n, _)| n == "response.failed")
            .count(),
        1
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn chatgpt_turn_state_header_returned() {
    let _sandbox = Sandbox::new();
    setup(1);
    let server = upstream(vec![with_header(hello(), "x-codex-turn-state", "turn-2")]).await;
    let (status, headers, body) = call(
        "/v1/responses",
        json!({"model":PUBLIC,"stream":true,"input":"hi","prompt_cache_key":"conversation-7"}),
        &[("x-codex-turn-state", "turn-1"), ("x-unrelated", "drop-me")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-codex-turn-state"], "turn-2");
    let requests = server.requests();
    let (_, upstream_headers, _) = &requests[0];
    assert_eq!(upstream_headers["x-codex-turn-state"], "turn-1");
    assert!(upstream_headers.get("x-unrelated").is_none());
    let session = crate::modules::native_providers::chatgpt::client::session_id(
        "administrator",
        "acc-0",
        "conversation-7",
    );
    assert_eq!(upstream_headers["session_id"], session.as_str());
}

#[tokio::test]
async fn chatgpt_probe_verifies_text() {
    let _sandbox = Sandbox::new();
    let accounts = setup(1);
    let server = upstream(vec![hello()]).await;
    let (verdict, _, account) = crate::proxy::upstream::native::probe(
        Provider::ChatGpt,
        NATIVE,
        None,
        "Reply briefly.",
        "Say hello",
    )
    .await
    .unwrap();
    assert_eq!(verdict, "verified");
    assert_eq!(account.as_deref(), Some(accounts[0].id.as_str()));
    assert!(store::get(Provider::ChatGpt, &accounts[0].id)
        .unwrap()
        .verified_models
        .contains_key(NATIVE));
    let (path, _, body) = &server.requests()[0];
    assert_eq!(path, "/backend-api/codex/responses");
    assert_eq!(body["instructions"], "Reply briefly.");
    assert_eq!(body["store"], false);
    assert_eq!(body["input"][0]["role"], "user");
}

#[tokio::test]
async fn chatgpt_unauthorized_after_refresh_retry_fails_over() {
    let _sandbox = Sandbox::new();
    setup(2);
    let unauthorized = reply(
        401,
        json!({"error":{"message":"Provided authentication token is expired."}}),
    );
    let server = upstream(vec![
        unauthorized.clone(),
        reply(
            200,
            json!({"access_token":"access-renewed","expires_in":3600}),
        ),
        unauthorized,
        hello(),
    ])
    .await;
    let (status, _, body) = call("/v1/chat/completions", chat(true), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Hel"));
    let requests = server.requests();
    let paths: Vec<&str> = requests.iter().map(|(p, _, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/backend-api/codex/responses",
            "/oauth/token",
            "/backend-api/codex/responses",
            "/backend-api/codex/responses",
        ]
    );
    // The same account retries once with the renewed token, then another account serves.
    assert_eq!(account_of(&requests[0].1), account_of(&requests[2].1));
    assert_eq!(requests[2].1["authorization"], "Bearer access-renewed");
    assert_ne!(account_of(&requests[2].1), account_of(&requests[3].1));
}

#[tokio::test]
async fn chatgpt_cloudflare_403_fails_over_without_refresh() {
    let _sandbox = Sandbox::new();
    let accounts = setup(2);
    let challenge = Reply {
        status: 403,
        headers: vec![("cf-mitigated".into(), "challenge".into())],
        content_type: "text/html",
        body: "<html><title>Just a moment...</title></html>".into(),
    };
    let server = upstream(vec![challenge, hello()]).await;
    let (status, _, body) = call("/v1/chat/completions", chat(true), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let requests = server.requests();
    let paths: Vec<&str> = requests.iter().map(|(p, _, _)| p.as_str()).collect();
    // A route challenge is not an account problem: no token refresh, no penalty.
    assert_eq!(
        paths,
        [
            "/backend-api/codex/responses",
            "/backend-api/codex/responses"
        ]
    );
    let (first, second) = (account_of(&requests[0].1), account_of(&requests[1].1));
    assert_ne!(first, second);
    let challenged = accounts::runtime(&by_chatgpt_id(&accounts, &first).id);
    assert_eq!(challenged.failures, 0);
    assert_eq!(challenged.cooldown_until, 0);
    assert!(challenged.last_error.is_none());
}

#[tokio::test]
async fn chatgpt_forbidden_json_fails_over_without_refresh() {
    let _sandbox = Sandbox::new();
    let accounts = setup(2);
    let server = upstream(vec![
        reply(
            403,
            json!({"error":{"message":"You do not have access to this resource."}}),
        ),
        hello(),
    ])
    .await;
    let (status, _, body) = call("/v1/chat/completions", chat(true), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Hel"));
    let requests = server.requests();
    let paths: Vec<&str> = requests.iter().map(|(p, _, _)| p.as_str()).collect();
    // Only a 401 earns a same-account token refresh; a 403 follows its classification.
    assert_eq!(
        paths,
        [
            "/backend-api/codex/responses",
            "/backend-api/codex/responses"
        ]
    );
    let (first, second) = (account_of(&requests[0].1), account_of(&requests[1].1));
    assert_ne!(first, second);
    let forbidden = accounts::runtime(&by_chatgpt_id(&accounts, &first).id);
    assert_eq!(forbidden.failures, 1);
}

#[tokio::test]
async fn chatgpt_responses_client_gets_upstream_error_before_output() {
    let _sandbox = Sandbox::new();
    setup(2);
    let error = json!({"type":"invalid_request_error","code":"context_length_exceeded",
        "message":"Your input exceeds the context window"});
    let server = upstream(vec![
        reply(400, json!({ "error": error })),
        sse(&[
            created(),
            json!({"type":"response.failed","response":{"id":"resp_1","model":NATIVE,
                "status":"failed","error":error}}),
        ]),
    ])
    .await;
    // Once as an HTTP answer, once as a stream event before output.
    for expected_requests in [1, 2] {
        let (status, _, body) = call(
            "/v1/responses",
            json!({"model":PUBLIC,"stream":true,"input":"hi"}),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["error"]["code"], "context_length_exceeded", "{body}");
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(
            value["error"]["message"],
            "Your input exceeds the context window"
        );
        assert_eq!(server.requests().len(), expected_requests);
    }
}

#[tokio::test]
async fn chatgpt_cancelled_stream_is_neutral() {
    let _sandbox = Sandbox::new();
    let accounts = setup(1);
    let _server = upstream(vec![sse(&[
        created(),
        delta("par"),
        json!({"type":"response.cancelled","response":{"id":"resp_1","model":NATIVE,"status":"cancelled"}}),
    ])])
    .await;
    let (status, _, body) = call(
        "/v1/responses",
        json!({"model":PUBLIC,"stream":true,"input":"hi"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let runtime = accounts::runtime(&accounts[0].id);
    assert_eq!(runtime.failures, 0);
    assert_eq!(runtime.cooldown_until, 0);
}
