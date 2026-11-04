//! Short-lived, administrator-initiated PKCE sessions. Qoder polls its device grant;
//! TRAE accepts an exact loopback callback, received beside the official browser.
use super::{http, *};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use reqwest::{header::HeaderMap, Method};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

const TTL: i64 = 600;
struct Pending {
    account: Account,
    verifier: String,
    nonce: String,
    trace: String,
    expires_at: i64,
    expected: Option<String>,
    completed: Option<String>,
    callback_url: Option<String>,
    callback_error: Option<String>,
}
struct Session {
    provider: Provider,
    server_browser: bool,
    expires_at: i64,
    pending: tokio::sync::Mutex<Pending>,
    cancelled: Mutex<bool>,
}
static SESSIONS: LazyLock<Mutex<HashMap<String, Arc<Session>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LISTENER: tokio::sync::Mutex<bool> = tokio::sync::Mutex::const_new(false);

#[derive(Serialize)]
pub struct Started {
    pub id: String,
    pub auth_url: String,
    pub expires_at: i64,
    pub manual_callback: bool,
    pub server_browser: bool,
}
#[derive(Serialize)]
pub struct Poll {
    pub status: &'static str,
    pub account: Option<AccountView>,
}

pub fn pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}
fn session(id: &str, provider: Provider) -> Result<Arc<Session>, String> {
    let sessions = SESSIONS.lock().map_err(|_| "Login sessions unavailable")?;
    let session = sessions
        .get(id)
        .cloned()
        .ok_or("Login session not found or expired")?;
    if session.provider != provider {
        return Err("Login belongs to another provider".into());
    }
    Ok(session)
}
fn valid(p: &Pending, session: &Session) -> Result<(), String> {
    if *session
        .cancelled
        .lock()
        .map_err(|_| "Login cancellation unavailable")?
        || p.expires_at <= now()
    {
        return Err("Login cancelled or expired; start a new authorization".into());
    }
    Ok(())
}
pub async fn start(
    provider: Provider,
    realm: Realm,
    account_id: Option<&str>,
    solo: bool,
) -> Result<Started, String> {
    start_with_browser(provider, realm, account_id, solo, false).await
}

