//! Short-lived, administrator-initiated PKCE sessions for Qoder's device grant: the
//! administrator signs in on the official page while this service polls for the grant.
use super::{http, *};
use crate::modules::egress::{self, LoginHold, Pool};
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
    /// Egress node the whole authorization uses; the account inherits it.
    node: Option<String>,
    /// Keeps that node off the pool's free list until the account is bound or the
    /// session ends.
    hold: Option<LoginHold>,
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
/// Drops cancelled and expired sessions, which also releases their node holds. A
/// session locked by an in-flight poll is kept until that poll finishes.
fn purge(sessions: &mut HashMap<String, Arc<Session>>) {
    sessions.retain(|_, session| {
        session.pending.try_lock().map_or(true, |p| {
            p.expires_at > now() && session.cancelled.lock().is_ok_and(|c| !*c)
        })
    });
}
/// A saved account keeps its node. A token for it issued through another address would
/// tie that address to the account's identity, so such a sign-in is refused.
fn bound_elsewhere(provider: Provider, a: &Account, node: Option<&str>) -> Result<bool, String> {
    let Some(previous) = store::all(provider)?
        .into_iter()
        .find(|p| p.uid == a.uid && p.realm == a.realm)
    else {
        return Ok(false);
    };
    Ok(egress::bound_node(Pool::Native(provider), &previous.id)?
        .is_some_and(|bound| node != Some(bound.as_str())))
}
const BOUND_ELSEWHERE: &str =
    "This account is bound to another egress node; reauthorize it from its own row";
