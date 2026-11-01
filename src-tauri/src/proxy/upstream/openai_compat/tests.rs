use super::*;
use axum::{routing::post, Router};
use tower::ServiceExt;

struct Mock {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock() -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let router = Router::new().route("/v1/chat/completions", post(completions));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Mock { base, task }
}

fn sse(events: &[Value]) -> Response {
    let mut body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

async fn completions(headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    assert_eq!(headers[header::AUTHORIZATION], "Bearer sk-upstream");
    assert!(!headers.contains_key("cookie"));
    assert_eq!(body["model"], "native-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("thinking").is_none());
    let messages = body["messages"].as_array().unwrap();
    assert!(messages.iter().all(|m| m["role"] != "developer"));
    let last = messages.last().unwrap()["content"].clone();
    match last.as_str().unwrap_or_default() {
        "json-fallback" => Json(json!({
            "id": "x", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":1}"}}]}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3}
        }))
        .into_response(),
        "structured" => {
            assert_eq!(body["response_format"]["type"], "json_schema");
            assert_eq!(body["response_format"]["json_schema"]["name"], "answer");
            sse(&[json!({"choices":[{"index":0,"delta":{"content":"{}"},"finish_reason":"stop"}]})])
        }
        "bad-request" => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "context length exceeded", "code": "context_length_exceeded"}})),
        )
            .into_response(),
        "bad-key" => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "Incorrect API key provided: sk-up***"}})),
        )
            .into_response(),
        "stream-error" => sse(&[json!({"error": {"message": "overloaded"}})]),
        _ => {
            assert_eq!(last, "hello");
            sse(&[
                json!({"choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"thinking"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{"content":"你好"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20}}),
            ])
        }
    }
}

fn upstream(base: &str) -> Upstream {
    Upstream {
        id: "relay".into(),
        name: "Relay".into(),
        base_url: base.into(),
        api_key: "sk-upstream".into(),
        enabled: true,
        models: vec![],
    }
}

fn published() -> PublishedModel {
    PublishedModel {
        id: "published:relay".into(),
        public_id: "relay-model".into(),
        display_name: "relay-model".into(),
        source_id: custom_upstream::source_id("relay"),
        native_model: "native-model".into(),
        enabled: true,
        legacy: false,
    }
}

fn request(path: &str, content: &str, stream: bool) -> Request {
    let mut payload = match path {
        "/v1/responses" => {
            json!({"instructions":"be brief","input":[{"type":"message","role":"developer","content":"rules"},{"type":"message","role":"user","content":content}]})
        }
        "/v1/messages" => {
            json!({"system":"be brief","max_tokens":64,"thinking":{"type":"enabled","budget_tokens":1024},"messages":[{"role":"user","content":content}]})
        }
        _ => {
            json!({"messages":[{"role":"developer","content":"rules"},{"role":"user","content":content}]})
        }
    };
    payload["model"] = json!("relay-model");
    payload["stream"] = json!(stream);
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("cookie", "private")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

async fn send(base: &str, request: Request) -> (StatusCode, axum::http::HeaderMap, String) {
    let base = base.to_string();
    let app = Router::new().fallback(move |request: Request| {
        let base = base.clone();
        async move {
            dispatch(
                reqwest::Client::new(),
                upstream(&base),
                published(),
                10,
                request,
            )
            .await
        }
    });
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn openai_compatible_upstream_serves_three_protocols_streaming_and_not() {
    let mock = mock().await;
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for stream in [false, true] {
            let (status, headers, body) = send(&mock.base, request(path, "hello", stream)).await;
            assert_eq!(status, StatusCode::OK, "{path} {stream}: {body}");
            assert_eq!(headers["x-upstream-provider"], "openai_compatible");
            assert_eq!(headers["x-account-email"], "relay@openai-compatible");
            assert_eq!(headers["x-mapped-model"], "native-model");
            assert!(body.contains("你好"), "{body}");
            assert!(body.contains("relay-model"), "{body}");
            if !stream {
                let value: Value = serde_json::from_str(&body).unwrap();
                let usage =
                    crate::proxy::pipeline::usage::CanonicalUsage::from_gemini(&value["usage"]);
                assert_eq!(usage.total_input_tokens, 100);
                assert_eq!(usage.output_tokens, 20);
            }
        }
    }
}

#[tokio::test]
async fn openai_compatible_upstream_accepts_non_streaming_json_answers() {
    let mock = mock().await;
    let (status, _, body) = send(
        &mock.base,
        request("/v1/chat/completions", "json-fallback", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        value["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "lookup"
    );
    let (status, _, body) = send(&mock.base, request("/v1/messages", "json-fallback", true)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"tool_use\""), "{body}");
}

#[tokio::test]
async fn openai_compatible_upstream_maps_structured_output_and_rejections() {
    let mock = mock().await;
    let mut structured = json!({"model":"relay-model","input":"structured","text":{"format":{"type":"json_schema","name":"answer","schema":{"type":"object"},"strict":true}}});
    structured["stream"] = json!(false);
    let structured_request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(structured.to_string()))
        .unwrap();
    let (status, _, body) = send(&mock.base, structured_request).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _, body) = send(
        &mock.base,
        request_for("/v1/chat/completions", "bad-request"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("context length exceeded"), "{body}");

    let (status, _, body) = send(&mock.base, request_for("/v1/chat/completions", "bad-key")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("custom_upstream_auth_failed"));
    assert!(!body.contains("sk-up"));

    // An error event before any output is a failed request, not an empty 200 stream.
    let (status, _, _) = send(
        &mock.base,
        request("/v1/chat/completions", "stream-error", true),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    let (status, _, _) = send(
        "http://127.0.0.1:9/v1",
        request("/v1/chat/completions", "hello", false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    let images = Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"model":"relay-model","prompt":"x"}"#))
        .unwrap();
    assert_eq!(send(&mock.base, images).await.0, StatusCode::BAD_REQUEST);
}

fn request_for(path: &str, content: &str) -> Request {
    request(path, content, false)
}

#[test]
fn prepare_keeps_portable_chat_fields_only() {
    let prepared = prepare(
        json!({"model":"public","thinking":{"type":"enabled"},"messages":[
            {"role":"developer","content":"rules"},
            {"role":"user","content":[{"type":"image_url","image_url":"https://x/y.png"}]}
        ],"temperature":0.2}),
        "native",
    );
    assert_eq!(prepared["model"], "native");
    assert_eq!(prepared["stream"], true);
    assert!(prepared.get("thinking").is_none());
    assert_eq!(prepared["messages"][0]["role"], "system");
    assert_eq!(
        prepared["messages"][1]["content"][0]["image_url"]["url"],
        "https://x/y.png"
    );
    assert_eq!(prepared["temperature"], 0.2);
}
