//! Short-lived, administrator-initiated PKCE sessions for Qoder's device grant: the
//! administrator signs in on the official page while this service polls for the grant.
use super::{http, *};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use reqwest::{header::HeaderMap, Method};
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
    expires_at: i64,
    expected: Option<String>,
    completed: Option<String>,
}
struct Session {
    provider: Provider,
    pending: tokio::sync::Mutex<Pending>,
    cancelled: Mutex<bool>,
}
static SESSIONS: LazyLock<Mutex<HashMap<String, Arc<Session>>>> =
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
) -> Result<Started, String> {
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
    let expires_at = now() + TTL;
    let mut url = url::Url::parse(&format!("{}/device/selectAccounts", qoder::website(realm)))
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
    let pending = Pending {
        account: a,
        verifier,
        nonce,
        expires_at,
        expected: account_id.map(str::to_owned),
        completed: None,
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
                pending: tokio::sync::Mutex::new(pending),
                cancelled: Mutex::new(false),
            }),
        );
    }
    Ok(Started {
        id,
        auth_url: url.to_string(),
        expires_at,
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
    Ok(())
}

fn imported_account(provider: Provider, realm: Realm, value: &Value) -> Account {
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
    a
}

/// An imported access token is used as-is only while it is present and unexpired (with a
/// one-minute margin); otherwise any refresh/personal credential is exchanged first.
fn needs_refresh_before_import(a: &Account, at: i64) -> bool {
    let refreshable =
        !a.credentials.refresh_token.is_empty() || !a.credentials.personal_token.is_empty();
    a.credentials.access_token.is_empty()
        || (refreshable && a.expires_at > 0 && a.expires_at <= at + 60)
}

pub async fn import(provider: Provider, realm: Realm, value: Value) -> Result<AccountView, String> {
    let mut a = imported_account(provider, realm, &value);
    let refreshed = needs_refresh_before_import(&a, now());
    if refreshed {
        qoder::refresh(&mut a).await.map_err(|e| e.to_string())?;
    }
    if let Err(error) = qoder::identity(&mut a).await {
        // An export may carry an access token that was revoked or expired without a
        // recorded expiry; retry once with the refresh credential before giving up.
        let refreshable =
            !a.credentials.refresh_token.is_empty() || !a.credentials.personal_token.is_empty();
        if refreshed || !refreshable || !matches!(error.status, 401 | 403) {
            return Err(error.to_string());
        }
        qoder::refresh(&mut a).await.map_err(|e| e.to_string())?;
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
    #[test]
    fn imported_device_identity_is_kept() {
        let account = imported_account(
            Provider::Qoder,
            Realm::Cn,
            &serde_json::json!({"auth":{"accessToken":"token","machineId":"existing-machine","deviceId":"existing-device","expiresAt":1800000000000_i64}}),
        );
        assert_eq!(account.credentials.access_token, "token");
        assert_eq!(account.credentials.machine_id, "existing-machine");
        assert_eq!(account.credentials.device_id, "existing-device");
        assert_eq!(account.expires_at, 1800000000);
    }
    #[test]
    fn expired_imported_tokens_are_refreshed_before_identity_validation() {
        let at = 1_800_000_000;
        let mut a = imported_account(
            Provider::Qoder,
            Realm::Cn,
            &serde_json::json!({"accessToken":"old","refreshToken":"refresh","expiresAt":at - 10}),
        );
        assert!(needs_refresh_before_import(&a, at));
        a.expires_at = at + 30; // within the one-minute margin
        assert!(needs_refresh_before_import(&a, at));
        a.expires_at = at + 3600;
        assert!(!needs_refresh_before_import(&a, at));
        a.expires_at = 0; // unknown expiry: try the supplied token first
        assert!(!needs_refresh_before_import(&a, at));
        // Without a refresh credential an expired token is still tried as-is.
        a.expires_at = at - 10;
        a.credentials.refresh_token.clear();
        assert!(!needs_refresh_before_import(&a, at));
        a.credentials.access_token.clear();
        assert!(needs_refresh_before_import(&a, at));
    }
    #[tokio::test]
    async fn cancel_does_not_wait_for_an_in_flight_poll() {
        let started = start(Provider::Qoder, Realm::Cn, None).await.unwrap();
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