pub async fn start_with_browser(
    provider: Provider,
    realm: Realm,
    account_id: Option<&str>,
    solo: bool,
    server_browser: bool,
) -> Result<Started, String> {
    if server_browser && provider != Provider::Trae {
        return Err("Only TRAE uses a server login browser".into());
    }
    let mut a = Account::new(provider, realm);
    if let Some(id) = account_id {
        let previous = store::get(provider, id)?;
        if previous.realm != realm {
            return Err("Reauthorization must use the account's region".into());
        }
        a.credentials.machine_id = previous.credentials.machine_id;
        a.credentials.device_id = previous.credentials.device_id;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let (verifier, challenge) = pkce();
    let nonce = if realm == Realm::Cn {
        uuid::Uuid::new_v4().to_string()
    } else {
        uuid::Uuid::new_v4().simple().to_string()
    };
    let trace = uuid::Uuid::new_v4().to_string();
    let expires_at = now() + TTL;
    let mut callback_url = None;
    let (auth_url, manual_callback) = match provider {
        Provider::Qoder => {
            let mut url =
                url::Url::parse(&format!("{}/device/selectAccounts", qoder::website(realm)))
                    .map_err(|_| "Invalid Qoder authorization URL")?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("challenge", &challenge)
                    .append_pair("challenge_method", "S256")
                    .append_pair("nonce", &nonce)
                    .append_pair("client_id", qoder::client_id(realm))
                    .append_pair("machine_id", &a.credentials.machine_id);
                if realm == Realm::Cn {
                    q.append_pair("redirect_uri", "qoder-work-cn://");
                }
            }
            a.credentials.client_id = qoder::client_id(realm).into();
            (url.to_string(), false)
        }
        Provider::Trae => {
            a.credentials.client_id = trae::client_id(solo).into();
            a.credentials.platform = format!(
                "{}{}",
                if realm == Realm::Cn { "cn" } else { "global" },
                if solo { "-solo" } else { "" }
            );
            a.credentials.auth_host = trae::auth_hosts(realm)[0].into();
            a.credentials.api_host = trae::core_host(realm, "").into();
            let mut login_host = if realm == Realm::Cn {
                "https://www.trae.cn"
            } else {
                "https://www.trae.ai"
            }
            .to_string();
            for host in trae::auth_hosts(realm).iter().take(2) {
                if let Ok(value) = trae::post(
                    &a,
                    host,
                    "/cloudide/api/v3/trae/GetLoginGuidance",
                    json!({"loginTraceID":trace,"login_trace_id":trace}),
                )
                .await
                {
                    let host = text(
                        payload(&value),
                        &["LoginHost", "loginHost", "LoginURL", "loginUrl"],
                    );
                    if !host.is_empty() {
                        login_host = if host.starts_with("https://") {
                            host
                        } else {
                            format!("https://{host}")
                        };
                        http::official_url(provider, &login_host).map_err(|e| e.to_string())?;
                        break;
                    }
                }
            }
            let mut url = http::official_url(provider, &login_host).map_err(|e| e.to_string())?;
            url.set_path("/authorization");
            url.set_query(None);
            url.set_fragment(None);
            let target = super::callback::target(server_browser)?;
            callback_url = Some(target.clone());
            {
                let mut q = url.query_pairs_mut();
                for (key, value) in [
                    ("login_version", "1"),
                    ("auth_from", if solo { "solo" } else { "trae" }),
                    ("login_channel", "native_ide"),
                    ("plugin_version", "local"),
                    ("auth_type", "local"),
                    ("client_id", &a.credentials.client_id),
                    ("redirect", "0"),
                    ("login_trace_id", &trace),
                    ("auth_callback_url", &target),
                    ("machine_id", &a.credentials.machine_id),
                    ("device_id", &a.credentials.device_id),
                    ("x_device_id", &a.credentials.device_id),
                    ("x_machine_id", &a.credentials.machine_id),
                    ("x_device_brand", "PC"),
                    ("x_device_type", std::env::consts::OS),
                    ("x_os_version", std::env::consts::OS),
                    ("x_app_type", "stable"),
                    ("code_challenge", &challenge),
                    ("code_challenge_method", "S256"),
                ] {
                    q.append_pair(key, value);
                }
                q.append_pair("x_app_version", &trae::version());
                if solo {
                    q.append_pair("hide_saas_login", "true");
                }
            }
            (url.to_string(), !server_browser && !start_listener().await)
        }
    };
    let pending = Pending {
        account: a,
        verifier,
        nonce,
        trace: trace.clone(),
        expires_at,
        expected: account_id.map(str::to_owned),
        completed: None,
        callback_url: callback_url.clone(),
        callback_error: None,
    };
    {
        let mut sessions = SESSIONS.lock().map_err(|_| "Login sessions unavailable")?;
        sessions.retain(|_, session| {
            session.pending.try_lock().map_or(true, |p| {
                p.expires_at > now() && session.cancelled.lock().is_ok_and(|c| !*c)
            })
        });
        if sessions.len() >= 20 {
            return Err("Too many pending authorizations; cancel one or wait for expiry".into());
        }
        sessions.insert(
            id.clone(),
            Arc::new(Session {
                provider,
                server_browser,
                expires_at,
                pending: tokio::sync::Mutex::new(pending),
                cancelled: Mutex::new(false),
            }),
        );
    }
    if server_browser {
        if let Err(error) = super::browser::start(
            &id,
            &auth_url,
            callback_url.as_deref().ok_or("Missing login callback")?,
            &trace,
            expires_at,
        )
        .await
        {
            let _ = cancel(provider, &id).await;
            return Err(error);
        }
    }
    Ok(Started {
        id,
        auth_url,
        expires_at,
        manual_callback,
        server_browser,
    })
}

