use super::*;
use axum::{routing::post, Router};
use std::{collections::BTreeMap, time::Duration};
use tower::ServiceExt;

struct NativeMock {
    task: tokio::task::JoinHandle<()>,
}
impl Drop for NativeMock {
    fn drop(&mut self) {
        self.task.abort();
        std::env::remove_var("STELLAR_TEST_WORKBUDDY_BASE");
    }
}
fn settings() -> Settings {
    Settings {
        enabled: true,
        models: BTreeMap::from([("wb-sonnet".into(), "claude-sonnet-4-6".into())]),
        ..Default::default()
    }
}
fn seed() -> String {
    let mut account = workbuddy::accounts::parse_import(json!({"uid":"mock-uid","nickname":"Mock account","accessToken":"native_secret","refreshToken":"refresh_secret","expiresAt":workbuddy::accounts::now()+3600})).unwrap();
    account.models.push(workbuddy::accounts::Model {
        id: "claude-sonnet-4-6".into(),
        name: "Sonnet".into(),
    });
    workbuddy::accounts::upsert(account).unwrap().id
}
async fn mock() -> NativeMock {
    serve(Router::new().route("/v2/chat/completions", post(upstream))).await
}
async fn serve(router: Router) -> NativeMock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STELLAR_TEST_WORKBUDDY_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    NativeMock { task }
}
async fn upstream(headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    assert_eq!(headers[header::AUTHORIZATION], "Bearer native_secret");
    assert_eq!(headers["x-user-id"], "mock-uid");
    for name in ["cookie", "x-api-key", "x-forwarded-for", "x-refresh-token"] {
        assert!(!headers.contains_key(name));
    }
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["messages"][0]["role"], "system");
    let user_content = &body["messages"][1]["content"];
    if user_content == "reject-before-generation" {
        return Response::builder().header(header::CONTENT_TYPE,"text/event-stream")
            .body(Body::from("data: {\"error\":{\"message\":\"upstream credential diagnostic must not leak\"}}\n\n")).unwrap();
    }
    if let Some(mode @ ("reject-after-generation" | "incomplete-after-generation")) =
        user_content.as_str()
    {
        let reject = mode == "reject-after-generation";
        let stream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(bytes::Bytes::from(format!("data: {}\n\ndata: {}\n\n",
                json!({"choices":[{"delta":{"content":"partial"}}]}),
                json!({"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20,"prompt_cache_hit_tokens":600}}))));
            // Commit the HTTP response before the upstream fails.
            tokio::time::sleep(Duration::from_millis(50)).await;
            if reject {
                yield Ok(bytes::Bytes::from_static(b"data: {\"error\":{\"message\":\"upstream credential diagnostic must not leak\"}}\n\n"));
            }
        };
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    assert_eq!(user_content, "hello");
    let events = format!(
        "data: {}\r\n\r\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"你好"},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20,"prompt_cache_hit_tokens":600,"credit":999}})
    );
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = events
        .as_bytes()
        .chunks(7)
        .map(|part| Ok(bytes::Bytes::copy_from_slice(part)))
        .collect();
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap()
}
fn request(path: &str, model: &str, stream: bool) -> Request {
    let mut payload = if path == "/v1/responses" || path == "/responses" {
        json!({"input":"hello"})
    } else {
        json!({"messages":[{"role":"user","content":"hello"}],"max_tokens":1024})
    };
    payload["model"] = json!(model);
    payload["stream"] = json!(stream);
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer downstream_secret")
        .header("cookie", "private-cookie")
        .body(Body::from(payload.to_string()))
        .unwrap()
}
async fn middleware(State(settings): State<Settings>, request: Request, next: Next) -> Response {
    dispatch(
        settings,
        Default::default(),
        5,
        Policy::default(),
        request,
        next,
    )
    .await
}
fn app(settings: Settings) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(|| async { "google path" }))
        .route("/v1/responses", post(|| async { "google path" }))
        .route("/responses", post(|| async { "google path" }))
        .route("/v1/messages", post(|| async { "google path" }))
        .route("/v1/images/generations", post(|| async { "google path" }))
        .layer(axum::middleware::from_fn_with_state(settings, middleware))
}
#[tokio::test]
async fn workbuddy_native_stream_to_three_protocols_and_pool_release() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let _mock = mock().await;
    let id = seed();
    for path in [
        "/v1/chat/completions",
        "/v1/responses",
        "/responses",
        "/v1/messages",
    ] {
        for stream in [false, true] {
            let response = app(settings())
                .oneshot(request(path, "wb-sonnet", stream))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-upstream-provider"], "workbuddy");
            let data = to_bytes(response.into_body(), 100_000).await.unwrap();
            let data = String::from_utf8(data.to_vec()).unwrap();
            assert!(data.contains("你好"));
            assert!(data.contains("wb-sonnet"));
            assert!(!data.contains("credit"));
            if !stream {
                let value: Value = serde_json::from_str(&data).unwrap();
                let usage =
                    crate::proxy::pipeline::usage::CanonicalUsage::from_gemini(&value["usage"]);
                assert_eq!(usage.total_input_tokens, 1000);
                assert_eq!(usage.cached_tokens, 600);
                assert_eq!(usage.output_tokens, 20);
            }
        }
    }
    let observed = workbuddy::accounts::list().unwrap();
    assert_eq!(observed[0].in_flight, 0);
    assert_eq!(observed[0].health.successes, 8);
    assert_eq!(observed[0].health.failures, 0);
    workbuddy::accounts::set_enabled(&id, false).unwrap();
    let unavailable = app(settings())
        .oneshot(request("/v1/chat/completions", "wb-sonnet", false))
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    let google = app(settings())
        .oneshot(request("/v1/chat/completions", "gemini-flash", false))
        .await
        .unwrap();
    assert_eq!(
        to_bytes(google.into_body(), 100).await.unwrap(),
        "google path"
    );
    let unsupported = app(settings())
        .oneshot(request("/v1/images/generations", "wb-sonnet", false))
        .await
        .unwrap();
    assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
    let mut disabled = settings();
    disabled.enabled = false;
    assert_eq!(
        app(disabled)
            .oneshot(request("/v1/chat/completions", "wb-sonnet", false))
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}
#[test]
fn workbuddy_codec_preserves_tools_images_and_rejects_stateful_loss() {
    use codec::{request as convert, Protocol};
    let converted = convert(Protocol::Responses,json!({"input":[{"role":"user","content":[{"type":"input_text","text":"look"},{"type":"input_image","image_url":"https://example.com/a.png"}]},{"type":"function_call","call_id":"call_1","name":"weather","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"rain"}],"tools":[{"type":"function","name":"weather","parameters":{"type":"object"}}]})).unwrap();
    assert_eq!(
        converted["messages"][0]["content"][1]["image_url"]["url"],
        "https://example.com/a.png"
    );
    assert_eq!(converted["messages"][2]["tool_call_id"], "call_1");
    assert_eq!(converted["tools"][0]["function"]["name"], "weather");
    assert!(convert(
        Protocol::Responses,
        json!({"input":"hello","previous_response_id":"resp_other_user"})
    )
    .is_err());
    // A forced Chat tool without a name is a request error, not an account failure.
    assert!(convert(
        Protocol::Chat,
        json!({"messages":[{"role":"user","content":"hi"}],"tool_choice":{"type":"function"}})
    )
    .is_err());
    let converted = convert(Protocol::Responses,json!({"input":[{"role":"user","content":"hi"},{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"q"}}],"tools":[{"type":"web_search","external_web_access":false},{"type":"web_search_preview"}],"tool_choice":{"type":"web_search"}})).unwrap();
    assert!(converted.get("tools").is_none());
    assert_eq!(converted["tool_choice"], "auto");
    assert_eq!(converted["messages"].as_array().unwrap().len(), 1);
    assert!(convert(
        Protocol::Responses,
        json!({"input":"hello","tools":[{"type":"file_search"}]})
    )
    .is_err());
    let converted = convert(Protocol::Responses,json!({"input":[{"type":"custom_tool_call","call_id":"call_p","name":"apply_patch","input":"*** Begin Patch"},{"type":"custom_tool_call_output","call_id":"call_p","output":"ok"}],"tools":[{"type":"custom","name":"apply_patch","description":"Edit files","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}],"tool_choice":{"type":"custom","name":"apply_patch"}})).unwrap();
    assert_eq!(converted["tools"][0]["function"]["name"], "apply_patch");
    assert_eq!(
        converted["tools"][0]["function"]["parameters"]["required"][0],
        "input"
    );
    assert!(
        converted["tools"][0]["function"]["parameters"]["properties"]["input"]["description"]
            .as_str()
            .unwrap()
            .contains("start: patch")
    );
    assert_eq!(
        converted["messages"][0]["tool_calls"][0]["function"]["arguments"],
        r#"{"input":"*** Begin Patch"}"#
    );
    assert_eq!(converted["messages"][1]["tool_call_id"], "call_p");
    assert_eq!(converted["tool_choice"]["function"]["name"], "apply_patch");
    let converted = convert(Protocol::Messages,json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"weather","input":{"city":"Shanghai"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"rain"},{"type":"text","text":"thanks"}]}],"tools":[{"name":"weather","input_schema":{"type":"object"}}]})).unwrap();
    assert_eq!(converted["messages"][1]["role"], "tool");
    assert_eq!(converted["messages"][2]["content"][0]["text"], "thanks");
}
#[test]
fn workbuddy_codec_streaming_tools_reasoning_and_incomplete_errors() {
    for protocol in [
        codec::Protocol::Chat,
        codec::Protocol::Responses,
        codec::Protocol::Messages,
    ] {
        let mut output = codec::Output::new(protocol, "wb-test");
        let mut frames = output.start();
        for chunk in [
            json!({"choices":[{"delta":{"reasoning_content":"think","content":"result"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"weather","arguments":"{\"city\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"weather","arguments":"\"Shanghai\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_cache_hit_tokens":60,"credit":888}}),
        ] {
            frames.extend(output.push(chunk).unwrap());
        }
        let (value, end) = output.end().unwrap();
        frames.extend(end);
        let wire = frames.concat();
        assert!(!wire.contains("credit"));
        assert!(wire.contains("Shanghai"));
        let usage = crate::proxy::pipeline::usage::CanonicalUsage::from_gemini(&value["usage"]);
        assert_eq!(usage.total_input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        if protocol == codec::Protocol::Chat {
            assert_eq!(
                value["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
                "weather"
            );
        }
        if protocol == codec::Protocol::Responses {
            let events: Vec<Value> = wire
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            for (i, event) in events.iter().enumerate() {
                assert_eq!(event["sequence_number"], i);
            }
            assert_eq!(events.last().unwrap()["type"], "response.completed");
        }
    }
    let mut incomplete = codec::Output::new(codec::Protocol::Chat, "wb-test");
    incomplete
        .push(json!({"choices":[{"delta":{"content":"partial"}}]}))
        .unwrap();
    assert!(incomplete.end().is_err());
}

#[tokio::test]
async fn workbuddy_full_gateway_starts_without_google_and_settles_http_and_sse() {
    use crate::commands::proxy::{internal_start_proxy_service, ProxyServiceState};
    use crate::modules::{billing, user_token_db as db};
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    db::init_db().unwrap();
    let token = db::create_token(
        "workbuddy-gateway".into(),
        "never".into(),
        None,
        0,
        None,
        None,
        None,
        1,
        0,
        vec!["claude-sonnet-4-6".into(), "customer-sonnet".into()],
    )
    .unwrap();
    db::update_token_with_balance(
        &token.id,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(Some(1_000_000)),
    )
    .unwrap();
    billing::save_prices(vec![
        billing::ModelPrice {
            pattern: "claude-sonnet-4-6".into(),
            price: billing::Price::PerToken {
                input_price_micros: 2_000_000,
                output_price_micros: 8_000_000,
                cached_price_micros: Some(500_000),
            },
        },
        billing::ModelPrice {
            pattern: "customer-sonnet".into(),
            price: billing::Price::PerCall {
                call_price_micros: 5000,
            },
        },
    ])
    .unwrap();
    let _mock = mock().await;
    let seed_id = seed();
    workbuddy::register_models(&workbuddy::accounts::get(&seed_id).unwrap().models).unwrap();
    let port = {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    };
    let config = crate::proxy::ProxyConfig {
        port,
        api_key: "test-gateway-admin".into(),
        only_raw_quota_models: false,
        custom_mapping: std::collections::HashMap::from([(
            "customer-sonnet".into(),
            "claude-sonnet-4-6".into(),
        )]),
        auth_mode: crate::proxy::config::ProxyAuthMode::Strict,
        ..Default::default()
    };
    let state = ProxyServiceState::new();
    let status = internal_start_proxy_service(
        config.clone(),
        &state,
        crate::modules::integration::SystemManager::Headless,
        std::sync::Arc::new(crate::commands::cloudflared::CloudflaredState::new()),
    )
    .await
    .unwrap();
    assert!(status.running);
    assert_eq!(status.active_accounts, 0);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = status.base_url;
    // The same settings save used by the Web UI must update the running scheduler.
    let mut saved = crate::models::AppConfig::new();
    saved.proxy = config;
    saved.proxy.scheduling.mode = crate::proxy::sticky_config::SchedulingMode::PerformanceFirst;
    saved.proxy.scheduling.max_wait_seconds = 0;
    saved.circuit_breaker.enabled = false;
    let updated = client
        .post(format!("{base}/api/config"))
        .bearer_auth("test-gateway-admin")
        .json(&json!({"config":saved}))
        .send()
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    {
        let instance = state.instance.read().await;
        let manager = &instance.as_ref().unwrap().token_manager;
        assert_eq!(
            manager.get_sticky_config().await.mode,
            crate::proxy::sticky_config::SchedulingMode::PerformanceFirst
        );
        assert_eq!(manager.get_sticky_config().await.max_wait_seconds, 0);
        assert!(!manager.get_circuit_breaker_config().await.enabled);
    }

    let denied = client
        .get(format!("{base}/api/workbuddy-settings"))
        .bearer_auth(&token.token)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    for method in [reqwest::Method::GET, reqwest::Method::POST] {
        let denied = client
            .request(method, format!("{base}/api/workbuddy-models/verify"))
            .bearer_auth(&token.token)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }
    let progress = client
        .get(format!("{base}/api/workbuddy-models/verify"))
        .bearer_auth("test-gateway-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(progress.status(), StatusCode::OK);
    assert_eq!(progress.json::<Value>().await.unwrap()["running"], false);
    let view: Value = client
        .get(format!("{base}/api/workbuddy-settings"))
        .bearer_auth("test-gateway-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["account_count"], 1);
    assert!(view.get("api_key").is_none());
    let catalog: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth(&token.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<_> = catalog["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["claude-sonnet-4-6", "customer-sonnet"]);
    let gemini: Value = client
        .get(format!("{base}/v1beta/models"))
        .bearer_auth("test-gateway-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!gemini["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| matches!(
            entry["name"].as_str(),
            Some("models/claude-sonnet-4-6" | "models/customer-sonnet")
        )));
    let unavailable = client
        .get(format!("{base}/v1beta/models/customer-sonnet"))
        .bearer_auth("test-gateway-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::NOT_FOUND);
    let _ = unavailable.text().await.unwrap();
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for streaming in [false, true] {
            let body = to_bytes(
                request(path, "claude-sonnet-4-6", streaming).into_body(),
                10000,
            )
            .await
            .unwrap();
            let response = client
                .post(format!("{base}{path}"))
                .bearer_auth(&token.token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.text().await.unwrap().contains("claude-sonnet-4-6"));
        }
    }
    // SSE settlement runs in the monitor task; wait for its durable record.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if db::usage_logs_for_token(&token.id, 1).unwrap().total == 6 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let logs = db::usage_logs_for_token(&token.id, 1).unwrap();
    assert!(logs.items.iter().all(|log| log.input_tokens == 1000
        && log.output_tokens == 20
        && log.cached_tokens == Some(600)
        && log.cost_micros == Some(1260)));
    assert_eq!(
        db::get_token_by_id(&token.id)
            .unwrap()
            .unwrap()
            .balance_micros,
        Some(992_440)
    );
    let body = to_bytes(
        request("/v1/chat/completions", "customer-sonnet", false).into_body(),
        10000,
    )
    .await
    .unwrap();
    let aliased = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&token.token)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(aliased.status(), StatusCode::OK);
    let aliased: Value = aliased.json().await.unwrap();
    assert_eq!(aliased["model"], "customer-sonnet");
    let logs = db::usage_logs_for_token(&token.id, 1).unwrap();
    assert_eq!(logs.total, 7);
    assert!(logs
        .items
        .iter()
        .any(|log| log.model == "customer-sonnet" && log.cost_micros == Some(5000)));
    let before = db::get_token_by_id(&token.id)
        .unwrap()
        .unwrap()
        .balance_micros;
    let rejected = client.post(format!("{base}/v1/chat/completions")).bearer_auth(&token.token)
        .json(&json!({"model":"customer-sonnet","stream":true,"messages":[{"role":"user","content":"reject-before-generation"}]}))
        .send().await.unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_GATEWAY);
    assert!(!rejected
        .text()
        .await
        .unwrap()
        .contains("credential diagnostic"));
    assert_eq!(
        db::get_token_by_id(&token.id)
            .unwrap()
            .unwrap()
            .balance_micros,
        before
    );
    let mut expected_logs = db::usage_logs_for_token(&token.id, 1).unwrap().total;
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for mode in ["reject-after-generation", "incomplete-after-generation"] {
            let payload = if path == "/v1/responses" {
                json!({"model":"customer-sonnet", "stream":true, "input":mode})
            } else {
                json!({"model":"customer-sonnet", "stream":true, "max_tokens":1024,
                    "messages":[{"role":"user","content":mode}]})
            };
            let failed = client
                .post(format!("{base}{path}"))
                .bearer_auth(&token.token)
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(failed.status(), StatusCode::OK);
            assert!(failed.text().await.is_err());
            expected_logs += 1;
            tokio::time::timeout(Duration::from_secs(3), async {
                while db::usage_logs_for_token(&token.id, 1).unwrap().total < expected_logs {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    }
    let logs = db::usage_logs_for_token(&token.id, 1).unwrap();
    let failed_calls: Vec<_> = logs
        .items
        .iter()
        .filter(|log| log.model == "customer-sonnet" && log.status == 502)
        .collect();
    assert_eq!(failed_calls.len(), 7); // Includes the rejection before generation.
    assert!(failed_calls.iter().all(|log| log.cost_micros == Some(0)));
    assert!(failed_calls.iter().any(|log| log.input_tokens == 1000
        && log.output_tokens == 20
        && log.cached_tokens == Some(600)));
    assert_eq!(
        db::get_token_by_id(&token.id)
            .unwrap()
            .unwrap()
            .balance_micros,
        before
    );
    for (path, method) in [
        ("/api/workbuddy-accounts", "GET"),
        ("/api/workbuddy-login/start", "POST"),
        ("/api/workbuddy-accounts", "POST"),
    ] {
        let denied = client
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .bearer_auth(&token.token)
            .json(&json!({"realm":"cn"}))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }
    assert_zero_balance_admin_policy(&client, &base).await;
    let disabled = client
        .post(format!("{base}/api/workbuddy-settings"))
        .bearer_auth("test-gateway-admin")
        .json(&json!({"enabled":false,"models":view["models"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(disabled.status(), StatusCode::OK);
    let blocked = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&token.token)
        .json(&json!({"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = blocked.text().await.unwrap();
    assert_eq!(
        db::get_token_by_id(&token.id)
            .unwrap()
            .unwrap()
            .balance_micros,
        Some(987_440)
    );
    state.instance.write().await.take();
    if let Some(server) = state.admin_server.write().await.take() {
        server.stop().await;
    };
}

async fn assert_zero_balance_admin_policy(client: &reqwest::Client, base: &str) {
    use crate::modules::user_token_db as db;

    let admin = "test-gateway-admin";
    let model = "claude-sonnet-4-6";
    // Exercise the same HTTP create/update endpoints used by the administrator UI.
    let created = client
        .post(format!("{base}/api/user-tokens"))
        .bearer_auth(admin)
        .json(&json!({"username":"zero-balance-policy","expires_type":"never","max_ips":0}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let key: db::UserToken = created.json().await.unwrap();
    assert_eq!(key.balance_micros, Some(0));
    assert!(key.free_models.is_empty());
    assert!(key.allowed_models.is_empty());
    assert!(key.enabled);
    let edit_url = format!("{base}/api/user-tokens/{}", key.id);

    let token_count = db::list_tokens().unwrap().len();
    let invalid = client
        .post(format!("{base}/api/user-tokens"))
        .bearer_auth(admin)
        .json(&json!({"username":"invalid-null-balance","expires_type":"never","max_ips":0,"balance_micros":null}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let _ = invalid.text().await.unwrap();
    assert_eq!(db::list_tokens().unwrap().len(), token_count);
    let invalid = client
        .patch(&edit_url)
        .bearer_auth(admin)
        .json(&json!({"balance_micros":null,"free_models":[model]}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let _ = invalid.text().await.unwrap();
    let unchanged = db::get_token_by_id(&key.id).unwrap().unwrap();
    assert_eq!(unchanged.balance_micros, Some(0));
    assert!(unchanged.free_models.is_empty());

    // Legacy access lists/quotas neither grant free use nor bypass balance checks.
    let legacy = client
        .patch(&edit_url)
        .bearer_auth(admin)
        .json(&json!({"allowed_models":[model,"unpriced-workbuddy-model"],"token_quota":999999}))
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::OK);
    let _ = legacy.text().await.unwrap();
    assert!(db::get_token_by_id(&key.id)
        .unwrap()
        .unwrap()
        .free_models
        .is_empty());
    for (requested_model, expected_status, expected_code) in [
        (model, StatusCode::PAYMENT_REQUIRED, "insufficient_balance"),
        (
            "unpriced-workbuddy-model",
            StatusCode::FORBIDDEN,
            "model_not_priced",
        ),
    ] {
        let denied = client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&key.token)
            .json(&json!({"model":requested_model,"messages":[{"role":"user","content":"hello"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), expected_status);
        let body: Value = denied.json().await.unwrap();
        assert_eq!(body["error"]["code"], expected_code);
    }
    assert_eq!(db::usage_logs_for_token(&key.id, 1).unwrap().total, 0);

    let granted = client
        .patch(&edit_url)
        .bearer_auth(admin)
        .json(&json!({"free_models":[format!("models/{model}")]}))
        .send()
        .await
        .unwrap();
    assert_eq!(granted.status(), StatusCode::OK);
    let _ = granted.text().await.unwrap();
    let granted_key = db::get_token_by_id(&key.id).unwrap().unwrap();
    assert_eq!(granted_key.balance_micros, Some(0));
    assert_eq!(granted_key.free_models, vec![model]);

    // The explicit free grant overrides the existing positive model price in
    // every supported HTTP/SSE protocol; real usage must still be recorded.
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for streaming in [false, true] {
            let body = to_bytes(request(path, model, streaming).into_body(), 10000)
                .await
                .unwrap();
            let response = client
                .post(format!("{base}{path}"))
                .bearer_auth(&key.token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-upstream-provider"], "workbuddy");
            assert!(response.text().await.unwrap().contains(model));
        }
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while db::usage_logs_for_token(&key.id, 1).unwrap().total < 6 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let logs = db::usage_logs_for_token(&key.id, 1).unwrap();
    assert_eq!(logs.total, 6);
    assert!(logs.items.iter().all(|log| log.status == 200
        && log.model == model
        && log.input_tokens == 1000
        && log.output_tokens == 20
        && log.cached_tokens == Some(600)
        && log.cost_micros == Some(0)
        && log.billing_mode.as_deref() == Some("free")));
    assert_eq!(
        db::get_token_by_id(&key.id)
            .unwrap()
            .unwrap()
            .balance_micros,
        Some(0)
    );

    let removed = client
        .patch(&edit_url)
        .bearer_auth(admin)
        .json(&json!({"free_models":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);
    let _ = removed.text().await.unwrap();
    let denied = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&key.token)
        .json(&json!({"model":model,"messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::PAYMENT_REQUIRED);
    let body: Value = denied.json().await.unwrap();
    assert_eq!(body["error"]["code"], "insufficient_balance");
    assert_eq!(db::usage_logs_for_token(&key.id, 1).unwrap().total, 6);
}

/// Upstream that answers each hit with the next scripted outcome and records which
/// account (x-user-id) served it.
#[derive(Default)]
struct Script {
    outcomes: std::sync::Mutex<std::collections::VecDeque<&'static str>>,
    hits: std::sync::Mutex<Vec<String>>,
}
async fn scripted(
    State(script): State<std::sync::Arc<Script>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let uid = headers["x-user-id"].to_str().unwrap().to_owned();
    script.hits.lock().unwrap().push(uid);
    let outcome = script.outcomes.lock().unwrap().pop_front().unwrap_or("ok");
    let sse = |body: Body| {
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(body)
            .unwrap()
    };
    match outcome {
        "ok" => sse(Body::from(format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"served"},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}})
        ))),
        "empty" => sse(Body::from("data: [DONE]\n\n")),
        "partial-error" => sse(Body::from_stream(async_stream::stream! {
            yield Ok::<_, std::io::Error>(bytes::Bytes::from(format!("data: {}\n\n",
                json!({"choices":[{"delta":{"content":"partial"}}]}))));
            // Let the gateway commit the response before the upstream fails.
            tokio::time::sleep(Duration::from_millis(50)).await;
            yield Ok(bytes::Bytes::from_static(b"data: {\"error\":{\"message\":\"failed\"}}\n\n"));
        })),
        status => StatusCode::from_u16(status.parse().unwrap())
            .unwrap()
            .into_response(),
    }
}
async fn scripted_pool(
    accounts: usize,
    outcomes: &[&'static str],
) -> (NativeMock, std::sync::Arc<Script>) {
    for n in 0..accounts {
        let mut account = workbuddy::accounts::parse_import(json!({"uid":format!("uid-{n}"),"accessToken":format!("token-{n}"),"expiresAt":workbuddy::accounts::now()+3600})).unwrap();
        account.models.push(workbuddy::accounts::Model {
            id: "claude-sonnet-4-6".into(),
            name: "Sonnet".into(),
        });
        workbuddy::accounts::upsert(account).unwrap();
    }
    let script = std::sync::Arc::new(Script::default());
    script.outcomes.lock().unwrap().extend(outcomes);
    let mock = serve(
        Router::new()
            .route("/v2/chat/completions", post(scripted))
            .with_state(script.clone()),
    )
    .await;
    (mock, script)
}
async fn send(streaming: bool, policy: Policy) -> Response {
    let payload = json!({"messages":[{"role":"user","content":"hello"}],"stream":streaming});
    forward(
        5,
        payload,
        streaming,
        "wb-sonnet",
        "claude-sonnet-4-6",
        codec::Protocol::Chat,
        "scope",
        policy,
    )
    .await
}
fn hits(script: &Script) -> Vec<String> {
    std::mem::take(&mut *script.hits.lock().unwrap())
}

#[tokio::test]
async fn workbuddy_pre_generation_failure_fails_over_and_rebinds_session() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let (_mock, script) = scripted_pool(2, &["429"]).await;
    // Without the breaker a failed account stays schedulable, so only exclusion
    // and the session rebind keep the retry and the next request off it.
    let mut policy = Policy::default();
    policy.breaker.enabled = false;
    for streaming in [true, false] {
        let response = send(streaming, policy.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let data = to_bytes(response.into_body(), 100_000).await.unwrap();
        assert!(String::from_utf8_lossy(&data).contains("served"));
        let served = hits(&script);
        assert_eq!(served.len(), 2);
        assert_ne!(served[0], served[1]);
        // The session follows the account that served it.
        let response = send(streaming, policy.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 100_000).await.unwrap();
        assert_eq!(hits(&script), vec![served[1].clone()]);
        // Next round: the newly bound account fails first and the request moves back.
        script.outcomes.lock().unwrap().extend(["empty"]);
    }
    let observed = workbuddy::accounts::list().unwrap();
    assert!(observed.iter().all(|a| a.in_flight == 0));
    assert_eq!(observed.iter().map(|a| a.health.failures).sum::<u64>(), 2);
}

#[tokio::test]
async fn workbuddy_failure_after_output_or_bad_request_is_not_retried() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let (_mock, script) = scripted_pool(2, &["partial-error", "partial-error", "400"]).await;
    let response = send(true, Policy::default()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(to_bytes(response.into_body(), 100_000).await.is_err());
    assert_eq!(hits(&script).len(), 1);
    let response = send(false, Policy::default()).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(hits(&script).len(), 1);
    workbuddy::accounts::scheduling::clear_cooldowns(None);
    let response = send(false, Policy::default()).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(hits(&script).len(), 1);
}

#[tokio::test]
async fn workbuddy_all_accounts_failing_returns_last_status_within_attempt_cap() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let (_mock, script) = scripted_pool(4, &["503", "empty", "429"]).await;
    let response = send(true, Policy::default()).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "workbuddy_upstream_rejected");
    let tried = hits(&script);
    assert_eq!(tried.len(), 3);
    assert_eq!(
        tried.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    // A fixed account has no alternative, so its failure is returned as is.
    workbuddy::accounts::scheduling::clear_cooldowns(None);
    script.outcomes.lock().unwrap().extend(["503"]);
    let policy = Policy {
        preferred: Some(workbuddy::accounts::list().unwrap()[0].id.clone()),
        ..Default::default()
    };
    assert_eq!(send(false, policy).await.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(hits(&script).len(), 1);
}
