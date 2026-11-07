use super::{
    auth, client, endpoints, errors, headers, identity, models,
    payload::{self, Mode},
    quota,
};
use crate::modules::native_providers::{
    http::{Failure, Penalty},
    store,
    tests::Sandbox,
    Account, Provider, Realm,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};

fn jwt(payload: Value) -> String {
    let part = |v: Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", part(json!({"alg": "none"})), part(payload))
}
fn map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.insert(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

#[test]
fn claims_prefer_id_token_and_fill_from_access_token() {
    let auth = |id: &str, plan: &str| {
        json!({"https://api.openai.com/auth": {
            "chatgpt_account_id": id, "chatgpt_user_id": "u1", "chatgpt_plan_type": plan}})
    };
    let mut id_claims = auth("acc", "plus");
    id_claims["email"] = json!("me@example.com");
    let mut access_claims = auth("other", "pro");
    access_claims["exp"] = json!(4_000_000_000i64);
    let mut a = Account::new(Provider::ChatGpt, Realm::Global);
    a.credentials.id_token = jwt(id_claims);
    a.credentials.access_token = jwt(access_claims);
    identity::apply(&mut a).unwrap();
    assert_eq!(a.uid, "acc:u1");
    assert_eq!(a.credentials.chatgpt_account_id, "acc");
    assert_eq!(a.credentials.user_type, "plus");
    assert_eq!(a.nickname, "me@example.com");
    assert_eq!(a.expires_at, 4_000_000_000);

    // The access token alone fills everything; `sub` backs up a missing user id.
    let mut b = Account::new(Provider::ChatGpt, Realm::Global);
    b.credentials.access_token = jwt(json!({"sub": "s9", "email": "x@y.z",
        "https://api.openai.com/auth": {"chatgpt_account_id": "acc2"}}));
    identity::apply(&mut b).unwrap();
    assert_eq!(b.uid, "acc2:s9");
    assert_eq!(b.nickname, "x@y.z");
    assert!(identity::claims("not-a-jwt").is_none());
}

#[test]
fn claims_without_account_id_is_import_error() {
    let mut a = Account::new(Provider::ChatGpt, Realm::Global);
    a.credentials.access_token = jwt(json!({"sub": "s"}));
    let err = identity::apply(&mut a).unwrap_err();
    assert_eq!(err.status, 400);
    assert!(err.message.contains("chatgpt_account_id"));
}

#[test]
fn client_version_respects_floor() {
    for (configured, expected) in [
        (Some("0.150.1"), "0.150.1"),
        (Some("0.144.0"), "0.144.0"),
        (Some("0.100.0"), "0.146.0"),
        (Some("garbage"), "0.146.0"),
        (Some("1.2"), "0.146.0"),
        (None, "0.146.0"),
    ] {
        assert_eq!(
            headers::pick_version(configured),
            expected,
            "{configured:?}"
        );
    }
    assert_eq!(headers::DEFAULT_CLIENT_VERSION, "0.146.0");
    std::env::set_var("ABV_CHATGPT_CLIENT_VERSION", "0.150.1");
    assert_eq!(headers::client_version(), "0.150.1");
    std::env::set_var("ABV_CHATGPT_CLIENT_VERSION", "0.100.0");
    assert_eq!(headers::client_version(), "0.146.0");
    std::env::remove_var("ABV_CHATGPT_CLIENT_VERSION");
}

#[test]
fn user_agent_is_canonical() {
    assert_eq!(
        headers::user_agent("0.146.0"),
        "codex_cli_rs/0.146.0 (Ubuntu 22.4.0; x86_64) xterm-256color"
    );
    let mut h = map(&[
        ("user-agent", "curl"),
        ("originator", "x"),
        ("version", "9"),
    ]);
    headers::apply_identity(&mut h, true);
    assert_eq!(h["originator"], "codex_cli_rs");
    assert_eq!(
        h["user-agent"],
        headers::user_agent(&headers::client_version()).as_str()
    );
    assert_eq!(h["version"], headers::client_version().as_str());
    headers::apply_identity(&mut h, false);
    assert!(h.get("version").is_none());
}

fn windows(p: &[(&str, &str)], s: &[(&str, &str)]) -> HeaderMap {
    let mut pairs = Vec::new();
    for (which, set) in [("primary", p), ("secondary", s)] {
        for (k, v) in set.iter() {
            pairs.push((format!("x-codex-{which}-{k}"), v.to_string()));
        }
    }
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    map(&refs)
}
fn names(q: &crate::modules::native_providers::Quota) -> Vec<&str> {
    q.packs.iter().map(|p| p.name.as_str()).collect()
}

#[test]
fn quota_windows_map_smaller_to_5h() {
    let at = 1000;
    let q = quota::from_headers(
        &windows(
            &[
                ("used-percent", "40"),
                ("window-minutes", "10080"),
                ("reset-after-seconds", "7000"),
            ],
            &[
                ("used-percent", "12.5"),
                ("window-minutes", "300"),
                ("reset-after-seconds", "60"),
            ],
        ),
        at,
    )
    .unwrap();
    assert_eq!(q.unit, "%");
    assert_eq!(names(&q), ["5h", "7d"]);
    let (short, long) = (&q.packs[0], &q.packs[1]);
    assert_eq!(
        (short.used, short.remaining, short.total),
        (Some(12.5), Some(87.5), Some(100.0))
    );
    assert_eq!(short.expires_at, Some(1060));
    assert_eq!((long.used, long.expires_at), (Some(40.0), Some(8000)));
    assert!(!short.unlimited);

    let one = quota::from_headers(
        &windows(&[("used-percent", "5"), ("window-minutes", "300")], &[]),
        at,
    )
    .unwrap();
    assert_eq!(names(&one), ["5h"]);
    let long_only = quota::from_headers(
        &windows(&[("used-percent", "5"), ("window-minutes", "10080")], &[]),
        at,
    )
    .unwrap();
    assert_eq!(names(&long_only), ["7d"]);

    let legacy = quota::from_headers(
        &windows(&[("used-percent", "70")], &[("used-percent", "20")]),
        at,
    )
    .unwrap();
    assert_eq!(names(&legacy), ["5h", "7d"]);
    assert_eq!(legacy.packs[0].used, Some(20.0));
    assert_eq!(legacy.packs[1].used, Some(70.0));
    assert!(quota::from_headers(&HeaderMap::new(), at).is_none());
}

#[test]
fn exhausted_until_from_body_or_full_window() {
    let at = 1000;
    let none = HeaderMap::new();
    let body = |e: Value| json!({"error": e});
    assert_eq!(
        quota::exhausted_until(
            &none,
            &body(json!({"type": "usage_limit_reached", "resets_in_seconds": "120"})),
            at
        ),
        Some(1120)
    );
    assert_eq!(
        quota::exhausted_until(
            &none,
            &body(json!({"resets_at": 5000, "resets_in_seconds": 10})),
            at
        ),
        Some(5000)
    );
    let full = windows(
        &[("used-percent", "100"), ("reset-after-seconds", "50")],
        &[],
    );
    assert_eq!(quota::exhausted_until(&full, &Value::Null, at), Some(1050));
    let almost = windows(
        &[("used-percent", "99"), ("reset-after-seconds", "50")],
        &[],
    );
    assert_eq!(quota::exhausted_until(&almost, &Value::Null, at), None);
    assert_eq!(
        quota::exhausted_until(&none, &body(json!({"message": "x"})), at),
        None
    );
}

fn prep(body: Value, mode: Mode) -> Value {
    payload::prepare(body, "gpt-test", mode).unwrap()
}

#[test]
fn payload_rules() {
    // Forced fields and model.
    let out = prep(
        json!({"model": "x", "stream": false, "store": true, "input": []}),
        Mode::Http,
    );
    assert_eq!(
        (out["stream"].clone(), out["store"].clone()),
        (json!(true), json!(false))
    );
    assert_eq!(out["model"], "gpt-test");
    let out = prep(
        json!({"stream": true, "store": false, "input": []}),
        Mode::Compact,
    );
    assert!(out.get("stream").is_none() && out.get("store").is_none());

    // Instructions.
    assert_eq!(
        prep(json!({"instructions": "  "}), Mode::Http)["instructions"],
        payload::DEFAULT_INSTRUCTIONS
    );
    assert_eq!(
        prep(json!({}), Mode::Http)["instructions"],
        payload::DEFAULT_INSTRUCTIONS
    );
    let out = prep(
        json!({"instructions": "base", "input": [
            {"type": "message", "role": "system", "content": "s1"},
            {"role": "system", "content": [{"type": "input_text", "text": "s2"}]},
            {"type": "message", "role": "user", "content": "hi"}]}),
        Mode::Http,
    );
    assert_eq!(out["instructions"], "s1\n\ns2\n\nbase");
    assert_eq!(out["input"].as_array().unwrap().len(), 1);

    // String input.
    assert_eq!(
        prep(json!({"input": "hello"}), Mode::Http)["input"],
        json!([{"type": "message", "role": "user", "content": "hello"}])
    );
    assert_eq!(prep(json!({"input": ""}), Mode::Http)["input"], json!([]));

    // Reasoning and include.
    let out = prep(
        json!({"reasoning": {"effort": "minimal"}, "include": ["x"]}),
        Mode::Http,
    );
    assert_eq!(out["reasoning"]["effort"], "none");
    assert_eq!(out["include"], json!(["x", "reasoning.encrypted_content"]));
    assert!(prep(json!({}), Mode::Http).get("include").is_none());
    let twice = prep(
        json!({"reasoning": {}, "include": ["reasoning.encrypted_content"]}),
        Mode::Http,
    );
    assert_eq!(twice["include"].as_array().unwrap().len(), 1);

    // Removed fields.
    let mut noisy = json!({"previous_response_id": "r", "input": []});
    for key in [
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "user",
        "metadata",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
        "truncation",
        "stop_sequences",
        "background",
    ] {
        noisy[key] = json!(1);
    }
    for mode in [Mode::Http, Mode::Compact] {
        let out = prep(noisy.clone(), mode);
        assert_eq!(
            out.as_object()
                .unwrap()
                .keys()
                .filter(|k| !matches!(
                    k.as_str(),
                    "input" | "model" | "instructions" | "stream" | "store"
                ))
                .count(),
            0,
            "{mode:?}"
        );
    }
    let ws = prep(noisy.clone(), Mode::WebSocket);
    assert_eq!(ws["previous_response_id"], "r");
    assert!(ws.get("background").is_none() && ws.get("temperature").is_none());
}

#[test]
fn payload_input_items() {
    let long = "c".repeat(70);
    let out = prep(
        json!({"input": [
            {"type": "reasoning", "id": "rs_1", "encrypted_content": "enc"},
            {"type": "message", "id": "msg_1", "role": "assistant", "content": "a"},
            {"type": "function_call", "id": "fc1", "call_id": long, "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": long, "output": "ok"},
            {"type": "function_call", "call_id": "short", "name": "g", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "short", "output": "ok"},
            {"type": "function_call_output", "call_id": "orphan", "output": "x"}]}),
        Mode::Http,
    );
    let items = out["input"].as_array().unwrap();
    assert_eq!(items.len(), 6, "orphan output removed");
    assert!(items.iter().all(|i| i.get("id").is_none()));
    assert_eq!(items[0]["encrypted_content"], "enc");
    assert_eq!(items[0]["summary"], json!([]));
    let call = items[2]["call_id"].as_str().unwrap();
    assert_eq!(call.len(), 64);
    assert!(call.starts_with("call_") && call[5..].chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(items[3]["call_id"], call);
    assert_eq!(items[4]["call_id"], "short");
}

#[test]
fn payload_tools() {
    let out = prep(
        json!({"tools": [
            {"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}, "strict": true}},
            {"type": "function", "name": "flat", "parameters": {}}]}),
        Mode::Http,
    );
    assert_eq!(
        out["tools"][0],
        json!({"type": "function", "name": "f", "description": "d", "parameters": {"type": "object"}, "strict": true})
    );
    assert_eq!(out["tools"][1]["name"], "flat");

    let replay = json!([{"type": "web_search_call", "status": "completed"}]);
    let out = prep(json!({"input": replay}), Mode::Http);
    assert_eq!(
        out["tools"],
        json!([{"type": "web_search", "external_web_access": false}])
    );
    assert_eq!(out["tool_choice"], "none");
    let out = prep(
        json!({"input": replay, "tools": [{"type": "function", "name": "f"}]}),
        Mode::Http,
    );
    assert_eq!(out["tools"].as_array().unwrap().len(), 2);
    assert!(out.get("tool_choice").is_none());
    let out = prep(
        json!({"input": replay, "tools": [{"type": "web_search"}]}),
        Mode::Http,
    );
    assert_eq!(out["tools"].as_array().unwrap().len(), 1);
    assert!(payload::prepare(json!([]), "m", Mode::Http).is_err());
}

fn classify(status: u16, headers: &[(&str, &str)], body: &str) -> Failure {
    errors::classify(status, &map(headers), body, 1000)
}

#[test]
fn classify_table() {
    let usage = r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":120}}"#;
    let f = classify(429, &[], usage);
    assert!(matches!(
        f.penalty,
        Some(Penalty::Until {
            at: 1120,
            message: "Account quota exhausted"
        })
    ));
    assert!(f.failover && f.status == 429);
    let f = classify(
        429,
        &[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-reset-after-seconds", "50"),
        ],
        "{}",
    );
    assert!(matches!(f.penalty, Some(Penalty::Until { at: 1050, .. })));
    let f = classify(429, &[], "{}");
    assert!(
        matches!(
            f.penalty,
            Some(Penalty::Breaker {
                retry_after: Some(5)
            })
        ) && f.failover
    );
    let f = classify(
        429,
        &[("retry-after", "7")],
        r#"{"error":{"type":"rate_limit_exceeded"}}"#,
    );
    assert!(matches!(
        f.penalty,
        Some(Penalty::Breaker {
            retry_after: Some(7)
        })
    ));

    let f = classify(402, &[], r#"{"detail":{"code":"deactivated_workspace"}}"#);
    assert!(matches!(f.penalty, Some(Penalty::Disable { .. })) && f.failover);
    assert!(matches!(
        classify(402, &[], "").penalty,
        Some(Penalty::Disable { .. })
    ));
    let f = classify(400, &[], r#"{"error":{"code":"workspace_deactivated"}}"#);
    assert!(matches!(f.penalty, Some(Penalty::Disable { .. })) && f.failover);

    let f = classify(401, &[], r#"{"error":{"code":"token_revoked"}}"#);
    assert!(matches!(f.penalty, Some(Penalty::Disable { .. })) && f.failover);
    let f = classify(401, &[], r#"{"detail":"Unauthorized"}"#);
    assert!(matches!(f.penalty, Some(Penalty::Disable { .. })));
    let f = classify(401, &[], r#"{"error":{"message":"expired"}}"#);
    assert!(matches!(f.penalty, Some(Penalty::Breaker { .. })) && f.failover && f.status == 401);

    for f in [
        classify(403, &[("cf-mitigated", "challenge")], ""),
        classify(403, &[], "<html><title>Just a moment...</title></html>"),
    ] {
        assert!(matches!(f.penalty, Some(Penalty::Neutral)) && f.failover && f.status == 502);
    }
    let f = classify(403, &[], r#"{"error":{"message":"forbidden"}}"#);
    assert!(matches!(f.penalty, Some(Penalty::Breaker { .. })) && f.failover && f.status == 403);

    for status in [500, 503, 529] {
        let f = classify(status, &[], "boom");
        assert!(matches!(f.penalty, Some(Penalty::Neutral)) && f.failover && f.status == status);
    }
    let f = classify(405, &[], "");
    assert!(f.failover && matches!(f.penalty, Some(Penalty::Neutral)) && f.status == 405);
    for body in [
        r#"{"error":{"message":"An error occurred while processing your request. Please try again."}}"#,
        r#"{"error":{"message":"You can retry your request, or contact us through help.openai.com. request id abc"}}"#,
    ] {
        let f = classify(400, &[], body);
        assert!(matches!(f.penalty, Some(Penalty::Neutral)) && f.failover && f.status == 503);
    }
    for body in [
        r#"{"error":{"code":"model_not_found","message":"x"}}"#,
        r#"{"error":{"message":"The model `gpt-9` does not exist or you do not have access"}}"#,
    ] {
        let f = classify(400, &[], body);
        assert!(matches!(f.penalty, Some(Penalty::Neutral)) && f.failover && f.status == 400);
    }
    for body in [
        r#"{"error":{"message":"This model's maximum context length is 1 tokens"}}"#,
        r#"{"error":{"type":"invalid_request_error","code":"whatever","message":"bad"}}"#,
        r#"{"error":{"code":"content_policy_violation","message":"no"}}"#,
    ] {
        let f = classify(400, &[], body);
        assert!(
            matches!(f.penalty, Some(Penalty::Neutral)) && !f.failover && f.status == 400,
            "{body}"
        );
    }
    for body in [
        r#"{"error":{"code":"server_is_overloaded"}}"#,
        r#"{"error":{"message":"The selected model is at capacity"}}"#,
    ] {
        let f = classify(400, &[], body);
        assert!(matches!(f.penalty, Some(Penalty::Neutral)) && f.failover && f.status == 503);
    }
    let f = classify(
        400,
        &[],
        r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#,
    );
    assert!(matches!(f.penalty, Some(Penalty::Neutral)) && !f.failover && f.status == 400);
    assert_eq!(f.message, "too long");
    let f = classify(400, &[], r#"{"error":{"message":"bad field"}}"#);
    assert!(matches!(f.penalty, Some(Penalty::Neutral)) && !f.failover && f.status == 400);
}

#[test]
fn classify_event_maps_stream_errors() {
    let f = errors::classify_event(
        &json!({"type": "error", "error": {"code": "server_is_overloaded"}}),
        1000,
    );
    assert!(f.failover && matches!(f.penalty, Some(Penalty::Neutral)) && f.status == 503);
    let f = errors::classify_event(
        &json!({"type": "response.failed", "response": {"error": {"code": "context_length_exceeded"}}}),
        1000,
    );
    assert!(!f.failover && f.status == 400);
    let f = errors::classify_event(
        &json!({"type": "error", "error": {"type": "usage_limit_reached", "resets_in_seconds": 30}}),
        1000,
    );
    assert!(matches!(f.penalty, Some(Penalty::Until { at: 1030, .. })));
    // Fallback: unknown server-looking errors fail over; request faults do not.
    let f = errors::classify_event(
        &json!({"type": "error", "error": {"code": "server_error"}}),
        1000,
    );
    assert!(f.failover && f.status == 502);
    let f = errors::classify_event(
        &json!({"type": "error", "error": {"code": "mystery"}}),
        1000,
    );
    assert!(f.failover && f.status == 502);
    let f = errors::classify_event(
        &json!({"type": "error", "error": {"type": "invalid_request_error", "code": "mystery"}}),
        1000,
    );
    assert!(!f.failover && f.status == 400 && matches!(f.penalty, Some(Penalty::Neutral)));
}

#[test]
fn websocket_incremental_turn_keeps_tool_outputs() {
    let input = json!([
        {"type": "function_call_output", "call_id": "c1", "output": "ok"},
        {"type": "item_reference", "id": "rs_1"}]);
    let out = prep(
        json!({"previous_response_id": "r", "input": input.clone()}),
        Mode::WebSocket,
    );
    assert_eq!(out["input"], input);
    // Without a previous response the orphan is still dropped.
    let out = prep(json!({"input": input}), Mode::WebSocket);
    assert_eq!(out["input"].as_array().unwrap().len(), 1);
}

// ---- network client, against a local mock ----

type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;

struct Mock {
    seen: Seen,
    _reset: ResetEndpoints,
}
struct ResetEndpoints;
impl Drop for ResetEndpoints {
    fn drop(&mut self) {
        endpoints::set_test(None);
    }
}

/// Serves `status`/`headers`/`body` for every path; `echo` answers with the request instead.
async fn mock(status: u16, extra: &[(&str, &str)], body: Value, echo: bool) -> Mock {
    use axum::{body::Bytes, http::Uri, response::IntoResponse, Router};
    let seen: Seen = Default::default();
    let log = seen.clone();
    let extra: Vec<(String, String)> = extra
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let app = Router::new().fallback(move |uri: Uri, h: HeaderMap, b: Bytes| {
        let (log, extra, body) = (log.clone(), extra.clone(), body.clone());
        async move {
            let target = uri
                .path_and_query()
                .map(|p| p.to_string())
                .unwrap_or_default();
            log.lock()
                .unwrap()
                .push((target.clone(), h.clone(), b.to_vec()));
            let payload = if echo {
                let map: serde_json::Map<String, Value> = h
                    .iter()
                    .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or_default())))
                    .collect();
                json!({"path": target, "headers": map})
            } else {
                body
            };
            let mut response = (
                axum::http::StatusCode::from_u16(status).unwrap(),
                [("content-type", "application/json")],
                payload.to_string(),
            )
                .into_response();
            for (k, v) in extra {
                response.headers_mut().insert(
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(&v).unwrap(),
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
    Mock {
        seen,
        _reset: ResetEndpoints,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn chatgpt_account() -> Account {
    let mut a = Account::new(Provider::ChatGpt, Realm::Global);
    a.credentials.access_token = jwt(json!({"https://api.openai.com/auth": {
        "chatgpt_account_id": "acc-1", "chatgpt_user_id": "u1"}}));
    a.credentials.refresh_token = "r1".into();
    a.credentials.chatgpt_account_id = "acc-1".into();
    a.uid = "acc-1:u1".into();
    a
}

fn token_response() -> Value {
    let auth = json!({"chatgpt_account_id": "acc-1", "chatgpt_user_id": "u1",
        "chatgpt_plan_type": "plus"});
    json!({
        "access_token": jwt(json!({"exp": 4_000_000_000i64, "https://api.openai.com/auth": auth})),
        "id_token": jwt(json!({"email": "me@example.com", "https://api.openai.com/auth": auth})),
        "expires_in": 3600,
        "refresh_token": "r2",
    })
}

#[tokio::test]
async fn refresh_rotates_and_parses_identity() {
    let _sandbox = Sandbox::new();
    let server = mock(200, &[], token_response(), false).await;
    let mut a = chatgpt_account();
    auth::refresh(&http(), &mut a).await.unwrap();
    assert_eq!(a.credentials.refresh_token, "r2");
    assert_eq!(a.uid, "acc-1:u1");
    assert_eq!(a.credentials.user_type, "plus");
    assert_eq!(a.nickname, "me@example.com");
    let drift = (a.expires_at - (crate::modules::native_providers::now() + 3600)).abs();
    assert!(drift <= 5, "expires_at drift {drift}");
    let seen = server.seen.lock().unwrap();
    let (path, h, body) = &seen[0];
    assert_eq!(path, "/oauth/token");
    assert!(h.contains_key("originator") && !h.contains_key("version"));
    assert_eq!(h["content-type"], "application/x-www-form-urlencoded");
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body).into_owned().collect();
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "r1");
    assert_eq!(form["client_id"], auth::CLIENT_ID);
    assert_eq!(form["scope"], "openid profile email");
}

#[tokio::test]
async fn refresh_without_refresh_token_keeps_old_one() {
    let _sandbox = Sandbox::new();
    let mut body = token_response();
    body.as_object_mut().unwrap().remove("refresh_token");
    let _server = mock(200, &[], body.clone(), false).await;
    let mut a = chatgpt_account();
    auth::refresh(&http(), &mut a).await.unwrap();
    assert_eq!(a.credentials.refresh_token, "r1");
    body["refresh_token"] = json!("");
    let _server = mock(200, &[], body, false).await;
    auth::refresh(&http(), &mut a).await.unwrap();
    assert_eq!(a.credentials.refresh_token, "r1");
    assert_eq!(a.credentials.access_token, token_response()["access_token"]);
}

#[tokio::test]
async fn refresh_invalid_grant_disables() {
    let _sandbox = Sandbox::new();
    let _server = mock(
        400,
        &[],
        json!({"error": "invalid_grant", "error_description": "Refresh token reused"}),
        false,
    )
    .await;
    let mut a = chatgpt_account();
    let before = a.credentials.access_token.clone();
    let err = auth::refresh(&http(), &mut a).await.unwrap_err();
    assert_eq!(err.status, 401);
    assert!(matches!(err.penalty, Some(Penalty::Disable { .. })));
    assert!(!err.failover);
    assert_eq!(a.credentials.access_token, before);
    assert_eq!(a.credentials.refresh_token, "r1");
}

#[tokio::test]
async fn refresh_invalid_client_is_config_error() {
    let _sandbox = Sandbox::new();
    let _server = mock(
        401,
        &[],
        json!({"error": {"code": "invalid_client"}}),
        false,
    )
    .await;
    let mut a = chatgpt_account();
    let err = auth::refresh(&http(), &mut a).await.unwrap_err();
    assert_eq!(err.status, 503);
    assert!(err.penalty.is_none());
}

#[tokio::test]
async fn refresh_without_token_is_dead_credential() {
    let _sandbox = Sandbox::new();
    let mut a = chatgpt_account();
    a.credentials.refresh_token.clear();
    let err = auth::refresh(&http(), &mut a).await.unwrap_err();
    assert!(matches!(err.penalty, Some(Penalty::Disable { .. })));
}

#[tokio::test]
async fn models_skip_hidden_and_wildcard() {
    let _sandbox = Sandbox::new();
    let server = mock(
        200,
        &[],
        json!({"models": [
            {"slug": "gpt-a", "display_name": "GPT A", "context_window": 272000,
             "max_context_window": 1000000, "input_modalities": ["text", "image"],
             "supported_reasoning_levels": [{"effort": "low"}], "visibility": "list",
             "supported_in_api": true},
            {"slug": "gpt-b", "max_context_window": 5000, "input_modalities": ["text"],
             "supported_reasoning_levels": []},
            {"slug": "hidden", "visibility": "hide"},
            {"slug": "gpt-*"},
            {"slug": "not-api", "supported_in_api": false}]}),
        false,
    )
    .await;
    let list = models::fetch(&http(), &chatgpt_account()).await.unwrap();
    let ids: Vec<_> = list.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gpt-a", "gpt-b"]);
    assert_eq!(list[0].name, "GPT A");
    assert_eq!(list[0].context_window, 272000);
    assert!(list[0].vision && list[0].reasoning);
    assert_eq!(list[1].name, "gpt-b");
    assert_eq!(list[1].context_window, 5000);
    assert!(!list[1].vision && !list[1].reasoning);
    let seen = server.seen.lock().unwrap();
    let version = headers::client_version();
    assert_eq!(
        seen[0].0,
        format!("/backend-api/codex/models?client_version={version}")
    );
    assert_eq!(seen[0].1["version"], version.as_str());
    assert_eq!(seen[0].1["chatgpt-account-id"], "acc-1");
}

#[tokio::test]
async fn models_failure_is_classified() {
    let _sandbox = Sandbox::new();
    let _server = mock(402, &[], json!({"detail": "payment required"}), false).await;
    let err = models::fetch(&http(), &chatgpt_account())
        .await
        .err()
        .unwrap();
    assert_eq!(err.status, 402);
    assert!(matches!(err.penalty, Some(Penalty::Disable { .. })));
}

#[tokio::test]
async fn send_sets_headers_and_filters_forwarded() {
    let _sandbox = Sandbox::new();
    let server = mock(200, &[], Value::Null, true).await;
    let a = chatgpt_account();
    let forwarded = map(&[
        ("authorization", "Bearer evil"),
        ("cookie", "c=1"),
        ("x-forwarded-for", "1.2.3.4"),
        ("user-agent", "evil/1"),
        ("originator", "evil"),
        ("chatgpt-account-id", "other"),
        ("x-codex-turn-state", "ts1"),
        ("accept-language", "de"),
    ]);
    let session = client::session_id("scope", &a.id, "key");
    let response = client::send(
        &http(),
        &a,
        json!({"input": []}),
        client::Endpoint::Responses,
        &session,
        &forwarded,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let echoed: Value = response.json().await.unwrap();
    let h = &echoed["headers"];
    assert_eq!(echoed["path"], "/backend-api/codex/responses");
    assert_eq!(
        h["authorization"],
        format!("Bearer {}", a.credentials.access_token)
    );
    assert_eq!(h["chatgpt-account-id"], "acc-1");
    assert_eq!(h["originator"], "codex_cli_rs");
    assert!(h["user-agent"]
        .as_str()
        .unwrap()
        .starts_with("codex_cli_rs/"));
    assert_eq!(h["version"], headers::client_version().as_str());
    assert_eq!(h["accept"], "text/event-stream");
    assert_eq!(h["session_id"], session.as_str());
    assert_eq!(h["conversation_id"], session.as_str());
    assert_eq!(h["x-codex-beta-features"], "remote_compaction_v2");
    assert_eq!(h["x-codex-turn-state"], "ts1");
    assert_eq!(h["accept-language"], "de");
    assert!(h.get("cookie").is_none() && h.get("x-forwarded-for").is_none());
    assert_eq!(server.seen.lock().unwrap().len(), 1);

    let compact = client::send(
        &http(),
        &a,
        json!({}),
        client::Endpoint::Compact,
        &session,
        &HeaderMap::new(),
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let echoed: Value = compact.json().await.unwrap();
    assert_eq!(echoed["path"], "/backend-api/codex/responses/compact");
    assert_eq!(echoed["headers"]["accept"], "application/json");
}

#[test]
fn session_id_is_a_stable_scoped_uuid() {
    let a = client::session_id("s", "acc", "k");
    assert_eq!(a, client::session_id("s", "acc", "k"));
    assert_ne!(a, client::session_id("s", "acc2", "k"));
    assert_ne!(a, client::session_id("s2", "acc", "k"));
    assert!(uuid::Uuid::parse_str(&a).is_ok());
}

#[tokio::test]
async fn send_records_quota_headers() {
    let _sandbox = Sandbox::new();
    let _server = mock(
        200,
        &[
            ("x-codex-primary-used-percent", "10"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-secondary-used-percent", "40"),
            ("x-codex-secondary-window-minutes", "10080"),
        ],
        json!({}),
        false,
    )
    .await;
    let a = store::upsert(chatgpt_account(), None).unwrap();
    client::send(
        &http(),
        &a,
        json!({}),
        client::Endpoint::Responses,
        "s",
        &HeaderMap::new(),
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let quota = store::get(Provider::ChatGpt, &a.id).unwrap().quota.unwrap();
    let names: Vec<_> = quota.packs.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["5h", "7d"]);
    assert_eq!(quota.packs[0].used, Some(10.0));
}

#[test]
fn quota_record_is_throttled_unless_a_window_is_full() {
    let _sandbox = Sandbox::new();
    let a = store::upsert(chatgpt_account(), None).unwrap();
    let used = |n: &str| map(&[("x-codex-primary-used-percent", n)]);
    let stored = || {
        store::get(Provider::ChatGpt, &a.id)
            .unwrap()
            .quota
            .unwrap()
            .packs[0]
            .used
            .unwrap()
    };
    quota::record(&a, &used("10"));
    quota::record(&a, &used("20"));
    assert_eq!(stored(), 10.0);
    quota::record(&a, &used("100"));
    assert_eq!(stored(), 100.0);
}

#[tokio::test]
async fn url_rejects_foreign_hosts() {
    let _sandbox = Sandbox::new();
    for (base, path) in [
        ("https://evil.com", "/x"),
        ("http://chatgpt.com", "/x"),
        ("https://chatgpt.com.evil.com", "/x"),
        ("http://127.0.0.1:9", "/x"),
    ] {
        assert_eq!(
            endpoints::url(base, path).unwrap_err().status,
            400,
            "{base}"
        );
    }
    let ok = endpoints::url("https://chatgpt.com/backend-api", "/codex/models").unwrap();
    assert_eq!(ok.as_str(), "https://chatgpt.com/backend-api/codex/models");
    assert!(endpoints::url("https://auth.openai.com", "/oauth/token").is_ok());
    let _server = mock(200, &[], json!({}), false).await;
    assert!(endpoints::url(&endpoints::current().auth, "/oauth/token").is_ok());
    assert_eq!(
        endpoints::url("http://127.0.0.1:9", "/x").unwrap().port(),
        Some(9)
    );
    assert!(endpoints::url("http://evil.com", "/x").is_err());
}