pub async fn poll(provider: Provider, id: &str) -> Result<Poll, String> {
    let session = session(id, provider)?;
    let mut p = session.pending.lock().await;
    valid(&p, &session)?;
    if let Some(id) = p.completed.as_deref() {
        return Ok(Poll {
            status: "authorized",
            account: Some((&store::get(provider, id)?).into()),
        });
    }
    if provider == Provider::Trae {
        if let Some(error) = &p.callback_error {
            return Err(error.clone());
        }
        if session.server_browser && !super::browser::running(id) {
            return Err("TRAE_BROWSER_STOPPED".into());
        }
        return Ok(Poll {
            status: "pending",
            account: None,
        });
    }
    let mut url = url::Url::parse(&format!(
        "{}/api/v1/deviceToken/poll",
        qoder::openapi(p.account.realm)
    ))
    .map_err(|_| "Invalid Qoder poll URL")?;
    url.query_pairs_mut()
        .append_pair("nonce", &p.nonce)
        .append_pair("verifier", &p.verifier)
        .append_pair("challenge_method", "S256");
    let mut headers = HeaderMap::new();
    http::insert(&mut headers, "user-agent", "QoderWork").map_err(|e| e.to_string())?;
    let result = http::json(provider, Method::GET, url.as_str(), headers, None).await;
    let value = match result {
        Ok(value) => value,
        Err(e) if e.status == 202 || e.status == 404 => {
            return Ok(Poll {
                status: "pending",
                account: None,
            })
        }
        Err(error) => return Err(error.to_string()),
    };
    if text(payload(&value), &["token", "device_token", "access_token"]).is_empty() {
        return Ok(Poll {
            status: "pending",
            account: None,
        });
    }
    qoder::apply_tokens(&mut p.account, &value).map_err(|e| e.to_string())?;
    qoder::identity(&mut p.account)
        .await
        .map_err(|e| e.to_string())?;
    // Cancellation can run while the provider request is in flight. Serialize its flag
    // with this final commit, rather than making Cancel wait for the network poll.
    let cancelled = session
        .cancelled
        .lock()
        .map_err(|_| "Login cancellation unavailable")?;
    if *cancelled || p.expires_at <= now() {
        return Err("Login cancelled or expired".into());
    }
    let account = store::upsert(p.account.clone(), p.expected.as_deref())?;
    p.completed = Some(account.id.clone());
    drop(cancelled);
    let id = account.id.clone();
    tokio::spawn(async move {
        let _ = accounts::sync(provider, &id, false).await;
    });
    Ok(Poll {
        status: "authorized",
        account: Some((&account).into()),
    })
}

pub async fn cancel(provider: Provider, id: &str) -> Result<(), String> {
    let session = session(id, provider)?;
    *session
        .cancelled
        .lock()
        .map_err(|_| "Login cancellation unavailable")? = true;
    if session.server_browser {
        super::browser::close(id);
    }
    Ok(())
}

/// Do not wait for Pending: it may be exchanging a one-time grant over the network.
pub fn browser_session(id: &str) -> Result<(), String> {
    let session = session(id, Provider::Trae)?;
    if !session.server_browser
        || session.expires_at <= now()
        || *session
            .cancelled
            .lock()
            .map_err(|_| "Login cancellation unavailable")?
    {
        return Err("Login browser cancelled, expired or unavailable".into());
    }
    Ok(())
}

pub async fn complete(provider: Provider, id: &str, callback: &str) -> Result<AccountView, String> {
    // A browser may stop reading as soon as it receives a response or closes its
    // tab. Keep a one-time grant exchange alive through validated persistence.
    let id = id.to_owned();
    let callback = callback.to_owned();
    tokio::spawn(async move {
        let result = complete_owned(provider, &id, &callback).await;
        if provider == Provider::Trae {
            if let (Err(error), Ok(session)) = (&result, session(&id, provider)) {
                let mut p = session.pending.lock().await;
                if p.completed.is_none() && valid(&p, &session).is_ok() {
                    p.callback_error = Some(error.clone());
                }
            }
        }
        result
    })
    .await
    .map_err(|_| "TRAE authorization task failed".to_string())?
}

