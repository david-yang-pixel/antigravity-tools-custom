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

/// A ChatGPT import: a bare refresh token, an object carrying one, a sub2api export account
/// (`credentials`), or a Codex `auth.json` (`tokens`). Access and id tokens, the account id
/// and the expiry are kept when present, so a still-valid export need not be refreshed.
/// ChatGPT accounts always live in the global realm, whatever the request says.
fn imported_chatgpt(value: &Value) -> Result<Account, String> {
    let mut a = Account::new(Provider::ChatGpt, Realm::Global);
    match value {
        Value::String(token) => a.credentials.refresh_token = token.trim().to_owned(),
        Value::Object(_) => {
            let root = ["credentials", "tokens"]
                .iter()
                .find_map(|key| value.get(*key).filter(|v| v.is_object()))
                .unwrap_or(value);
            let field = |keys: &[&str]| text(root, keys).trim().to_owned();
            let c = &mut a.credentials;
            c.refresh_token = field(&["refresh_token", "refreshToken", "rt"]);
            c.access_token = field(&["access_token", "accessToken"]);
            c.id_token = field(&["id_token", "idToken"]);
            c.chatgpt_account_id = field(&[
                "chatgpt_account_id",
                "chatgptAccountId",
                "account_id",
                "accountId",
            ]);
            a.expires_at = root
                .get("expires_at")
                .or_else(|| root.get("expiresAt"))
                .and_then(epoch)
                .unwrap_or(0);
        }
        _ => {}
    }
    if a.credentials.refresh_token.is_empty() && a.credentials.access_token.is_empty() {
        return Err("Paste a ChatGPT refresh token, a {\"refresh_token\": \"...\"} object, a sub2api export or a Codex auth.json".into());
    }
    Ok(a)
}

/// One account of a multi-account export.
pub(super) struct BatchEntry {
    pub label: String,
    pub value: Value,
    /// Why this entry is not imported, e.g. an account of another platform.
    pub skip: Option<String>,
}

/// The accounts of an export such as sub2api's `{"accounts": [...]}`; `None` when the value
/// is a single account.
pub(super) fn batch_entries(value: &Value) -> Option<Vec<BatchEntry>> {
    let accounts = value.get("accounts")?.as_array()?;
    Some(
        accounts
            .iter()
            .enumerate()
            .map(|(index, account)| {
                let credentials = account.get("credentials").unwrap_or(&Value::Null);
                let label = [
                    text(account, &["name", "email"]),
                    text(credentials, &["email"]),
                ]
                .into_iter()
                .find(|label| !label.is_empty())
                .unwrap_or_else(|| format!("#{}", index + 1));
                let platform = text(account, &["platform"]);
                let skip = (!platform.is_empty() && platform != "openai")
                    .then(|| format!("Skipped: {platform} account, not ChatGPT"));
                BatchEntry {
                    label,
                    value: account.clone(),
                    skip,
                }
            })
            .collect(),
    )
}

/// The outcome of one account of a batch import.
#[derive(Clone, serde::Serialize)]
pub struct BatchItem {
    pub label: String,
    /// `imported`, `skipped` or `failed`.
    pub status: String,
    pub message: String,
    pub account: Option<AccountView>,
}

