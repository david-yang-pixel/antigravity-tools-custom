//! Administrator-initiated imports of native provider accounts (ChatGPT): a pasted token
//! or export is validated with the official service through the account's egress route,
//! then saved.
use super::{http, *};
use crate::modules::egress::{self, Pool};
use sha2::{Digest, Sha256};

/// A saved account keeps its node. A token for it issued through another address would
/// tie that address to the account's identity, so such an import is refused.
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
/// A row re-import keeps the same node as its authorization and inference traffic.
const CHATGPT_BOUND_ELSEWHERE: &str =
    "This ChatGPT account is bound to another egress node; re-import its token from the account's own row";

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
    a.login_credentials = super::reauthorize::from_json(value)?;
    a.reauthorization.authorization = super::reauthorize::service_from_json(value)?;
    a.reauthorization.auto = a.login_credentials.is_some();
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
pub async fn import_batch(provider: Provider, value: Value) -> Result<Vec<BatchItem>, String> {
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
            None => match import(provider, entry.value.clone(), None).await {
                Ok(account) => item("imported", String::new(), Some(account)),
                Err(error) => item("failed", error, None),
            },
        });
    }
    Ok(report)
}

/// Hex SHA-256 of a pasted refresh token; the token itself is never kept for this.
fn fingerprint(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Imports use a new login route, not an existing account's refresh transaction.
/// Reject known credentials before any network I/O: rotating one and then failing
/// identity validation or the egress check would strand the saved refresh token.
fn reject_saved_credentials(a: &Account, saved: &[Account]) -> Result<(), String> {
    let supplied = &a.credentials;
    // A rotated refresh token is no longer saved verbatim; its import fingerprint still is.
    let pasted = (!supplied.refresh_token.is_empty()).then(|| fingerprint(&supplied.refresh_token));
    if saved.iter().any(|previous| {
        let credentials = &previous.credentials;
        [
            (&supplied.access_token, &credentials.access_token),
            (&supplied.refresh_token, &credentials.refresh_token),
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

/// Nodes a new account's import tries when a node cannot reach the upstream.
const IMPORT_NODES: usize = 3;

pub async fn import(
    provider: Provider,
    value: Value,
    account_id: Option<&str>,
) -> Result<AccountView, String> {
    import_checked(provider, value, account_id, None).await
}
pub(super) async fn import_checked(
    provider: Provider,
    value: Value,
    account_id: Option<&str>,
    revision: Option<&str>,
) -> Result<AccountView, String> {
    import_inner(provider, value, account_id, revision, None, None).await
}
pub(super) async fn import_new(
    provider: Provider,
    value: Value,
    auto: bool,
    id: &str,
    revision: &str,
    node: Option<&str>,
) -> Result<AccountView, String> {
    import_inner(
        provider,
        value,
        None,
        None,
        Some((auto, id, revision)),
        node,
    )
    .await
}
async fn import_inner(
    provider: Provider,
    value: Value,
    account_id: Option<&str>,
    revision: Option<&str>,
    new_login_auto: Option<(bool, &str, &str)>,
    new_login_node: Option<&str>,
) -> Result<AccountView, String> {
    let mut a = match provider {
        Provider::ChatGpt => imported_chatgpt(&value)?,
    };
    if let Some((auto, _, _)) = new_login_auto {
        a.reauthorization.auto = auto;
    }
    if let Some((_, id, revision)) = new_login_auto {
        a.reauthorization.protocol =
            super::login_imports::job_state(provider, id, revision)?.protocol;
    }
    let pasted = a.credentials.refresh_token.clone();
    reject_saved_credentials(&a, &store::all(provider)?)?;
    // Direct OAuth does not select the account's node. New accounts obtain an
    // available node here, and saved accounts keep their binding.
    let preferred = match account_id {
        Some(id) => {
            store::get(provider, id)?;
            egress::bound_node(Pool::Native(provider), id)?
        }
        None => new_login_node.map(str::to_owned),
    };
    // A new account's auto-chosen node that cannot reach the upstream is reported and
    // the import moves to another node. Only connection failures move: nothing was sent,
    // so the pasted credentials are untouched.
    let mut attempt = 1;
    let (route, refreshed) = loop {
        let route = egress::login_route(Pool::Native(provider), preferred.as_deref()).await?;
        // Another import may have saved these credentials while we waited for the
        // serialized login route. Recheck under that route's guard before using them.
        reject_saved_credentials(&a, &store::all(provider)?)?;
        let client = http::login_client(provider, &route.exit).map_err(|e| e.to_string())?;
        match authorize_chatgpt(&client, &mut a).await {
            Ok(refreshed) => break (route, refreshed),
            Err(error) if error.unreached && route.hold.is_some() && attempt < IMPORT_NODES => {
                if let Some(node) = route.node.as_deref() {
                    tracing::warn!(
                        "{} import could not reach the upstream through node {node}; trying another node",
                        provider.name()
                    );
                    egress::report_unreachable(Pool::Native(provider), node);
                }
                attempt += 1;
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    // From a token exchange on, the pasted refresh token is spent.
    if refreshed {
        chatgpt::identity::apply(&mut a).map_err(|e| {
            if revision.is_some() || new_login_auto.is_some() {
                e.to_string()
            } else {
                salvage_chatgpt(&a, &pasted, e.to_string())
            }
        })?;
    }
    let a = save_import(
        provider,
        a.clone(),
        account_id,
        route.node.as_deref(),
        &pasted,
        revision,
        new_login_auto.map(|(_, id, revision)| (id, revision)),
    )
    .map_err(|error| {
        if refreshed && revision.is_none() && new_login_auto.is_none() {
            salvage_chatgpt(&a, &pasted, error)
        } else {
            error
        }
    })?;
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
    account_id: Option<&str>,
    node: Option<&str>,
    pasted: &str,
    revision: Option<&str>,
    import_job: Option<(&str, &str)>,
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
        return Err(CHATGPT_BOUND_ELSEWHERE.into());
    }
    if !pasted.is_empty() {
        a.credentials
            .imported_refresh_sha256
            .push(fingerprint(pasted));
    }
    if let Some(login) = &a.login_credentials {
        super::reauthorize::check_email(&a, login)?;
    }
    let a = if let Some((id, revision)) = import_job {
        store::upsert_new(a, id, revision)?
    } else {
        store::upsert_checked(a, account_id, revision)?
    };
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
    fn saved_tokens_are_rejected_individually_before_import() {
        let mut previous = Account::new(Provider::ChatGpt, Realm::Global);
        previous.credentials.access_token = "saved-access".into();
        previous.credentials.refresh_token = "saved-refresh".into();
        let saved = [previous];
        for auth in [
            json!({"access_token": "saved-access"}),
            json!({"access_token": "expired", "refresh_token": "saved-refresh"}),
            json!("saved-refresh"),
        ] {
            let a = imported_chatgpt(&auth).unwrap();
            let error = reject_saved_credentials(&a, &saved).unwrap_err();
            assert!(error.contains("already saved"));
            assert!(!error.contains("saved-refresh"));
        }
        let fresh = imported_chatgpt(&json!({"access_token": "new-access"})).unwrap();
        assert!(reject_saved_credentials(&fresh, &saved).is_ok());
    }
}