async fn complete_owned(
    provider: Provider,
    id: &str,
    callback: &str,
) -> Result<AccountView, String> {
    if provider != Provider::Trae {
        return Err("Only TRAE uses browser callbacks".into());
    }
    let url = url::Url::parse(callback).map_err(|_| "Paste the complete browser callback URL")?;
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let session = session(id, provider)?;
    let mut p = session.pending.lock().await;
    valid(&p, &session)?;
    let expected = p
        .callback_url
        .as_deref()
        .ok_or("This login has no browser callback")?;
    let expected = url::Url::parse(expected).map_err(|_| "Invalid login callback")?;
    if url.scheme() != expected.scheme()
        || url.host_str() != expected.host_str()
        || url.port_or_known_default() != expected.port_or_known_default()
        || url.path() != expected.path()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("The callback URL does not match this authorization session".into());
    }
    if let Some(id) = p.completed.as_deref() {
        return Ok((&store::get(provider, id)?).into());
    }
    validate_callback(&query, id, &p.trace)?;
    p.callback_error = None;
    let mut a = p.account.clone();
    apply_callback_host(&mut a, &query)?;
    let region = query
        .get("userRegion")
        .or_else(|| query.get("user_region"))
        .map_or("", String::as_str)
        .to_ascii_lowercase();
    a.credentials.api_host = trae::core_host(a.realm, &region).into();
    let code = callback_code(&query)?;
    if !code.is_empty() {
        trae::exchange_code(
            &mut a,
            &code,
            &p.verifier,
            query.get("userTag").map_or("", String::as_str),
        )
        .await
        .map_err(|e| e.to_string())?;
    } else if let Some(tokens) = callback_tokens(&query)? {
        let tokens = payload(&tokens);
        let refresh = text(tokens, &["RefreshToken", "refreshToken", "refresh_token"]);
        if !refresh.is_empty() {
            a.credentials.refresh_token = refresh;
        }
        if text(
            tokens,
            &[
                "Token",
                "AccessToken",
                "accessToken",
                "access_token",
                "token",
            ],
        )
        .is_empty()
        {
            trae::refresh(&mut a).await.map_err(|e| e.to_string())?;
        } else {
            trae::apply_tokens(&mut a, tokens).map_err(|e| e.to_string())?;
        }
    } else {
        return Err(
            "TRAE callback contains no authorization grant; start a new browser authorization"
                .into(),
        );
    }
    trae::identity(&mut a).await.map_err(|e| e.to_string())?;
    let cancelled = session
        .cancelled
        .lock()
        .map_err(|_| "Login cancellation unavailable")?;
    if *cancelled || p.expires_at <= now() {
        return Err("Login cancelled or expired".into());
    }
    let a = store::upsert(a, p.expected.as_deref())?;
    p.completed = Some(a.id.clone());
    p.callback_error = None;
    drop(cancelled);
    let id = a.id.clone();
    tokio::spawn(async move {
        let _ = accounts::sync(provider, &id, false).await;
    });
    Ok((&a).into())
}

/// Shared by the private browser pipe and desktop loopback listener. Never derive
/// the credential destination from an incoming Host/Forwarded header.
pub async fn receive_callback(id: &str, query: &str) -> Result<(), String> {
    let session = session(id, Provider::Trae)?;
    let callback = {
        let p = session.pending.lock().await;
        valid(&p, &session)?;
        // The fixed /authorize loopback path cannot carry our nonce. Bind every
        // delivered grant to the provider's trace instead.
        let pairs: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
        let traces: Vec<_> = pairs
            .iter()
            .filter(|(key, _)| key == "loginTraceID" || key == "login_trace_id")
            .collect();
        if traces.is_empty() || traces.iter().any(|(_, trace)| trace.as_ref() != p.trace) {
            return Err("Authorization trace mismatch".into());
        }
        p.callback_url
            .clone()
            .ok_or("This login has no browser callback")?
    };
    let mut url = url::Url::parse(&callback).map_err(|_| "Invalid login callback")?;
    url.set_query(Some(query));
    complete(Provider::Trae, id, url.as_str()).await.map(|_| ())
}

