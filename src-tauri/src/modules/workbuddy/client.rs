//! Tencent's native WorkBuddy wire protocol. Endpoint/header references are recorded
//! in docs/WORKBUDDY_UPSTREAM.md; never send downstream credentials to these hosts.
use super::accounts::{Account, Model, Realm};
use rquest::{
    header::{HeaderMap, HeaderValue},
    Method,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::OnceLock, time::Duration};

/// Connection settings shared by direct and egress clients.
/// Uses the captured official Electron/Node HTTP/1.1 profile. The former Chrome
/// profile remains an explicit administrator rollback, never an automatic replay.
pub fn builder() -> rquest::ClientBuilder {
    if !std::env::var("ABV_WORKBUDDY_LEGACY_CHROME")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"))
    {
        return super::transport::builder();
    }
    rquest::Client::builder()
        .emulation(
            rquest_util::EmulationOption::builder()
                .emulation(rquest_util::Emulation::Chrome123)
                .skip_headers(true)
                .build(),
        )
        .no_proxy()
        .redirect(rquest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        // Two idle sockets match the per-account concurrency cap. Each account
        // owns a client, so this does not pool those accounts together.
        .pool_max_idle_per_host(2)
}
/// One client for login, before an account exists. Account traffic must use
/// [`http_for`], which keeps a separate connection pool per account.
pub fn http() -> Result<&'static rquest::Client, String> {
    static CLIENT: OnceLock<Result<rquest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            builder()
                .build()
                .map_err(|_| "Cannot initialize WorkBuddy connection".into())
        })
        .as_ref()
        .map_err(Clone::clone)
}
/// The account's own egress route (its airport node, or direct).
pub async fn http_for(account: &Account) -> Result<rquest::Client, String> {
    super::egress::client_for(account).await
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
pub fn headers(realm: Realm, account: Option<&Account>) -> Result<HeaderMap, String> {
    let origin = match realm {
        Realm::Cn => "https://www.codebuddy.cn",
        Realm::Global => "https://www.workbuddy.ai",
    };
    let ua = account
        .map(|a| a.client.user_agent.clone())
        .filter(|ua| !ua.is_empty())
        .unwrap_or_else(|| super::identity::pinned_user_agent(realm));
    let mut h = HeaderMap::new();
    for (key, value) in [
        ("content-type", "application/json"),
        ("accept", "application/json"),
        ("x-requested-with", "XMLHttpRequest"),
        ("x-codebuddy-request", "1"),
        ("origin", origin),
        ("user-agent", &ua),
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
        // Empty ids stay omitted. Do not derive the published wb2a fingerprint here.
        if !account.client.machine_id.is_empty() {
            add(&mut h, "x-machine-id", &account.client.machine_id)?;
        }
        if !account.client.session_id.is_empty() {
            add(&mut h, "x-session-id", &account.client.session_id)?;
        }
    }
    Ok(h)
}
pub async fn raw_json_response(response: rquest::Response) -> Result<Value, String> {
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
pub(super) async fn read_json_body(response: rquest::Response) -> Result<Value, String> {
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
pub async fn json_response(response: rquest::Response) -> Result<Value, String> {
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
    account: &Account,
    query: &[(&str, &str)],
    body: Option<Value>,
) -> Result<Value, String> {
    let http = http_for(account).await?;
    call_with(&http, realm, method, path, Some(account), query, body).await
}
/// `call` over an explicit route, for login requests made before an account exists.
pub async fn call_with(
    http: &rquest::Client,
    realm: Realm,
    method: Method,
    path: &str,
    account: Option<&Account>,
    query: &[(&str, &str)],
    body: Option<Value>,
) -> Result<Value, String> {
    let mut request = http
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
    let response = http_for(account)
        .await?
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
            if let Ok(data) = call(account.realm, Method::GET, path, account, &[], None).await {
                return Ok(data);
            }
        }
        Err::<Value, String>("Cannot synchronize WorkBuddy model catalog".into())
    };
    let (ent, v3) = tokio::join!(
        enterprise,
        call(account.realm, Method::GET, "/v3/config", account, &[], None)
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
    crate::proxy::session_manager::SessionManager::account_session_key(scope, payload)
}

/// Why a chat request produced no upstream HTTP response. Only `Connection`
/// may move to another account; the others are final for this request.
#[derive(Debug)]
pub enum ChatError {
    /// The TCP/TLS request never received an HTTP response. One other account may try.
    Connection(String),
    /// The caller's request cannot be expressed on the WorkBuddy wire.
    Request(String),
    /// This account's stored data cannot form a valid request.
    Account(String),
    /// The deadline ran out before or while waiting for the upstream response.
    Timeout(String),
}
struct Conversation {
    id: String,
    touched: i64,
}
fn conversations() -> &'static std::sync::Mutex<std::collections::HashMap<String, Conversation>> {
    static CONVERSATIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Conversation>>,
    > = std::sync::OnceLock::new();
    CONVERSATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
/// 32-hex id stable for one downstream session. Not a hash of the prompt.
fn upstream_conversation_id(session: &str) -> String {
    let Ok(mut map) = conversations().lock() else {
        return uuid::Uuid::new_v4().simple().to_string();
    };
    let now = super::accounts::now();
    map.retain(|_, entry| now.saturating_sub(entry.touched) < 3600);
    if let Some(entry) = map.get_mut(session) {
        entry.touched = now;
        return entry.id.clone();
    }
    if map.len() >= 10000 {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(key, _)| key.clone())
        {
            map.remove(&oldest);
        }
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    map.insert(
        session.to_owned(),
        Conversation {
            id: id.clone(),
            touched: now,
        },
    );
    id
}

fn require_leading_system(payload: &mut Value) -> Result<(), ChatError> {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return Err(ChatError::Request(
            "WorkBuddy requires a leading system message from the caller".into(),
        ));
    };
    if messages
        .first()
        .is_some_and(|message| message["role"] == "system")
    {
        return Ok(());
    }
    if let Some(index) = messages
        .iter()
        .position(|message| message["role"] == "system")
    {
        let system = messages.remove(index);
        messages.insert(0, system);
        return Ok(());
    }
    Err(ChatError::Request(
        "WorkBuddy requires a leading system message from the caller".into(),
    ))
}

/// Default neutral first line for a system message that opens like a third-party
/// agent prompt. Tencent's gateway rejects such openings with code 11128.
pub const DEFAULT_SYSTEM_LEAD: &str = "Follow the instructions below.";

/// The neutral first line, or `None` when normalization is turned off.
/// `ABV_WORKBUDDY_SYSTEM_LEAD` replaces the default; an empty value disables it.
fn system_lead() -> Option<String> {
    lead_from(std::env::var("ABV_WORKBUDDY_SYSTEM_LEAD").ok())
}
fn lead_from(value: Option<String>) -> Option<String> {
    match value {
        Some(value) if value.trim().is_empty() => None,
        Some(value) => Some(value.trim().to_owned()),
        None => Some(DEFAULT_SYSTEM_LEAD.to_owned()),
    }
}

/// Puts the neutral line in front of every system message whose opening looks like
/// a third-party agent prompt. Detection is broad, the change is not: one line is
/// added, nothing is removed or rewritten, and other messages stay byte-for-byte.
fn neutralize_system_openings(payload: &mut Value) {
    if let Some(lead) = system_lead() {
        neutralize_with(payload, &lead);
    }
}
fn neutralize_with(payload: &mut Value, lead: &str) {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    use crate::proxy::mappers::prompt_sanitizer::PromptSanitizer;
    for message in messages.iter_mut().filter(|m| m["role"] == "system") {
        match message.get_mut("content") {
            Some(Value::String(text)) => {
                if PromptSanitizer::opens_like_agent_prompt(text) {
                    *text = format!("{lead}\n\n{text}");
                }
            }
            Some(Value::Array(parts)) => {
                let opening = parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if PromptSanitizer::opens_like_agent_prompt(&opening) {
                    parts.insert(0, json!({"type":"text","text":lead}));
                }
            }
            _ => {}
        }
    }
}

/// The numeric result code in an upstream error body, for callers outside this module.
pub fn upstream_error_code(value: &Value) -> Option<i64> {
    error_code(value)
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
) -> Result<rquest::Response, ChatError> {
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
            return Err(ChatError::Request(
                "Unsupported WorkBuddy tool_choice".into(),
            ));
        }
    }
    if payload["tool_choice"] == "none" {
        let obj = payload
            .as_object_mut()
            .ok_or_else(|| ChatError::Request("Invalid request".into()))?;
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
    require_leading_system(&mut payload)?;
    neutralize_system_openings(&mut payload);
    let conversation =
        upstream_conversation_id(&format!("{}:{}", account.id, session_key(scope, &payload)));
    let mut h = headers(account.realm, Some(account)).map_err(ChatError::Account)?;
    add(&mut h, "accept", "application/json, text/event-stream").map_err(ChatError::Account)?;
    let product_version = if account.client.product_version.is_empty() {
        super::identity::PRODUCT_VERSION
    } else {
        account.client.product_version.as_str()
    };
    for (name, value) in [
        ("x-agent-purpose", "conversation"),
        ("x-agent-intent", "craft"),
        ("x-agent-type", "main"),
        ("x-private-data", "true"),
        ("x-ide-name", "WorkBuddy"),
        ("x-ide-type", "WorkBuddy"),
        ("x-ide-version", product_version),
        ("x-product", "WorkBuddy"),
    ] {
        add(&mut h, name, value).map_err(ChatError::Account)?;
    }
    // Official buildConversationHeaders shares one generated message/request ID.
    let message_id = uuid::Uuid::new_v4().simple().to_string();
    for (name, value) in [
        ("x-conversation-id", conversation),
        (
            "x-conversation-request-id",
            uuid::Uuid::new_v4().simple().to_string(),
        ),
        ("x-conversation-message-id", message_id.clone()),
        ("x-request-id", message_id),
        (
            "x-root-request-id",
            uuid::Uuid::new_v4().simple().to_string(),
        ),
        ("x-trace-id", uuid::Uuid::new_v4().simple().to_string()),
    ] {
        add(&mut h, name, &value).map_err(ChatError::Account)?;
    }
    if account.enterprise_id.is_empty() || account.realm == Realm::Global {
        add(&mut h, "x-no-enterprise-id", "1").map_err(ChatError::Account)?;
    } else {
        add(&mut h, "x-enterprise-id", &account.enterprise_id).map_err(ChatError::Account)?;
    }
    if account.realm == Realm::Global {
        add(&mut h, "x-domain", "www.workbuddy.ai").map_err(ChatError::Account)?;
    } else if !account.domain.is_empty() {
        add(&mut h, "x-domain", &account.domain).map_err(ChatError::Account)?;
    } else {
        add(&mut h, "x-no-department-info", "1").map_err(ChatError::Account)?;
    }
    if !account.device_token.is_empty() {
        add(&mut h, "x-device-token", &account.device_token).map_err(ChatError::Account)?;
    }
    let http = http_for(account).await.map_err(ChatError::Connection)?;
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(ChatError::Timeout(
            "WorkBuddy model request timed out".into(),
        ));
    }
    // A channel rejection stays on this account. Replaying the same body to
    // /console or to another account spreads one failure across the pool.
    let response = http
        .post(format!("{}/v2/chat/completions", base(account.realm)))
        .headers(h)
        .timeout(remaining)
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                ChatError::Timeout("WorkBuddy model request timed out".into())
            } else {
                ChatError::Connection("WorkBuddy model request failed".into())
            }
        })?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let response_headers = response.headers().clone();
    // An interrupted or oversized error body still came with a real status.
    // Keep that status and drop only the result code.
    let bytes = axum::body::to_bytes(
        axum::body::Body::from_stream(response.bytes_stream()),
        64 * 1024,
    )
    .await
    .unwrap_or_default();
    let code = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| error_code(&value));
    let mut rejected = axum::http::Response::builder()
        .status(status)
        // Preserve only the numeric result code for model-availability decisions.
        // Upstream messages may contain credential diagnostics and stay discarded.
        .body(
            code.map(|code| json!({"code":code}).to_string().into_bytes())
                .unwrap_or_default(),
        )
        .map_err(|_| ChatError::Connection("Invalid upstream response".into()))?;
    *rejected.headers_mut() = response_headers;
    rejected
        .headers_mut()
        .remove(rquest::header::CONTENT_LENGTH);
    rejected
        .headers_mut()
        .remove(rquest::header::CONTENT_ENCODING);
    Ok(rquest::Response::from(rejected))
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
    async fn workbuddy_conversation_identity_is_stable_within_an_account_and_isolated_between_accounts(
    ) {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let app = Router::new().route(
            "/v2/chat/completions",
            post(|headers: HeaderMap| async move {
                Json(
                    json!({"conversation":headers["x-conversation-id"].to_str().unwrap(),
                "message":headers["x-conversation-message-id"].to_str().unwrap(),
                "request":headers["x-request-id"].to_str().unwrap()}),
                )
            }),
        );
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        async fn send(account: &Account) -> Value {
            chat(account, "same-downstream-session",
                json!({"messages":[{"role":"system","content":"Use supplied context."},{"role":"user","content":"test input"}]}),
                "test-model", tokio::time::Instant::now() + Duration::from_secs(3))
                .await.unwrap().json().await.unwrap()
        }
        let a =
            super::super::accounts::parse_import(json!({"uid":"identity-a","accessToken":"test"}))
                .unwrap();
        let b =
            super::super::accounts::parse_import(json!({"uid":"identity-b","accessToken":"test"}))
                .unwrap();
        let first = send(&a).await;
        let repeated = send(&a).await;
        let other = send(&b).await;
        assert_eq!(first["conversation"], repeated["conversation"]);
        assert_ne!(first["conversation"], other["conversation"]);
        assert_eq!(first["message"], first["request"]);
        assert_ne!(first["request"], repeated["request"]);
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
            // Hot path only blocks refresh near expiry; keep this inside that window.
            "expiresAt":accounts::now()+10,
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
            json!({"messages":[{"role":"system","content":"stay"},{"role":"user","content":"hi"}]}),
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
            .route("/v2/plugin/auth/token",get(|| async { Json(json!({"code":0,"data":{"accessToken":"login-secret","refreshToken":"refresh-secret","expiresIn":10}})) }))
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
        let started = super::super::login::start(Realm::Cn, None).await.unwrap();
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
        // Reauthorizing one saved account refuses a sign-in by another account.
        let other = super::super::accounts::upsert(
            super::super::accounts::parse_import(json!({"uid":"other","accessToken":"secret"}))
                .unwrap(),
        )
        .unwrap();
        let reauth = super::super::login::start(Realm::Global, Some(&other.id))
            .await
            .unwrap();
        let refused = super::super::login::poll(&reauth.id).await.unwrap();
        assert_eq!(refused.status, "wrong_account");
        assert!(refused.account.is_none());
        assert_eq!(
            super::super::login::poll(&reauth.id).await.unwrap().status,
            "expired"
        );
        assert_eq!(super::super::accounts::get(&other.id).unwrap().uid, "other");
        // The same account reauthorizes in its own realm.
        let reauth = super::super::login::start(Realm::Global, Some(&id))
            .await
            .unwrap();
        let renewed = super::super::login::poll(&reauth.id).await.unwrap();
        assert_eq!(renewed.status, "ready");
        assert_eq!(renewed.account.unwrap().id, id);
        assert_eq!(super::super::accounts::list().unwrap().len(), 2);
        super::super::login::cancel(&started.id).await.unwrap();
        assert!(super::super::login::poll(&started.id).await.is_err());
    }
    #[tokio::test]
    async fn workbuddy_caller_system_is_kept_and_a_missing_one_is_not_invented() {
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
        let user_only = chat(
            &account,
            "test",
            json!({"messages":[{"role":"user","content":"original content"}]}),
            "user-only",
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await;
        assert!(matches!(user_only, Err(ChatError::Request(_))));
        for payload in [
            json!({"messages":[{"role":"system","content":"original content"}]}),
            json!({"messages":[{"role":"user","content":"later"},{"role":"developer","content":"original content"}]}),
        ] {
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
            assert_eq!(body["messages"][0]["content"], "original content");
            assert!(!body.to_string().contains("You are a helpful assistant."));
        }
    }

    #[tokio::test]
    async fn workbuddy_channel_rejection_stays_on_the_first_response() {
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
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    let ids = [
                        "x-conversation-id",
                        "x-conversation-request-id",
                        "x-conversation-message-id",
                        "x-request-id",
                        "x-root-request-id",
                        "x-trace-id",
                    ]
                    .map(|name| headers[name].to_str().unwrap().to_owned());
                    assert_eq!(
                        ids.iter().collect::<std::collections::HashSet<_>>().len(),
                        5
                    );
                    assert_eq!(ids[2], ids[3]);
                    assert_eq!(ids[0].len(), 32);
                    assert_eq!(
                        headers["accept"].to_str().unwrap(),
                        "application/json, text/event-stream"
                    );
                    assert!(headers["user-agent"]
                        .to_str()
                        .unwrap()
                        .starts_with("WorkBuddy/"));
                    assert!(!headers.contains_key("sec-ch-ua"));
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
        let payload = json!({"messages":[{"role":"system","content":"caller"},{"role":"user","content":"hi"}]});
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        assert_eq!(
            chat(&account, "channel-model", payload.clone(), "test", deadline)
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            chat(&account, "other-model", payload, "test", deadline)
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn workbuddy_oversized_error_body_keeps_the_upstream_status() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let app = Router::new().route(
            "/v2/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    "x".repeat(80 * 1024),
                )
            }),
        );
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let account =
            super::super::accounts::parse_import(json!({"uid":"test","accessToken":"secret"}))
                .unwrap();
        let payload = json!({"messages":[{"role":"system","content":"caller"},{"role":"user","content":"hi"}]});
        let response = chat(
            &account,
            "model",
            payload,
            "test",
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 429);
    }

    #[test]
    fn workbuddy_agent_openings_get_one_neutral_line_and_nothing_else_changes() {
        let claude_code = "You are Claude Code, Anthropic's official CLI for Claude.\nRules.";
        let mut payload = json!({"messages":[
            {"role":"system","content":claude_code},
            {"role":"system","content":[
                {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.3; cc_entrypoint=cli;"},
                {"type":"text","text":"You are Claude Code."},
            ]},
            {"role":"system","content":"Answer in Chinese."},
            {"role":"user","content":"You are great."},
        ]});
        let untouched = payload["messages"][2].clone();
        let user = payload["messages"][3].clone();
        neutralize_with(&mut payload, DEFAULT_SYSTEM_LEAD);
        assert_eq!(
            payload["messages"][0]["content"],
            format!("{DEFAULT_SYSTEM_LEAD}\n\n{claude_code}")
        );
        let parts = payload["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], json!({"type":"text","text":DEFAULT_SYSTEM_LEAD}));
        assert!(parts[1]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header"));
        // Non-matching system messages and every user message stay byte-for-byte.
        assert_eq!(payload["messages"][2], untouched);
        assert_eq!(payload["messages"][3], user);
        // The default applies when unset, a value replaces it, and empty turns it off.
        assert_eq!(lead_from(None).as_deref(), Some(DEFAULT_SYSTEM_LEAD));
        assert_eq!(
            lead_from(Some(" Custom. ".into())).as_deref(),
            Some("Custom.")
        );
        assert_eq!(lead_from(Some("  ".into())), None);
    }

    #[tokio::test]
    async fn workbuddy_upstream_receives_the_neutral_line_before_an_agent_opening() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let app = Router::new().route(
            "/v2/chat/completions",
            post(|Json(body): Json<Value>| async move { Json(body) }),
        );
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let account =
            super::super::accounts::parse_import(json!({"uid":"test","accessToken":"secret"}))
                .unwrap();
        let response = chat(
            &account,
            "model",
            json!({"messages":[
                {"role":"system","content":"You are opencode, an interactive CLI tool."},
                {"role":"user","content":"hi"},
            ]}),
            "test",
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        let body: Value = response.json().await.unwrap();
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with(&format!("{DEFAULT_SYSTEM_LEAD}\n\nYou are opencode")));
        assert_eq!(body["messages"][1]["content"], "hi");
    }
}
