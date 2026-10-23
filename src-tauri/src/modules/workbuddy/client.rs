//! Tencent's native WorkBuddy wire protocol. Endpoint/header references are recorded
//! in docs/WORKBUDDY_UPSTREAM.md; never send downstream credentials to these hosts.
use super::accounts::{Account, Model, Realm};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Method,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::OnceLock, time::Duration};

pub fn http() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| "Cannot initialize WorkBuddy connection".into())
        })
        .as_ref()
        .map_err(Clone::clone)
}
pub fn base(realm: Realm) -> String {
    #[cfg(test)]
    if let Ok(value) = std::env::var("STELLAR_TEST_WORKBUDDY_BASE") {
        return value;
    }
    match realm {
        Realm::Cn => "https://copilot.tencent.com",
        Realm::Global => "https://www.workbuddy.ai",
    }
    .into()
}
fn add(headers: &mut HeaderMap, key: &'static str, value: &str) -> Result<(), String> {
    headers.insert(
        key,
        HeaderValue::from_str(value).map_err(|_| "Invalid WorkBuddy identity header")?,
    );
    Ok(())
}
fn default_user_agent(realm: Realm) -> &'static str {
    match realm {
        Realm::Cn => "WorkBuddy/5.5.4 WorkBuddy/5.5.4 CLI/2.137.1",
        Realm::Global => "WorkBuddy/5.5.4 WorkBuddy AI/5.5.4 CLI/2.137.1",
    }
}
pub fn headers(realm: Realm, account: Option<&Account>) -> Result<HeaderMap, String> {
    let origin = match realm {
        Realm::Cn => "https://www.codebuddy.cn",
        Realm::Global => "https://www.workbuddy.ai",
    };
    let ua = account
        .map(|a| a.client.user_agent.as_str())
        .filter(|ua| !ua.is_empty())
        .unwrap_or_else(|| default_user_agent(realm));
    let mut h = HeaderMap::new();
    for (key, value) in [
        ("content-type", "application/json"),
        ("accept", "application/json"),
        ("x-requested-with", "XMLHttpRequest"),
        ("x-codebuddy-request", "1"),
        ("origin", origin),
        ("user-agent", ua),
        (
            "accept-language",
            if realm == Realm::Cn { "zh-CN" } else { "en-US" },
        ),
    ] {
        add(&mut h, key, value)?;
    }
    add(&mut h, "referer", &format!("{origin}/"))?;
    if let Some(account) = account {
        add(
            &mut h,
            "authorization",
            &format!("Bearer {}", account.access_token),
        )?;
        add(&mut h, "x-user-id", &account.uid)?;
        let machine = if account.client.machine_id.is_empty() {
            let digest = Sha256::digest(format!("wb2a:machine:{}", account.uid).as_bytes());
            digest[..18]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        } else {
            account.client.machine_id.clone()
        };
        let session = if account.client.session_id.is_empty() {
            let digest = Sha256::digest(format!("wb2a:session:{}", account.uid).as_bytes());
            digest[..18]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        } else {
            account.client.session_id.clone()
        };
        add(&mut h, "x-machine-id", &machine)?;
        add(&mut h, "x-session-id", &session)?;
    }
    Ok(h)
}
pub async fn raw_json_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "WorkBuddy returned HTTP {}; check account authorization and network",
            status.as_u16()
        ));
    }
    read_json_body(response).await
}