pub async fn receive_loopback(query: &str) -> Result<(), String> {
    let trace = url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "loginTraceID" || key == "login_trace_id")
        .map(|(_, value)| value.into_owned())
        .ok_or("Missing authorization trace")?;
    // Snapshot under the map lock; never hold it while waiting for a session or
    // provider request. Server-owned browsers have their own private receivers.
    let sessions: Vec<_> = SESSIONS
        .lock()
        .map_err(|_| "Login sessions unavailable")?
        .iter()
        .filter(|(_, session)| session.provider == Provider::Trae && !session.server_browser)
        .map(|(id, session)| (id.clone(), session.clone()))
        .collect();
    for (id, session) in sessions {
        let p = session.pending.lock().await;
        if p.trace == trace {
            valid(&p, &session)?;
            drop(p);
            return receive_callback(&id, query).await;
        }
    }
    Err("Login session not found or expired".into())
}

fn apply_callback_host(a: &mut Account, query: &HashMap<String, String>) -> Result<(), String> {
    if let Some(host) = query.get("host").filter(|s| !s.is_empty()) {
        let raw = if host.contains("://") {
            host.clone()
        } else {
            format!("https://{host}")
        };
        let url = http::official_url(Provider::Trae, &raw).map_err(|e| e.to_string())?;
        if url.path() != "/" || url.query().is_some() {
            return Err("TRAE callback returned an invalid account service host".into());
        }
        a.credentials.auth_host = url.origin().ascii_serialization();
    }
    Ok(())
}

fn callback_tokens(query: &HashMap<String, String>) -> Result<Option<Value>, String> {
    for key in ["userJwt", "user_jwt", "UserJwt"] {
        if let Some(raw) = query.get(key) {
            let value: Value = serde_json::from_str(raw)
                .map_err(|_| "Invalid TRAE credential callback envelope")?;
            if !value.is_object() {
                return Err("Invalid TRAE credential callback envelope".into());
            }
            if text(
                payload(&value),
                &[
                    "Token",
                    "AccessToken",
                    "accessToken",
                    "access_token",
                    "token",
                    "RefreshToken",
                    "refreshToken",
                    "refresh_token",
                ],
            )
            .is_empty()
            {
                return Err("TRAE credential callback contains no token".into());
            }
            return Ok(Some(value));
        }
    }
    let token = text(
        &json!(query),
        &["accessToken", "access_token", "Token", "token"],
    );
    let refresh = text(
        &json!(query),
        &["refreshToken", "refresh_token", "RefreshToken"],
    );
    if token.is_empty() && refresh.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        json!({"Token":token,"RefreshToken":refresh,"expiresAt":query.get("expiresAt")}),
    ))
}
pub fn validate_callback(
    query: &HashMap<String, String>,
    id: &str,
    trace: &str,
) -> Result<(), String> {
    if query.get("state").is_some_and(|value| value != id) {
        return Err("Authorization state mismatch".into());
    }
    if query
        .get("login_trace_id")
        .or_else(|| query.get("loginTraceID"))
        .is_some_and(|value| value != trace)
    {
        return Err("Authorization trace mismatch".into());
    }
    Ok(())
}
fn callback_code(query: &HashMap<String, String>) -> Result<String, String> {
    for key in [
        "authCode",
        "auth_code",
        "AuthCode",
        "authorization_code",
        "code",
    ] {
        if let Some(code) = query.get(key).filter(|code| !code.is_empty()) {
            return Ok(code.clone());
        }
    }
    for key in ["authCodeInfo", "auth_code_info", "AuthCodeInfo"] {
        if let Some(raw) = query.get(key) {
            let info: Value = serde_json::from_str(raw)
                .map_err(|_| "Invalid TRAE authorization code envelope")?;
            if ["ExpireAt", "expireAt", "expiresAt"]
                .iter()
                .find_map(|key| info.get(*key).and_then(epoch))
                .is_some_and(|expires| expires <= now())
            {
                return Err("TRAE authorization code has expired".into());
            }
            return Ok(text(&info, &["AuthCode", "authCode", "auth_code", "code"]));
        }
    }
    Ok(String::new())
}

async fn start_listener() -> bool {
    let mut started = LISTENER.lock().await;
    if *started {
        return true;
    }
    let Ok(listener) = tokio::net::TcpListener::bind((
        std::net::Ipv4Addr::LOCALHOST,
        super::callback::loopback_port(),
    ))
    .await
    else {
        return false;
    };
    let router = super::callback::loopback_routes();
    *started = true;
    tokio::spawn(async move {
        if axum::serve(listener, router).await.is_err() {
            *LISTENER.lock().await = false;
        }
    });
    true
}