/// Imports every account of an export one after another; one failure never stops the rest.
pub async fn import_batch(
    provider: Provider,
    realm: Realm,
    value: Value,
) -> Result<Vec<BatchItem>, String> {
    if provider != Provider::ChatGpt {
        return Err("Batch import is available for ChatGPT accounts".into());
    }
    let entries = batch_entries(&value)
        .ok_or("Expected an export with an \"accounts\" list; import a single account instead")?;
    let mut report = Vec::with_capacity(entries.len());
    for entry in entries {
        let item = |status: &str, message: String, account: Option<AccountView>| BatchItem {
            label: entry.label.clone(),
            status: status.into(),
            message,
            account,
        };
        report.push(match &entry.skip {
            Some(reason) => item("skipped", reason.clone(), None),
            None => match import(provider, realm, entry.value.clone(), None).await {
                Ok(account) => item("imported", String::new(), Some(account)),
                Err(error) => item("failed", error, None),
            },
        });
    }
    Ok(report)
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

/// Validates a ChatGPT import; returns whether the refresh token was exchanged. An exported
/// access token that is valid for more than five minutes is kept and proven with one catalog
/// request, so the export's refresh token is not spent; an expired, unusable or rejected one
/// falls back to the refresh token.
async fn authorize_chatgpt(
    client: &reqwest::Client,
    a: &mut Account,
) -> Result<bool, http::Failure> {
    let has_refresh = !a.credentials.refresh_token.is_empty();
    if !a.credentials.access_token.is_empty() && chatgpt::identity::apply(a).is_ok() {
        if a.expires_at > now() + 300 {
            match chatgpt::models::fetch(client, a).await {
                Ok(_) => return Ok(false),
                Err(error) if !has_refresh || !matches!(error.status, 401 | 403) => {
                    return Err(error)
                }
                Err(_) => {}
            }
        } else if !has_refresh {
            return Err(http::Failure::new(
                401,
                "This ChatGPT access token has expired and the import carries no refresh token",
            ));
        }
    }
    if !has_refresh {
        return Err(http::Failure::new(
            400,
            "This import needs a ChatGPT refresh token or an access token with a chatgpt_account_id",
        ));
    }
    chatgpt::auth::refresh(client, a).await?;
    Ok(true)
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
    let mut refreshed = false;
    match provider {
        Provider::Qoder => {
            let client = http::login_client(provider, &route.exit).map_err(|e| e.to_string())?;
            authorize_qoder(&client, &mut a)
                .await
                .map_err(|e| e.to_string())?;
        }
        Provider::ChatGpt => {
            let client =
                http::plain_login_client(provider, &route.exit).map_err(|e| e.to_string())?;
            refreshed = authorize_chatgpt(&client, &mut a)
                .await
                .map_err(|e| e.to_string())?;
            // From a token exchange on, the pasted refresh token is spent.
            if refreshed {
                chatgpt::identity::apply(&mut a)
                    .map_err(|e| salvage_chatgpt(&a, &pasted, e.to_string()))?;
            }
        }
    }
    let a = match provider {
        Provider::Qoder => save_import(
            provider,
            a,
            &value,
            account_id,
            route.node.as_deref(),
            &pasted,
        )?,
        Provider::ChatGpt => save_import(
            provider,
            a.clone(),
            &value,
            account_id,
            route.node.as_deref(),
            &pasted,
        )
        .map_err(|error| {
            if refreshed {
                salvage_chatgpt(&a, &pasted, error)
            } else {
                error
            }
        })?,
    };
    drop(route);
    let id = a.id.clone();
    tokio::spawn(async move {
        let _ = accounts::sync(provider, &id, false).await;
    });
    Ok((&a).into())
}

/// After a ChatGPT token exchange the pasted refresh token is spent. When the import then
/// fails, the rotated credentials are kept on the saved account the new tokens identify
/// (under its revision, egress binding unchanged); without one, the administrator learns
/// that a new Codex sign-in is needed.
fn salvage_chatgpt(a: &Account, pasted: &str, error: String) -> String {
    let owner = store::all(Provider::ChatGpt).ok().and_then(|all| {
        all.into_iter()
            .find(|saved| !a.uid.is_empty() && saved.uid == a.uid && saved.realm == a.realm)
    });
    if let Some(owner) = owner {
        let saved = store::update(Provider::ChatGpt, &owner.id, Some(&owner.revision), |s| {
            s.credentials.access_token = a.credentials.access_token.clone();
            s.credentials.refresh_token = a.credentials.refresh_token.clone();
            s.credentials.id_token = a.credentials.id_token.clone();
            s.credentials.chatgpt_account_id = a.credentials.chatgpt_account_id.clone();
            s.expires_at = a.expires_at;
            // These are new credentials: like any new authorization they invalidate older
            // operations in flight and revive an account its dead credential turned off,
            // never one the administrator turned off.
            s.revision = uuid::Uuid::new_v4().to_string();
            s.enabled = s.enabled || s.credential_dead;
            s.credential_dead = false;
            s.auth_error = None;
            if !pasted.is_empty() {
                store::add_fingerprints(
                    &mut s.credentials.imported_refresh_sha256,
                    [fingerprint(pasted)],
                );
            }
        });
        if saved.is_ok() {
            let name = if owner.nickname.is_empty() {
                &owner.uid
            } else {
                &owner.nickname
            };
            return format!(
                "This token belongs to ChatGPT account {name}; its credentials were updated there"
            );
        }
    }
    format!(
        "{error}. The pasted refresh token was consumed; sign in to Codex again to get a new one"
    )
}

/// Validates an authorized import against its row, its egress binding and the vault, then
/// saves it on `node`.
fn save_import(
    provider: Provider,
    mut a: Account,
    value: &Value,
    account_id: Option<&str>,
    node: Option<&str>,
    pasted: &str,
) -> Result<Account, String> {
    if let Some(id) = account_id {
        let previous = store::get(provider, id)?;
        if previous.uid != a.uid || previous.realm != a.realm {
            return Err(format!(
                "This token belongs to a different {} account",
                provider.name()
            ));
        }
    }
    if bound_elsewhere(provider, &a, node)? {
        return Err(match provider {
            Provider::Qoder => BOUND_ELSEWHERE,
            Provider::ChatGpt => CHATGPT_BOUND_ELSEWHERE,
        }
        .into());
    }
    let root = value.get("auth").filter(|v| v.is_object()).unwrap_or(value);
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
            .push(fingerprint(pasted));
    }
    let a = store::upsert(a, account_id)?;
    egress::adopt(Pool::Native(provider), &a.id, node);
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chatgpt_imports_read_every_known_credential_shape() {
        // A sub2api export account, its bare credentials object, and a Codex auth.json.
        let credentials = json!({"access_token": "at", "id_token": "it", "refresh_token": "rt",
            "chatgpt_account_id": "acc", "expires_at": "2026-11-16T02:49:36.000Z"});
        for value in [
            json!({"name": "me@example.com", "platform": "openai", "credentials": credentials}),
            credentials.clone(),
            json!({"tokens": {"access_token": "at", "id_token": "it", "refresh_token": "rt",
                "account_id": "acc"}}),
        ] {
            let a = imported_chatgpt(&value).unwrap_or_else(|e| panic!("{value}: {e}"));
            assert_eq!(a.realm, Realm::Global);
            assert_eq!(a.credentials.access_token, "at", "{value}");
            assert_eq!(a.credentials.id_token, "it", "{value}");
            assert_eq!(a.credentials.refresh_token, "rt", "{value}");
            assert_eq!(a.credentials.chatgpt_account_id, "acc", "{value}");
        }
        let exported = imported_chatgpt(&credentials).unwrap();
        assert_eq!(exported.expires_at, 1_794_797_376);
        // The original shapes still work: a bare refresh token, or an object carrying one.
        for value in [
            json!("rt"),
            json!({"refresh_token": "rt"}),
            json!({"rt": "rt"}),
        ] {
            let a = imported_chatgpt(&value).unwrap();
            assert_eq!(a.credentials.refresh_token, "rt");
            assert!(a.credentials.access_token.is_empty());
        }
        for value in [json!(""), json!({}), json!({"credentials": {}}), json!(42)] {
            assert!(imported_chatgpt(&value).is_err(), "{value}");
        }
    }

    #[test]
    fn chatgpt_batch_entries_come_from_an_export_and_skip_other_platforms() {
        let export = json!({"exported_at": "2026-11-07T14:47:05Z", "accounts": [
            {"name": "a@example.com", "platform": "openai", "credentials": {"refresh_token": "ra"}},
            {"name": "claude@example.com", "platform": "anthropic", "credentials": {"refresh_token": "rc"}},
            {"credentials": {"email": "b@example.com", "refresh_token": "rb"}},
        ]});
        let entries = batch_entries(&export).expect("an export is a batch");
        let labels: Vec<_> = entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            ["a@example.com", "claude@example.com", "b@example.com"]
        );
        assert!(entries[0].skip.is_none() && entries[2].skip.is_none());
        assert!(entries[1].skip.as_deref().unwrap().contains("anthropic"));
        // A single account is not a batch.
        assert!(batch_entries(&json!({"refresh_token": "r"})).is_none());
        assert!(batch_entries(&json!("r")).is_none());
        assert!(batch_entries(&json!({"accounts": []})).is_some_and(|e| e.is_empty()));
    }

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