/// Bounded parsing for endpoints that must inspect a non-2xx business result.
/// Callers are responsible for interpreting the HTTP status before accepting it.
pub(super) async fn read_json_body(response: reqwest::Response) -> Result<Value, String> {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "WorkBuddy response was interrupted")?;
        if data.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err("WorkBuddy response exceeds size limit".into());
        }
        data.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&data).map_err(|_| "WorkBuddy returned invalid JSON")?;
    Ok(value)
}
pub async fn json_response(response: reqwest::Response) -> Result<Value, String> {
    let value = raw_json_response(response).await?;
    if let Some(code) = value.get("code") {
        if code.as_i64() != Some(0) && code.as_str() != Some("0") {
            // Do not relay upstream diagnostics: they can contain credentials.
            return Err(
                "WorkBuddy rejected this operation; refresh or reauthorize the account".into(),
            );
        }
        return value
            .get("data")
            .cloned()
            .ok_or_else(|| "WorkBuddy response is missing data".into());
    }
    Ok(value)
}
pub async fn call(
    realm: Realm,
    method: Method,
    path: &str,
    account: Option<&Account>,
    query: &[(&str, &str)],
    body: Option<Value>,
) -> Result<Value, String> {
    let mut request = http()?
        .request(method, format!("{}{path}", base(realm)))
        .headers(headers(realm, account)?)
        .query(query)
        .timeout(Duration::from_secs(25));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| "Cannot connect to WorkBuddy; check server network")?;
    json_response(response).await
}
pub async fn refresh(account: &mut Account) -> Result<(), String> {
    if account.refresh_token.is_empty() {
        return Err("This WorkBuddy account has no refreshToken; authorize it again".into());
    }
    let mut h = headers(account.realm, Some(account))?;
    add(&mut h, "x-refresh-token", &account.refresh_token)?;
    add(&mut h, "x-auth-refresh-source", "plugin")?;
    if !account.enterprise_id.is_empty() {
        add(&mut h, "x-enterprise-id", &account.enterprise_id)?;
    }
    if !account.device_token.is_empty() {
        add(&mut h, "x-device-token", &account.device_token)?;
    }
    let response = http()?
        .post(format!(
            "{}/v2/plugin/auth/token/refresh",
            base(account.realm)
        ))
        .headers(h)
        .timeout(Duration::from_secs(25))
        .send()
        .await
        .map_err(|_| "WorkBuddy token refresh connection failed")?;
    let data = json_response(response).await?;
    let access = data
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("WorkBuddy refresh did not return a token; authorize again")?;
    account.access_token = access.into();
    if let Some(refresh) = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        account.refresh_token = refresh.into();
    }
    if let Some(domain) = data.get("domain").and_then(Value::as_str) {
        account.domain = domain.into();
    }
    account.expires_at = super::accounts::now()
        + data
            .get("expiresIn")
            .and_then(Value::as_i64)
            .unwrap_or(3600)
            .clamp(1, 365 * 86400);
    super::accounts::validate(account)
}
fn parse_models(data: &Value, realm: Realm) -> Vec<Model> {
    let list = data
        .as_array()
        .or_else(|| data.get("models").and_then(Value::as_array));
    let cli = if realm == Realm::Cn {
        data.get("agents")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find(|a| a.get("name").and_then(Value::as_str) == Some("cli"))
            })
            .and_then(|a| a.get("models"))
            .and_then(Value::as_array)
    } else {
        None
    };
    list.into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.as_str().or_else(|| m.get("id").and_then(Value::as_str))?;
            if !super::valid_model_id(id)
                || m.get("disabled").and_then(Value::as_bool) == Some(true)
            {
                return None;
            }
            if cli.is_some_and(|ids| !ids.is_empty() && !ids.iter().any(|v| v.as_str() == Some(id)))
            {
                return None;
            }
            if realm == Realm::Cn
                && (id.starts_with("nes-")
                    || m.get("maxOutputTokens")
                        .and_then(Value::as_u64)
                        .is_some_and(|n| n > 0 && n <= 256)
                    || m.get("tags").and_then(Value::as_array).is_some_and(|tags| {
                        tags.iter().any(|v| v.as_str() == Some("text-to-image"))
                    }))
            {
                return None;
            }
            Some(Model {
                id: id.into(),
                name: m.get("name").and_then(Value::as_str).unwrap_or(id).into(),
            })
        })
        .collect()
}
pub async fn models(account: &Account) -> Result<Vec<Model>, String> {
    let enterprise = async {
        let paths: &[&str] = if account.realm == Realm::Global {
            &[
                "/v2/enterprises/personal/models",
                "/console/enterprises/personal/models",
            ]
        } else {
            &["/console/enterprises/personal/models"]
        };
        for path in paths {
            if let Ok(data) = call(account.realm, Method::GET, path, Some(account), &[], None).await
            {
                return Ok(data);
            }
        }
        Err::<Value, String>("Cannot synchronize WorkBuddy model catalog".into())
    };
    let (ent, v3) = tokio::join!(
        enterprise,
        call(
            account.realm,
            Method::GET,
            "/v3/config",
            Some(account),
            &[],
            None
        )
    );
    let mut merged = BTreeMap::new();
    for data in [ent, v3].into_iter().flatten() {
        for model in parse_models(&data, account.realm) {
            merged.insert(model.id.clone(), model);
        }
    }
    if merged.is_empty() {
        return Err(
            "No usable models returned by WorkBuddy; check authorization and model availability"
                .into(),
        );
    }
    Ok(merged.into_values().collect())
}

pub fn session_key(scope: &str, payload: &Value) -> String {
    let first = payload
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|m| m["role"] == "user"))
        .cloned()
        .unwrap_or(Value::Null);
    format!("{:x}", Sha256::digest(format!("{scope}:{first}")))
}