fn imported_account(provider: Provider, realm: Realm, value: &Value) -> Result<Account, String> {
    let root = value.get("auth").filter(|v| v.is_object()).unwrap_or(value);
    let mut a = Account::new(provider, realm);
    a.credentials.access_token = text(root, &["accessToken", "access_token", "token", "Token"]);
    a.credentials.refresh_token = text(root, &["refreshToken", "refresh_token", "RefreshToken"]);
    a.credentials.personal_token = text(root, &["personalToken", "personal_token", "pat"]);
    let machine = text(root, &["machineId", "machine_id"]);
    let device = text(root, &["deviceId", "device_id"]);
    if !machine.is_empty() {
        a.credentials.machine_id = machine;
    }
    if !device.is_empty() {
        a.credentials.device_id = device;
    }
    a.expires_at = root
        .get("expiresAt")
        .or_else(|| root.get("expires_at"))
        .and_then(epoch)
        .unwrap_or(0);
    if provider == Provider::Trae {
        a.credentials.platform = text(root, &["platform"]);
        let client = text(root, &["clientId", "client_id"]);
        a.credentials.client_id = if client.is_empty() {
            trae::client_id(a.credentials.platform.ends_with("solo")).into()
        } else {
            client
        };
        let auth_host = text(root, &["apiHost", "authHost", "auth_host"]);
        a.credentials.auth_host = if auth_host.is_empty() {
            trae::auth_hosts(realm)[0].into()
        } else {
            http::official_url(provider, &auth_host).map_err(|e| e.to_string())?;
            auth_host.trim_end_matches('/').into()
        };
        let host = text(root, &["apiBaseUrl", "api_base_url"]);
        a.credentials.api_host = if host.is_empty() {
            trae::core_host(realm, &text(root, &["loginRegion", "login_region"])).into()
        } else {
            http::official_url(provider, &host).map_err(|e| e.to_string())?;
            host.trim_end_matches('/').into()
        };
    }
    Ok(a)
}