/// ChatGPT has no browser reauthorization: its row re-imports a token through its node.
const CHATGPT_BOUND_ELSEWHERE: &str =
    "This ChatGPT account is bound to another egress node; re-import its token from the account's own row";
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
    // Only Qoder has a device grant; ChatGPT accounts are imported from a refresh token.
    match provider {
        Provider::Qoder => {}
        Provider::ChatGpt => {
            return Err("ChatGPT accounts are added by importing a refresh token".into())
        }
    }
    let mut a = Account::new(provider, realm);
    let mut preferred = None;
    if let Some(id) = account_id {
        let previous = store::get(provider, id)?;
        if previous.realm != realm {
            return Err("Reauthorization must use the account's region".into());
        }
        a.credentials = Credentials {
            access_token: String::new(),
            refresh_token: String::new(),
            personal_token: String::new(),
            ..previous.credentials
        };
        // A saved account reauthorizes through its own node.
        preferred = egress::bound_node(Pool::Native(provider), id)?;
    }
    // Release abandoned sessions before choosing a node, or their holds hide it.
    if let Ok(mut sessions) = SESSIONS.lock() {
        purge(&mut sessions);
    }
    let mut route = egress::login_route(Pool::Native(provider), preferred.as_deref()).await?;
    let node = route.node.clone();
    let mut hold = route.hold.take();
    drop(route);
    let id = uuid::Uuid::new_v4().to_string();
    let (verifier, challenge) = pkce();
    let nonce = if realm == Realm::Cn {
        uuid::Uuid::new_v4().to_string()
    } else {
        uuid::Uuid::new_v4().simple().to_string()
    };
    let expires_at = now() + TTL;
    if let Some(hold) = hold.as_mut() {
        hold.hold_until(expires_at);
    }
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
        node,
        hold,
    };
    {
        let mut sessions = SESSIONS.lock().map_err(|_| "Login sessions unavailable")?;
        purge(&mut sessions);
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
    let route = egress::login_route(Pool::Native(provider), p.node.as_deref()).await?;
    let client = http::login_client(provider, &route.exit).map_err(|e| e.to_string())?;
    let result = http::json(&client, provider, Method::GET, url.as_str(), headers, None).await;
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
    qoder::identity(&client, &mut p.account)
        .await
        .map_err(|e| e.to_string())?;
    drop(route);
    // Cancellation can run while the provider request is in flight. Serialize its flag
    // with this final commit, rather than making Cancel wait for the network poll.
    let cancelled = session
        .cancelled
        .lock()
        .map_err(|_| "Login cancellation unavailable")?;
    if *cancelled || p.expires_at <= now() {
        return Err("Login cancelled or expired".into());
    }
    if bound_elsewhere(provider, &p.account, p.node.as_deref())? {
        drop(cancelled);
        *session
            .cancelled
            .lock()
            .map_err(|_| "Login cancellation unavailable")? = true;
        if let Some(hold) = p.hold.as_mut() {
            hold.release();
        }
        return Err(BOUND_ELSEWHERE.into());
    }
    let account = store::upsert(p.account.clone(), p.expected.as_deref())?;
    // Bind before the first sync, or auto-assignment would pick another node.
    egress::adopt(Pool::Native(provider), &account.id, p.node.as_deref());
    if let Some(hold) = p.hold.as_mut() {
        hold.release();
    }
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
    // Free the node now unless a poll holds the session; purge or the hold's own
    // deadline frees it otherwise.
    if let Ok(mut p) = session.pending.try_lock() {
        if let Some(hold) = p.hold.as_mut() {
            hold.release();
        }
    }
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

/// A ChatGPT import is one refresh token: a bare JSON string, or an object carrying it.
/// ChatGPT accounts always live in the global realm, whatever the request says.
fn imported_chatgpt(value: &Value) -> Result<Account, String> {
    let token = match value {
        Value::String(token) => token.trim().to_owned(),
        Value::Object(_) => text(value, &["refresh_token", "refreshToken", "rt"])
            .trim()
            .to_owned(),
        _ => String::new(),
    };
    if token.is_empty() {
        return Err(
            "Paste a ChatGPT refresh token, as a string or as {\"refresh_token\": \"...\"}".into(),
        );
    }
    let mut a = Account::new(Provider::ChatGpt, Realm::Global);
    a.credentials.refresh_token = token;
    Ok(a)
}

/// An imported access token is used as-is only while it is present and unexpired (with a
/// one-minute margin); otherwise any refresh/personal credential is exchanged first.
fn needs_refresh_before_import(a: &Account, at: i64) -> bool {
    let refreshable =
        !a.credentials.refresh_token.is_empty() || !a.credentials.personal_token.is_empty();
    a.credentials.access_token.is_empty()
        || (refreshable && a.expires_at > 0 && a.expires_at <= at + 60)
}

/// Hex SHA-256 of a pasted refresh token; the token itself is never kept for this.
fn fingerprint(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Imports use a new login route, not an existing account's refresh transaction.
/// Reject known credentials before any network I/O: rotating one and then failing
/// identity validation or the egress check would strand the saved refresh token.
/// Check all saved regions, including when the administrator selected the wrong one.
fn reject_saved_credentials(a: &Account, saved: &[Account]) -> Result<(), String> {
    let supplied = &a.credentials;
    // A rotated ChatGPT refresh token is no longer saved verbatim; its import fingerprint
    // still is.
    let pasted = match a.provider {
        Provider::Qoder => None,
        Provider::ChatGpt => {
            (!supplied.refresh_token.is_empty()).then(|| fingerprint(&supplied.refresh_token))
        }
    };
    if saved.iter().any(|previous| {
        let credentials = &previous.credentials;
        [
            (&supplied.access_token, &credentials.access_token),
            (&supplied.refresh_token, &credentials.refresh_token),
            (&supplied.personal_token, &credentials.personal_token),
        ]
        .into_iter()
        .any(|(supplied, saved)| !supplied.is_empty() && supplied == saved)
            || pasted
                .as_ref()
                .is_some_and(|digest| credentials.imported_refresh_sha256.contains(digest))
    }) {
        return Err("This credential is already saved; refresh or reauthorize the account from its own row instead of importing it again".into());
    }
    Ok(())
}

/// Validates an imported Qoder export and fills its identity.
async fn authorize_qoder(
    client: &qoder_client::transport::Client,
    a: &mut Account,
) -> Result<(), http::Failure> {
    let refreshed = needs_refresh_before_import(a, now());
    if refreshed {
        qoder::refresh(client, a).await?;
    }
    if let Err(error) = qoder::identity(client, a).await {
        // An export may carry an access token that was revoked or expired without a
        // recorded expiry; retry once with the refresh credential before giving up.
        let refreshable =
            !a.credentials.refresh_token.is_empty() || !a.credentials.personal_token.is_empty();
        if refreshed || !refreshable || !matches!(error.status, 401 | 403) {
            return Err(error);
        }
        qoder::refresh(client, a).await?;
        qoder::identity(client, a).await?;
    }
    Ok(())
}

pub async fn import(
    provider: Provider,
    realm: Realm,
    value: Value,
    account_id: Option<&str>,
) -> Result<AccountView, String> {
    // Only ChatGPT re-imports onto a row; Qoder rows reauthorize through the device grant.
    if provider == Provider::Qoder && account_id.is_some() {
        return Err("Qoder accounts reauthorize from their row's browser login".into());
    }
    let mut a = match provider {
        Provider::Qoder => imported_account(provider, realm, &value),
        Provider::ChatGpt => imported_chatgpt(&value)?,
    };
    let pasted = a.credentials.refresh_token.clone();
    reject_saved_credentials(&a, &store::all(provider)?)?;
    // A row re-import goes through that account's own node, like a browser
    // reauthorization. A new account is validated through the node it would take,
    // and keeps that node.
    let preferred = match account_id {
        Some(id) => {
            store::get(provider, id)?;
            egress::bound_node(Pool::Native(provider), id)?
        }
        None => None,
    };
    let route = egress::login_route(Pool::Native(provider), preferred.as_deref()).await?;
    // Another import may have saved these credentials while we waited for the
    // serialized login route. Recheck under that route's guard before using them.
    reject_saved_credentials(&a, &store::all(provider)?)?;
    match provider {
        Provider::Qoder => {
            let client = http::login_client(provider, &route.exit).map_err(|e| e.to_string())?;
            authorize_qoder(&client, &mut a)
                .await
                .map_err(|e| e.to_string())?;
        }
        Provider::ChatGpt => {
            // The refresh token is the whole import: always exchange it, then require the
            // workspace identity the new tokens carry.
            let client =
                http::plain_login_client(provider, &route.exit).map_err(|e| e.to_string())?;
            chatgpt::auth::refresh(&client, &mut a)
                .await
                .map_err(|e| e.to_string())?;
            chatgpt::identity::apply(&mut a).map_err(|e| e.to_string())?;
        }
    }
    if let Some(id) = account_id {
        let previous = store::get(provider, id)?;
        if previous.uid != a.uid || previous.realm != a.realm {
            return Err(format!(
                "This token belongs to a different {} account",
                provider.name()
            ));
        }
    }
    if bound_elsewhere(provider, &a, route.node.as_deref())? {
        return Err(match provider {
            Provider::Qoder => BOUND_ELSEWHERE,
            Provider::ChatGpt => CHATGPT_BOUND_ELSEWHERE,
        }
        .into());
    }
    let root = value
        .get("auth")
        .filter(|v| v.is_object())
        .unwrap_or(&value);
    // Only Qoder accounts carry a device identity worth keeping across imports.
    let previous = match provider {
        Provider::Qoder => store::all(provider)?
            .into_iter()
            .find(|previous| previous.uid == a.uid && previous.realm == a.realm),
        Provider::ChatGpt => None,
    };
    if let Some(previous) = previous {
        if text(root, &["machineId", "machine_id"]).is_empty() {
            a.credentials.machine_id = previous.credentials.machine_id;
        }
        if text(root, &["deviceId", "device_id"]).is_empty() {
            a.credentials.device_id = previous.credentials.device_id;
        }
    }
    if provider == Provider::ChatGpt && !pasted.is_empty() {
        a.credentials
            .imported_refresh_sha256
            .push(fingerprint(&pasted));
    }
    let a = store::upsert(a, account_id)?;
    egress::adopt(Pool::Native(provider), &a.id, route.node.as_deref());
    drop(route);
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
    #[test]
    fn saved_tokens_are_rejected_individually_before_import() {
        let at = now();
        let mut previous = Account::new(Provider::Qoder, Realm::Cn);
        previous.credentials.access_token = "saved-access".into();
        previous.credentials.refresh_token = "saved-refresh".into();
        previous.credentials.personal_token = "saved-personal".into();
        let saved = [previous];
        for auth in [
            serde_json::json!({"accessToken":"saved-access"}),
            serde_json::json!({"accessToken":"expired","refreshToken":"saved-refresh","expiresAt":at - 1}),
            serde_json::json!({"personalToken":"saved-personal"}),
        ] {
            for realm in [Realm::Cn, Realm::Global] {
                let a = imported_account(Provider::Qoder, realm, &auth);
                let error = reject_saved_credentials(&a, &saved).unwrap_err();
                assert!(error.contains("already saved"));
                assert!(error.contains("reauthorize"));
                assert!(!error.contains("saved-refresh"));
            }
        }
    }
    #[test]
    fn empty_tokens_and_device_identifiers_do_not_mark_an_import_as_saved() {
        let mut previous = Account::new(Provider::Qoder, Realm::Cn);
        previous.credentials.access_token = "saved-access".into();
        let a = imported_account(
            Provider::Qoder,
            Realm::Cn,
            &serde_json::json!({
                "accessToken":"new-access",
                "machineId":previous.credentials.machine_id,
                "deviceId":previous.credentials.device_id
            }),
        );
        assert!(reject_saved_credentials(&a, &[previous]).is_ok());
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