pub(super) fn error_code(value: &Value) -> Option<i64> {
    let code = value.get("code").or_else(|| value.pointer("/error/code"))?;
    code.as_i64().or_else(|| {
        let text = code.as_str()?;
        if let Some(suffix) = text.strip_prefix("11-") {
            format!("11{suffix}").parse().ok()
        } else {
            text.parse().ok()
        }
    })
}

pub async fn chat(
    account: &Account,
    model: &str,
    mut payload: Value,
    scope: &str,
    deadline: tokio::time::Instant,
) -> Result<reqwest::Response, String> {
    payload["model"] = json!(model);
    payload["stream"] = json!(true);
    payload["stream_options"] = json!({"include_usage":true});
    if payload.get("max_tokens").is_none() {
        if let Some(value) = payload.get("max_completion_tokens").cloned() {
            payload["max_tokens"] = value;
        }
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("max_completion_tokens");
        obj.remove("n");
    }
    if let Some(choice) = payload
        .get("tool_choice")
        .filter(|v| v.is_object())
        .cloned()
    {
        if let Some(name) = choice.pointer("/function/name").and_then(Value::as_str) {
            payload["tool_choice"] = json!(name);
        } else {
            return Err("Unsupported WorkBuddy tool_choice".into());
        }
    }
    if payload["tool_choice"] == "none" {
        let obj = payload.as_object_mut().ok_or("Invalid request")?;
        obj.remove("tools");
        obj.remove("tool_choice");
    }
    if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("developer") {
                message["role"] = json!("system");
            }
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in content {
                    if let Some(url) = part
                        .get("image_url")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                    {
                        part["image_url"] = json!({"url":url});
                    }
                }
            }
        }
    }
    if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
        // Native WorkBuddy requires a leading system message, even for an ordinary
        // user-only request. Keep an existing system/developer prompt unchanged.
        if messages
            .first()
            .is_none_or(|message| message["role"] != "system")
        {
            messages.insert(
                0,
                json!({"role":"system","content":"You are a helpful assistant."}),
            );
        }
    }
    let conversation = session_key(scope, &payload);
    let request_id = uuid::Uuid::new_v4().simple().to_string();
    let mut h = headers(account.realm, Some(account))?;
    add(&mut h, "accept", "text/event-stream")?;
    let product_version = if account.client.product_version.is_empty() {
        "5.5.4"
    } else {
        account.client.product_version.as_str()
    };
    for (name, value) in [
        ("x-agent-purpose", "conversation"),
        ("x-ide-name", "WorkBuddy"),
        ("x-ide-type", "WorkBuddy"),
        ("x-ide-version", product_version),
        ("x-product", "WorkBuddy"),
    ] {
        add(&mut h, name, value)?;
    }
    add(&mut h, "x-conversation-id", &conversation)?;
    add(&mut h, "x-conversation-request-id", &request_id)?;
    add(&mut h, "x-conversation-message-id", &request_id)?;
    add(&mut h, "x-request-id", &request_id)?;
    add(&mut h, "x-root-request-id", &request_id)?;
    add(&mut h, "x-trace-id", &request_id)?;
    if account.enterprise_id.is_empty() || account.realm == Realm::Global {
        add(&mut h, "x-no-enterprise-id", "1")?;
    } else {
        add(&mut h, "x-enterprise-id", &account.enterprise_id)?;
    }
    if account.realm == Realm::Global {
        add(&mut h, "x-domain", "www.workbuddy.ai")?;
    } else if !account.domain.is_empty() {
        add(&mut h, "x-domain", &account.domain)?;
    } else {
        add(&mut h, "x-no-department-info", "1")?;
    }
    if !account.device_token.is_empty() {
        add(&mut h, "x-device-token", &account.device_token)?;
    }
    let send = |path: &str| {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("WorkBuddy model request timed out".to_owned());
        }
        http().map(|client| {
            client
                .post(format!("{}{path}", base(account.realm)))
                .headers(h.clone())
                .timeout(remaining)
                .json(&payload)
        })
    };
    let response = send("/v2/chat/completions")?
        .send()
        .await
        .map_err(|_| "WorkBuddy model request failed or timed out")?;
    if response.status().is_success() {
        return Ok(response);
    }
    // Tencent has two official chat paths. Only an explicit, pre-generation channel
    // rejection permits the console path; never replay a stream or a network failure.
    let status = response.status();
    let response_headers = response.headers().clone();
    let bytes = axum::body::to_bytes(
        axum::body::Body::from_stream(response.bytes_stream()),
        64 * 1024,
    )
    .await
    .map_err(|_| "WorkBuddy error response was interrupted or too large")?;
    let code = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| error_code(&value));
    if code == Some(11128) {
        return send("/console/chat/completions")?
            .send()
            .await
            .map_err(|_| "WorkBuddy model request failed or timed out".into());
    }
    let mut rejected = axum::http::Response::builder()
        .status(status)
        // Preserve only the numeric result code for model-availability decisions.
        // Upstream messages may contain credential diagnostics and stay discarded.
        .body(
            code.map(|code| json!({"code":code}).to_string().into_bytes())
                .unwrap_or_default(),
        )
        .map_err(|_| "Invalid upstream response")?;
    *rejected.headers_mut() = response_headers;
    rejected
        .headers_mut()
        .remove(reqwest::header::CONTENT_LENGTH);
    rejected
        .headers_mut()
        .remove(reqwest::header::CONTENT_ENCODING);
    Ok(reqwest::Response::from(rejected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::HeaderMap,
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Mock(tokio::task::JoinHandle<()>);
    impl Drop for Mock {
        fn drop(&mut self) {
            self.0.abort();
            std::env::remove_var("STELLAR_TEST_WORKBUDDY_BASE");
        }
    }
    #[tokio::test]
    async fn workbuddy_request_deadline_bounds_refresh_wait_and_preserves_rotated_tokens() {
        use super::super::accounts;
        use tokio::{sync::Notify, time::Instant};

        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let app = Router::new().route(
            "/v2/plugin/auth/token/refresh",
            post({
                let calls = calls.clone();
                let started = started.clone();
                let release = release.clone();
                move || {
                    let calls = calls.clone();
                    let started = started.clone();
                    let release = release.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        release.notified().await;
                        Json(json!({"code":0,"data":{"accessToken":"fresh-secret","refreshToken":"rotated-secret","expiresIn":3600}}))
                    }
                }
            }),
        );
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let mut account = accounts::parse_import(json!({
            "uid":"deadline", "accessToken":"old-secret", "refreshToken":"old-refresh",
            "expiresAt":accounts::now()+60,
        }))
        .unwrap();
        account.models.push(accounts::Model {
            id: "test".into(),
            name: "Test".into(),
        });
        account
            .verified_models
            .insert("test".into(), accounts::now());
        let id = accounts::upsert(account).unwrap().id;

        // Waiting on a maintenance refresh must return by the request deadline,
        // release pool capacity, and not enqueue work after the caller leaves.
        let lock = accounts::refresh_lock(&id).unwrap();
        let guard = lock.lock().await;
        let lease = accounts::acquire("test").unwrap();
        let waiting = async {
            let _lease = lease;
            accounts::fresh_before(&id, Instant::now() + Duration::from_millis(40)).await
        };
        assert!(tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .is_err());
        assert_eq!(accounts::list().unwrap()[0].in_flight, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(guard);

        // Once rotation has started, a request timeout must release its lease
        // while the refresh continues through saving the replacement credentials.
        let lease = accounts::acquire("test").unwrap();
        let preparing = async {
            let _lease = lease;
            accounts::fresh_before(&id, Instant::now() + Duration::from_millis(200)).await
        };
        let (result, received) = tokio::join!(
            preparing,
            tokio::time::timeout(Duration::from_secs(1), started.notified()),
        );
        assert!(received.is_ok());
        assert!(result.is_err());
        assert_eq!(accounts::list().unwrap()[0].in_flight, 0);
        assert_eq!(accounts::get(&id).unwrap().refresh_token, "old-refresh");
        release.notify_one();
        let updated = tokio::time::timeout(Duration::from_secs(2), accounts::fresh(&id, false))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.access_token, "fresh-secret");
        assert!(updated.verified_models.contains_key("test"));
        assert_eq!(accounts::get(&id).unwrap().refresh_token, "rotated-secret");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn workbuddy_chat_uses_remaining_deadline_without_rounding_up() {
        use super::super::accounts;
        use tokio::time::Instant;

        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let app = Router::new().route(
            "/v2/chat/completions",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                "too late"
            }),
        );
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let account = accounts::parse_import(json!({"uid":"test","accessToken":"test"})).unwrap();
        let started = Instant::now();
        let result = chat(
            &account,
            "test",
            json!({"messages":[]}),
            "test",
            started + Duration::from_millis(40),
        )
        .await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(400));
    }

    #[tokio::test]
    async fn workbuddy_official_login_refresh_singleflight_and_model_sync() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let count = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let app = Router::new()
            .route("/v2/plugin/auth/state",post(|| async { Json(json!({"code":0,"data":{"state":"mock-state","authUrl":"https://copilot.tencent.com/authorize?state=mock-state"}})) }))
            .route("/v2/plugin/auth/token",get(|| async { Json(json!({"code":0,"data":{"accessToken":"login-secret","refreshToken":"refresh-secret","expiresIn":60}})) }))
            .route("/v2/plugin/login/account",get(|headers:HeaderMap| async move { assert_eq!(headers["authorization"],"Bearer login-secret"); Json(json!({"code":0,"data":{"uid":12345,"nickname":"Example"}})) }))
            .route("/v2/plugin/auth/token/refresh",post(|State(count):State<Arc<AtomicUsize>>,headers:HeaderMap| async move {
                assert_eq!(headers["x-refresh-token"],"refresh-secret"); count.fetch_add(1,Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                Json(json!({"code":0,"data":{"accessToken":"fresh-secret","refreshToken":"rotated-secret","expiresIn":3600}}))
            }))
            .route("/console/enterprises/personal/models",get(|| async { Json(json!({"code":0,"data":{"models":[{"id":"native-model","name":"Model"}]}})) }))
            .route("/v3/config",get(|| async { Json(json!({"code":0,"data":{"models":[{"id":"native-model","name":"Latest"},{"id":"not-in-cli"},{"id":"nes-test"}],"agents":[{"name":"cli","models":["native-model","nes-test"]}]}})) }))
            .with_state(count.clone());
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let started = super::super::login::start(Realm::Cn).await.unwrap();
        let logged = super::super::login::poll(&started.id).await.unwrap();
        let id = logged.account.unwrap().id;
        assert_eq!(logged.status, "ready");
        assert_eq!(super::super::accounts::list().unwrap()[0].uid, "12345");
        let again = super::super::login::poll(&started.id).await.unwrap();
        assert_eq!(again.account.unwrap().id, id);
        assert_eq!(super::super::accounts::list().unwrap().len(), 1);
        let (first, second) = tokio::join!(
            super::super::accounts::fresh(&id, false),
            super::super::accounts::fresh(&id, false)
        );
        assert_eq!(first.unwrap().access_token, "fresh-secret");
        assert_eq!(second.unwrap().access_token, "fresh-secret");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let account = super::super::accounts::sync(&id, false).await.unwrap();
        assert_eq!(account.models.len(), 1);
        assert_eq!(account.candidate_model_count, 1);
        assert_eq!(
            super::super::accounts::get(&id).unwrap().models[0].name,
            "Latest"
        );
        let settings = super::super::load().unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.models["native-model"], "native-model");
        super::super::login::cancel(&started.id).await.unwrap();
        assert!(super::super::login::poll(&started.id).await.is_err());
    }
    #[tokio::test]
    async fn workbuddy_leading_system_is_added_without_replacing_caller_instructions() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/v2/chat/completions",
                    post(|Json(body): Json<Value>| async move { Json(body) }),
                ),
            )
            .await
            .unwrap();
        }));
        let account =
            super::super::accounts::parse_import(json!({"uid":"test","accessToken":"secret"}))
                .unwrap();
        for role in ["user", "system", "developer"] {
            let payload = json!({"messages":[{"role":role,"content":"original content"}]});
            let response = chat(
                &account,
                "test",
                payload,
                "test",
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["messages"][0]["role"], "system");
            if role == "user" {
                assert_eq!(body["messages"].as_array().unwrap().len(), 2);
                assert_eq!(
                    body["messages"][1],
                    json!({"role":"user","content":"original content"})
                );
            } else {
                assert_eq!(body["messages"].as_array().unwrap().len(), 1);
                assert_eq!(body["messages"][0]["content"], "original content");
            }
        }
    }

    #[tokio::test]
    async fn workbuddy_only_explicit_channel_rejection_uses_console_path() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let app = Router::new()
            .route(
                "/v2/chat/completions",
                post(|Json(body): Json<Value>| async move {
                    (
                        axum::http::StatusCode::FORBIDDEN,
                        Json(
                            json!({"code":if body["model"] == "channel-model" {11128} else {403}}),
                        ),
                    )
                }),
            )
            .route(
                "/console/chat/completions",
                post(|State(count): State<Arc<AtomicUsize>>| async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    "accepted"
                }),
            )
            .with_state(calls.clone());
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let account =
            super::super::accounts::parse_import(json!({"uid":"test","accessToken":"secret"}))
                .unwrap();
        let payload = json!({"messages":[{"role":"user","content":"hi"}]});
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        assert!(
            chat(&account, "channel-model", payload.clone(), "test", deadline)
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert_eq!(
            chat(&account, "other-model", payload, "test", deadline)
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