pub async fn import(provider: Provider, realm: Realm, value: Value) -> Result<AccountView, String> {
    let mut a = imported_account(provider, realm, &value)?;
    if provider == Provider::Trae {
        if a.credentials.access_token.is_empty() {
            trae::refresh(&mut a).await.map_err(|e| e.to_string())?;
        }
        trae::identity(&mut a).await.map_err(|e| e.to_string())?;
    } else {
        if a.credentials.access_token.is_empty() {
            qoder::refresh(&mut a).await.map_err(|e| e.to_string())?;
        }
        qoder::identity(&mut a).await.map_err(|e| e.to_string())?;
    }
    let root = value
        .get("auth")
        .filter(|v| v.is_object())
        .unwrap_or(&value);
    if let Some(previous) = store::all(provider)?
        .into_iter()
        .find(|previous| previous.uid == a.uid && previous.realm == a.realm)
    {
        if text(root, &["machineId", "machine_id"]).is_empty() {
            a.credentials.machine_id = previous.credentials.machine_id;
        }
        if text(root, &["deviceId", "device_id"]).is_empty() {
            a.credentials.device_id = previous.credentials.device_id;
        }
    }
    let a = store::upsert(a, None)?;
    let id = a.id.clone();
    tokio::spawn(async move {
        let _ = accounts::sync(provider, &id, false).await;
    });
    Ok((&a).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct CallbackFixture {
        id: String,
        session: Arc<Session>,
        trace: String,
    }
    impl CallbackFixture {
        fn new() -> Self {
            Self::with_browser(false)
        }
        fn with_browser(server_browser: bool) -> Self {
            let id = uuid::Uuid::new_v4().to_string();
            let trace = format!("test-{id}");
            let session = Arc::new(Session {
                provider: Provider::Trae,
                server_browser,
                expires_at: now() + TTL,
                pending: tokio::sync::Mutex::new(Pending {
                    account: Account::new(Provider::Trae, Realm::Cn),
                    verifier: "private-test-verifier".into(),
                    nonce: "private-test-nonce".into(),
                    trace: trace.clone(),
                    expires_at: now() + TTL,
                    expected: None,
                    completed: None,
                    callback_url: Some("http://127.0.0.1:18080/authorize".into()),
                    callback_error: None,
                }),
                cancelled: Mutex::new(false),
            });
            SESSIONS.lock().unwrap().insert(id.clone(), session.clone());
            Self { id, session, trace }
        }
        fn path(&self) -> String {
            "/authorize".into()
        }
    }
    impl Drop for CallbackFixture {
        fn drop(&mut self) {
            SESSIONS.lock().unwrap().remove(&self.id);
        }
    }

    #[tokio::test]
    async fn browser_control_requires_its_live_session_without_waiting_for_grant_exchange() {
        let legacy = CallbackFixture::new();
        assert!(browser_session(&legacy.id).is_err());
        let fixture = CallbackFixture::with_browser(true);
        let _grant_in_progress = fixture.session.pending.lock().await;
        assert!(browser_session(&fixture.id).is_ok());
        cancel(Provider::Trae, &fixture.id).await.unwrap();
        assert!(browser_session(&fixture.id).is_err());
        assert!(browser_session(&uuid::Uuid::new_v4().to_string()).is_err());
    }

    #[test]
    fn trae_callback_accepts_direct_grants_without_inventing_credentials() {
        let query = HashMap::from([("userJwt".into(), json!({"Token":"test-token","RefreshToken":"test-refresh","TokenExpireAt":1800000000000_i64}).to_string())]);
        let tokens = callback_tokens(&query).unwrap().unwrap();
        let mut a = Account::new(Provider::Trae, Realm::Cn);
        trae::apply_tokens(&mut a, &tokens).unwrap();
        assert_eq!(a.credentials.access_token, "test-token");
        assert_eq!(a.credentials.refresh_token, "test-refresh");
        assert_eq!(a.expires_at, 1800000000);
        assert!(callback_tokens(&HashMap::new()).unwrap().is_none());
        assert!(callback_tokens(&HashMap::from([(
            "userJwt".into(),
            "{not-json-secret".into()
        )]))
        .is_err());
        assert!(callback_tokens(&HashMap::from([("userJwt".into(), "{}".into())])).is_err());
        assert!(callback_tokens(&HashMap::from([(
            "refreshToken".into(),
            "test-refresh".into()
        )]))
        .unwrap()
        .is_some());
    }

    #[test]
    fn callback_account_host_remains_separate_and_must_be_a_provider_service() {
        let mut a = Account::new(Provider::Trae, Realm::Cn);
        a.credentials.api_host = "https://trae-api-cn.mchost.guru".into();
        apply_callback_host(
            &mut a,
            &HashMap::from([("host".into(), "console.enterprise.trae.cn".into())]),
        )
        .unwrap();
        assert_eq!(
            a.credentials.auth_host,
            "https://console.enterprise.trae.cn"
        );
        assert_eq!(a.credentials.api_host, "https://trae-api-cn.mchost.guru");
        for host in [
            "https://attacker.example",
            "http://api.trae.cn",
            "https://api.trae.cn/path",
            "https://api.trae.cn?token=x",
        ] {
            assert!(
                apply_callback_host(&mut a, &HashMap::from([("host".into(), host.into())]))
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn browser_callback_preflight_is_scoped_and_bad_grants_never_commit() {
        use axum::{
            body::{to_bytes, Body},
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;
        let fixture = CallbackFixture::new();
        let app = super::super::callback::loopback_routes();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri(fixture.path())
                    .header("origin", "https://www.trae.cn")
                    .header("access-control-request-method", "GET")
                    .header("access-control-request-headers", "content-type")
                    .header("access-control-request-private-network", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "https://www.trae.cn"
        );
        assert_eq!(
            response.headers()["access-control-allow-credentials"],
            "true"
        );
        assert_eq!(
            response.headers()["access-control-allow-private-network"],
            "true"
        );
        assert_eq!(
            poll(Provider::Trae, &fixture.id).await.unwrap().status,
            "pending"
        );
        let response = app
            .clone()
            .oneshot(
                Request::get(fixture.path())
                    .header("origin", "https://attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response
            .headers()
            .contains_key("access-control-allow-origin"));
        let response = app
            .clone()
            .oneshot(
                Request::get(format!(
                    "{}?loginTraceID={}&userJwt=not-json-secret",
                    fixture.path(),
                    fixture.trace
                ))
                .header("origin", "https://www.trae.cn")
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("not-json-secret"));
        assert!(poll(Provider::Trae, &fixture.id).await.is_err());
        let pending = fixture.session.pending.lock().await;
        assert!(pending.completed.is_none());
        assert!(pending.account.credentials.access_token.is_empty());
        drop(pending);
        cancel(Provider::Trae, &fixture.id).await.unwrap();
        let response = app
            .oneshot(
                Request::get(fixture.path())
                    .header("origin", "https://www.trae.cn")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    #[tokio::test]
    async fn grants_remain_bound_to_the_live_session() {
        let fixture = CallbackFixture::with_browser(true);
        assert!(receive_callback(&fixture.id, "loginTraceID=wrong-trace")
            .await
            .is_err());
        assert!(receive_callback(
            &fixture.id,
            &format!("loginTraceID={}&login_trace_id=wrong-trace", fixture.trace)
        )
        .await
        .is_err());
        assert!(fixture
            .session
            .pending
            .lock()
            .await
            .callback_error
            .is_none());
        cancel(Provider::Trae, &fixture.id).await.unwrap();
        assert!(
            receive_callback(&fixture.id, &format!("loginTraceID={}", fixture.trace))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn exact_loopback_path_selects_only_the_matching_local_session() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;
        let fixture = CallbackFixture::new();
        let trace = format!("desktop-{}", fixture.id);
        {
            let mut p = fixture.session.pending.lock().await;
            p.trace = trace.clone();
            p.callback_url = Some("http://127.0.0.1:18080/authorize".into());
        }
        let app = super::super::callback::loopback_routes();
        let response = app
            .clone()
            .oneshot(
                Request::get("/authorize?userJwt=bad")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(fixture
            .session
            .pending
            .lock()
            .await
            .callback_error
            .is_none());
        let server_browser = CallbackFixture::with_browser(true);
        assert!(
            receive_loopback(&format!("loginTraceID={}", server_browser.trace))
                .await
                .is_err()
        );
        let response = app
            .oneshot(
                Request::get(format!("/authorize?loginTraceID={trace}&userJwt=bad"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(fixture
            .session
            .pending
            .lock()
            .await
            .callback_error
            .is_some());
        assert!(fixture.session.pending.lock().await.completed.is_none());
    }
    #[test]
    fn imported_device_and_oauth_host_are_kept_separate_from_inference_host() {
        let account = imported_account(Provider::Trae, Realm::Global, &json!({"auth":{"accessToken":"token","machineId":"existing-machine","deviceId":"existing-device","clientId":"existing-client","apiHost":"https://grow-normal.traeapi.us","apiBaseUrl":"https://coreva-normal.trae.ai","expiresAt":1800000000000_i64}})).unwrap();
        assert_eq!(account.credentials.machine_id, "existing-machine");
        assert_eq!(account.credentials.device_id, "existing-device");
        assert_eq!(account.credentials.client_id, "existing-client");
        assert_eq!(
            account.credentials.auth_host,
            "https://grow-normal.traeapi.us"
        );
        assert_eq!(
            account.credentials.api_host,
            "https://coreva-normal.trae.ai"
        );
        assert_eq!(account.expires_at, 1800000000);
        assert!(imported_account(
            Provider::Trae,
            Realm::Cn,
            &json!({"apiBaseUrl":"https://untrusted.example"})
        )
        .is_err());
    }
    #[tokio::test]
    async fn cancel_does_not_wait_for_an_in_flight_poll() {
        let started = start(Provider::Qoder, Realm::Cn, None, false)
            .await
            .unwrap();
        let session = session(&started.id, Provider::Qoder).unwrap();
        let pending = session.pending.lock().await;
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            cancel(Provider::Qoder, &started.id),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(valid(&pending, &session).is_err());
        drop(pending);
        assert!(poll(Provider::Qoder, &started.id).await.is_err());
    }
}
