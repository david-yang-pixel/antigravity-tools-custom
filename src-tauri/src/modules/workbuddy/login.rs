//! Only administrator-initiated, short-lived authorizations can import credentials.
use super::{
    accounts::{self, AccountView, Realm},
    client,
};
use reqwest::Method;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

struct Session {
    state: String,
    realm: Realm,
    expires_at: i64,
    completed: Option<String>,
    cancelled: bool,
}
static SESSIONS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Session>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[derive(Serialize)]
pub struct Started {
    pub id: String,
    pub auth_url: String,
    pub expires_at: i64,
}
#[derive(Serialize)]
pub struct Poll {
    pub status: &'static str,
    pub account: Option<AccountView>,
}

fn valid_auth_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.host_str().is_some_and(|host| {
            [
                "codebuddy.cn",
                "workbuddy.cn",
                "workbuddy.ai",
                "tencent.com",
            ]
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
        })
}
pub async fn start(realm: Realm) -> Result<Started, String> {
    let data = client::call(
        realm,
        Method::POST,
        "/v2/plugin/auth/state",
        None,
        &[("platform", "CLI")],
        Some(json!({})),
    )
    .await?;
    let state = data
        .get("state")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 4096)
        .ok_or("WorkBuddy returned no authorization state")?;
    let auth_url = data
        .get("authUrl")
        .and_then(Value::as_str)
        .filter(|s| valid_auth_url(s))
        .ok_or("WorkBuddy returned an invalid authorization URL")?
        .to_owned();
    let id = uuid::Uuid::new_v4().to_string();
    let expires_at = accounts::now() + 600;
    let mut sessions = SESSIONS
        .lock()
        .map_err(|_| "WorkBuddy authorization unavailable")?;
    sessions.retain(|_, session| {
        session
            .try_lock()
            .map(|s| !s.cancelled && s.expires_at > accounts::now())
            .unwrap_or(true)
    });
    if sessions.len() >= 32 {
        return Err(
            "Too many pending WorkBuddy authorizations; cancel an existing one or wait for expiry"
                .into(),
        );
    }
    sessions.insert(
        id.clone(),
        Arc::new(tokio::sync::Mutex::new(Session {
            state: state.into(),
            realm,
            expires_at,
            completed: None,
            cancelled: false,
        })),
    );
    Ok(Started {
        id,
        auth_url,
        expires_at,
    })
}
fn session(id: &str) -> Result<Arc<tokio::sync::Mutex<Session>>, String> {
    SESSIONS
        .lock()
        .map_err(|_| "WorkBuddy authorization unavailable")?
        .get(id)
        .cloned()
        .ok_or_else(|| "WorkBuddy authorization expired; start again".into())
}
pub async fn cancel(id: &str) -> Result<(), String> {
    let entry = SESSIONS
        .lock()
        .map_err(|_| "WorkBuddy authorization unavailable")?
        .remove(id);
    if let Some(entry) = entry {
        entry.lock().await.cancelled = true;
    }
    Ok(())
}
pub async fn poll(id: &str) -> Result<Poll, String> {
    let entry = session(id)?;
    // Serializes simultaneous polls; completed state makes network retries idempotent.
    let mut session = entry.lock().await;
    if session.cancelled || session.expires_at <= accounts::now() {
        return Ok(Poll {
            status: "expired",
            account: None,
        });
    }
    if let Some(id) = &session.completed {
        return Ok(Poll {
            status: "ready",
            account: accounts::list()?.into_iter().find(|a| &a.id == id),
        });
    }
    let auth = match client::call(
        session.realm,
        Method::GET,
        "/v2/plugin/auth/token",
        None,
        &[("state", &session.state)],
        None,
    )
    .await
    {
        Ok(auth) => auth,
        Err(error) if error.starts_with("WorkBuddy rejected") => {
            return Ok(Poll {
                status: "waiting",
                account: None,
            })
        }
        Err(error) => return Err(error),
    };
    let Some(access) = auth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(Poll {
            status: "waiting",
            account: None,
        });
    };
    let mut headers = client::headers(session.realm, None)?;
    headers.insert(
        "authorization",
        format!("Bearer {access}")
            .parse()
            .map_err(|_| "Invalid WorkBuddy authorization token")?,
    );
    let response = client::http()?
        .get(format!(
            "{}/v2/plugin/login/account",
            client::base(session.realm)
        ))
        .headers(headers)
        .query(&[("state", &session.state)])
        .timeout(std::time::Duration::from_secs(25))
        .send()
        .await
        .map_err(|_| "Cannot retrieve WorkBuddy account identity")?;
    let identity = client::json_response(response).await?;
    let uid = identity
        .get("uid")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .ok_or("WorkBuddy returned no account identity")?;
    let mut value = json!({ "account": identity, "auth": auth });
    value["account"]["uid"] = json!(uid);
    value["auth"]["realm"] = json!(session.realm);
    value["auth"]["expiresAt"] = json!(
        accounts::now()
            + value["auth"]["expiresIn"]
                .as_i64()
                .unwrap_or(3600)
                .clamp(1, 365 * 86400)
    );
    // Cancellation removes the map entry before waiting on this poll's lock.
    let sessions = SESSIONS
        .lock()
        .map_err(|_| "WorkBuddy authorization unavailable")?;
    if !sessions.contains_key(id) || session.expires_at <= accounts::now() {
        return Ok(Poll {
            status: "expired",
            account: None,
        });
    }
    let account = accounts::upsert(accounts::parse_import(value)?)?;
    session.completed = Some(account.id.clone());
    // Keep a completed session until TTL so a lost HTTP response cannot lose the result.
    Ok(Poll {
        status: "ready",
        account: Some(account),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorization_urls_are_official_https_only() {
        assert!(valid_auth_url("https://www.codebuddy.cn/login?state=test"));
        for url in [
            "http://codebuddy.cn",
            "https://codebuddy.cn.evil.test",
            "https://evil.test/codebuddy.cn",
            "https://user@codebuddy.cn",
            "javascript:alert(1)",
        ] {
            assert!(!valid_auth_url(url));
        }
    }
}
