//! Login material and resumable jobs for the administrator's reauthorization service.
//! Secrets live in the encrypted vault. Ordinary account views expose only job state.
use super::{now, store, text, Account, AccountView, Provider};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{LazyLock, Mutex},
    time::Duration,
};

const SERVICE: &str = "https://login.dbc2api.top";
const MAX_ATTEMPTS: u32 = 3;
static ACTIVE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

#[derive(Clone, Deserialize, Serialize)]
pub struct LoginCredentials {
    pub email: String,
    pub password: String,
    pub two_factor: String,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    pub auto: bool,
    pub needed: bool,
    pub requested: bool,
    pub batch: Option<Batch>,
    pub attempts: u32,
    pub retry_at: i64,
    pub error: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Batch {
    id: String,
    token: String,
    base: String,
    created_at: i64,
}

fn valid_email(email: &str) -> bool {
    email
        .split_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
        && !email.chars().any(char::is_whitespace)
}
pub fn parse(input: &str) -> Result<Vec<LoginCredentials>, String> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<_> = line.trim().split("----").collect();
        let invalid = || format!("Line {}: expected email----password----2fa", index + 1);
        if fields.len() != 3 {
            return Err(invalid());
        }
        let email = fields[0].trim().to_lowercase();
        let password = fields[1].to_owned();
        let two_factor = fields[2]
            .split_whitespace()
            .collect::<String>()
            .to_uppercase();
        if !valid_email(&email)
            || password.is_empty()
            || password.len() > 4096
            || password.chars().any(char::is_control)
            || two_factor.trim_end_matches('=').len() < 8
            || two_factor.len() > 512
            || !two_factor
                .trim_end_matches('=')
                .chars()
                .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c))
        {
            return Err(invalid());
        }
        if !seen.insert(email.clone()) {
            return Err(format!("Line {}: duplicate email", index + 1));
        }
        entries.push(LoginCredentials {
            email,
            password,
            two_factor,
        });
    }
    if entries.is_empty() || entries.len() > 500 {
        return Err("Enter 1–500 login credential lines".into());
    }
    Ok(entries)
}
pub(super) fn from_json(value: &Value) -> Result<Option<LoginCredentials>, String> {
    let sources = [
        value.get("login_credentials"),
        Some(value),
        value.get("credentials"),
        value.get("extra"),
    ];
    let find = |keys: &[&str]| {
        sources
            .iter()
            .flatten()
            .map(|v| text(v, keys))
            .find(|s| !s.is_empty())
            .unwrap_or_default()
    };
    let password = find(&["password"]);
    let factor = find(&["2fa", "two_factor", "totp_secret"]);
    // Incomplete login material must not prevent importing an otherwise valid token.
    if password.is_empty() || factor.is_empty() {
        return Ok(None);
    }
    let email = [
        find(&["email"]),
        super::chatgpt::identity::claims(&find(&["id_token", "idToken"]))
            .map(|c| c.email)
            .unwrap_or_default(),
        super::chatgpt::identity::claims(&find(&["access_token", "accessToken"]))
            .map(|c| c.email)
            .unwrap_or_default(),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or_default();
    parse(&format!("{email}----{password}----{factor}")).map(|mut entries| entries.pop())
}
pub(super) fn check_email(a: &Account, login: &LoginCredentials) -> Result<(), String> {
    let claims = super::chatgpt::identity::claims(&a.credentials.id_token)
        .filter(|c| !c.email.is_empty())
        .or_else(|| super::chatgpt::identity::claims(&a.credentials.access_token));
    let email = claims
        .map(|c| c.email)
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| a.nickname.clone());
    if !email.eq_ignore_ascii_case(&login.email) {
        return Err("Login email does not match the authorized account".into());
    }
    Ok(())
}

/// Bind TXT credentials to verified, existing accounts. Validate every line before writing.
pub fn bind(
    provider: Provider,
    id: Option<&str>,
    input: &str,
    auto: bool,
) -> Result<usize, String> {
    let entries = parse(input)?;
    if id.is_some() && entries.len() != 1 {
        return Err("Enter one credential line for this account".into());
    }
    let accounts = store::all(provider)?;
    let mut updates = Vec::new();
    for entry in entries {
        let matches: Vec<_> = accounts
            .iter()
            .filter(|a| id.is_none_or(|id| id == a.id) && check_email(a, &entry).is_ok())
            .collect();
        if matches.len() != 1 {
            return Err(format!("No unique saved account for {}", entry.email));
        }
        updates.push((matches[0].id.clone(), matches[0].revision.clone(), entry));
    }
    store::bind_logins(provider, &updates, auto)?;
    Ok(updates.len())
}

pub(super) fn base_url() -> Result<String, String> {
    let base = std::env::var("ABV_CHATGPT_REAUTH_URL").unwrap_or_else(|_| SERVICE.into());
    let url = url::Url::parse(base.trim()).map_err(|_| "Invalid reauthorization service URL")?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !(local && url.scheme() == "http"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Reauthorization URL must use HTTPS without credentials, query or fragment".into(),
        );
    }
    Ok(url.as_str().trim_end_matches('/').into())
}
fn segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn due(a: &Account, at: i64) -> bool {
    let s = &a.reauthorization;
    if a.login_credentials.is_none() || s.retry_at > at {
        return false;
    }
    if s.batch.is_some() || s.requested {
        return true;
    }
    s.auto
        && a.auto_refresh
        && (a.enabled || a.credential_dead)
        && s.attempts < MAX_ATTEMPTS
        && (s.needed
            || (a.credential_dead
                && a.auth_error
                    .as_deref()
                    .is_some_and(|e| e.contains("refresh token is no longer valid")))
            || (a.enabled
                && a.credentials.refresh_token.is_empty()
                && a.expires_at > 0
                && a.expires_at <= at))
}
/// An explicit request starts/resumes a job; HTTP callers never wait for an OAuth login.
pub fn request(provider: Provider, id: &str) -> Result<AccountView, String> {
    let account = store::get(provider, id)?;
    let login = account
        .login_credentials
        .as_ref()
        .ok_or("Add email----password----2fa first")?;
    check_email(&account, login)?;
    base_url()?;
    let account = store::update(provider, id, Some(&account.revision), |a| {
        a.reauthorization.requested = true;
        a.reauthorization.attempts = 0;
        a.reauthorization.retry_at = 0;
        a.reauthorization.error = None;
    })?;
    launch(account.clone());
    Ok((&account).into())
}
pub(super) struct Guard(String);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.lock() {
            active.remove(&self.0);
        }
    }
}
fn launch(account: Account) {
    let Some(guard) = claim(&account.id) else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let _guard = guard;
        if let Err(error) = step(&account).await {
            // Never echo a service response or a reqwest URL: either may contain secrets.
            let _ = store::update(
                account.provider,
                &account.id,
                Some(&account.revision),
                |a| {
                    a.reauthorization.error = Some(error);
                    a.reauthorization.requested = false;
                    a.reauthorization.retry_at = now()
                        + if a.reauthorization.batch.is_some() {
                            30
                        } else {
                            3600
                        };
                },
            );
        }
    });
}
pub(super) fn claim(id: &str) -> Option<Guard> {
    let Ok(mut active) = ACTIVE.lock() else {
        return None;
    };
    if active.len() >= 3 || !active.insert(id.to_owned()) {
        return None;
    }
    Some(Guard(id.to_owned()))
}
async fn json_response(response: reqwest::Response) -> Result<Value, String> {
    if !response.status().is_success() {
        return Err(format!(
            "Reauthorization service returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let bytes = super::chatgpt::client::read_bytes(response)
        .await
        .map_err(|_| "Cannot read reauthorization response")?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid reauthorization response".into())
}
pub(super) async fn create_batch(
    client: &reqwest::Client,
    login: &LoginCredentials,
) -> Result<Batch, String> {
    let base = base_url()?;
    let response = client.post(format!("{base}/api/batches"))
            .timeout(Duration::from_secs(20))
            .json(&json!({"input": format!("{}----{}----{}", login.email, login.password, login.two_factor)}))
            .send().await.map_err(|_| "Cannot submit reauthorization; retry is delayed to avoid duplicate jobs")?;
    let value = json_response(response).await?;
    let id = text(&value, &["batch_id"]);
    let token = text(&value, &["access_token"]);
    if !segment(&id) || token.is_empty() || token.len() > 4096 {
        return Err("Reauthorization service returned an invalid job".into());
    }
    Ok(Batch {
        id,
        token,
        base,
        created_at: now(),
    })
}
pub(super) enum Progress {
    Pending,
    Failed(String),
    Ready(Value),
}
pub(super) async fn poll_batch(
    client: &reqwest::Client,
    batch: &Batch,
) -> Result<Progress, String> {
    if now() - batch.created_at > 3600 {
        return Ok(Progress::Failed(
            "Reauthorization job timed out; retry later".into(),
        ));
    }
    let endpoint = format!("{}/api/batches/{}", batch.base, batch.id);
    let response = client
        .get(&endpoint)
        .bearer_auth(&batch.token)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| "Cannot query reauthorization job")?;
    let value = json_response(response).await?;
    let snapshot = &value["snapshot"];
    let status = snapshot["status"]
        .as_str()
        .ok_or("Invalid reauthorization job status")?;
    if status != "completed" {
        return Ok(Progress::Pending);
    }
    if snapshot["success"].as_u64() != Some(1) || snapshot["total"].as_u64() != Some(1) {
        let code = snapshot["items"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["errorCode"].as_str())
            .filter(|code| {
                !code.is_empty()
                    && code.len() < 80
                    && code
                        .bytes()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            });
        return Ok(Progress::Failed(match code {
            Some(code) => format!(
                "Reauthorization failed ({code}); check the login credentials or account status"
            ),
            None => "Reauthorization failed; check the login credentials or account status".into(),
        }));
    }
    let response = client
        .get(format!("{endpoint}/download?format=sub2"))
        .bearer_auth(&batch.token)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| "Cannot download reauthorized Sub2 JSON")?;
    let export = json_response(response).await?;
    let entry = exported_account(&export)?;
    Ok(Progress::Ready(entry))
}
async fn step(original: &Account) -> Result<(), String> {
    let a = store::get(original.provider, &original.id)?;
    if a.revision != original.revision || !due(&a, now()) {
        return Ok(());
    }
    let login = a
        .login_credentials
        .as_ref()
        .ok_or("Login credentials are missing")?;
    check_email(&a, login)?;
    let client = super::http::client_for(&a)
        .await
        .map_err(|_| "Cannot connect through this account's egress node")?;
    let Some(batch) = &a.reauthorization.batch else {
        // Persist the attempt before POST. A lost response must not cause a burst of jobs.
        store::update(a.provider, &a.id, Some(&a.revision), |a| {
            a.reauthorization.attempts += 1;
            a.reauthorization.retry_at = now() + 3600;
        })?;
        let batch = create_batch(&client, login).await?;
        store::update(a.provider, &a.id, Some(&a.revision), |a| {
            a.reauthorization.batch = Some(batch);
            a.reauthorization.requested = false;
            a.reauthorization.retry_at = now() + 5;
            a.reauthorization.error = None;
        })?;
        return Ok(());
    };
    let entry = match poll_batch(&client, batch).await? {
        Progress::Pending => {
            store::update(a.provider, &a.id, Some(&a.revision), |a| {
                a.reauthorization.retry_at = now() + 5;
                a.reauthorization.error = None;
            })?;
            return Ok(());
        }
        Progress::Failed(error) => {
            store::update(a.provider, &a.id, Some(&a.revision), |a| {
                a.reauthorization.batch = None
            })?;
            return Err(error);
        }
        Progress::Ready(entry) => entry,
    };
    // The normal importer proves identity with OpenAI on this account's node. The revision
    // check also prevents a late job from replacing a newer manual authorization.
    super::login::import_checked(a.provider, entry, Some(&a.id), Some(&a.revision))
        .await
        .map_err(|_| "Cannot apply reauthorized credentials to the original account")?;
    super::accounts::scheduling::clear_cooldowns(Some(&a.id));
    Ok(())
}
fn exported_account(export: &Value) -> Result<Value, String> {
    let entries = export["accounts"]
        .as_array()
        .ok_or("Expected a Sub2 accounts export")?;
    if entries.len() != 1 || entries[0]["platform"] != "openai" {
        return Err("Reauthorization must return exactly one OpenAI account".into());
    }
    Ok(entries[0].clone())
}
pub fn start() {
    tauri::async_runtime::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            for provider in Provider::ALL {
                super::login_imports::tick(provider);
                if !store::settings(provider).is_ok_and(|s| s.enabled) {
                    continue;
                }
                if let Ok(accounts) = store::all(provider) {
                    for a in accounts.into_iter().filter(|a| due(a, now())) {
                        launch(a);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::{chatgpt, tests::Sandbox, Realm};
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    fn token(email: &str, marker: &str) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(json!({"email": email, "exp": now() + 7200,
            "marker": marker, "https://api.openai.com/auth": {"chatgpt_account_id":"account", "chatgpt_user_id":"user"}}).to_string()))
    }
    fn saved() -> Account {
        let mut a = Account::new(Provider::ChatGpt, Realm::Global);
        a.credentials.access_token = token("a@example.com", "old");
        a.credentials.id_token = token("a@example.com", "old");
        chatgpt::identity::apply(&mut a).unwrap();
        store::upsert(a, None).unwrap()
    }
    #[test]
    fn binding_validates_the_whole_batch_and_preserves_material_on_reimport() {
        let _sandbox = Sandbox::new();
        let a = saved();
        let input = "a@example.com----private-password----JBSWY3DPEHPK3PXP";
        assert!(bind(
            a.provider,
            None,
            &format!("{input}\nb@example.com----secret----JBSWY3DPEHPK3PXP"),
            true
        )
        .is_err());
        assert!(store::get(a.provider, &a.id)
            .unwrap()
            .login_credentials
            .is_none());
        bind(a.provider, Some(&a.id), input, true).unwrap();
        let mut replacement = Account::new(a.provider, Realm::Global);
        replacement.credentials.access_token = token("a@example.com", "new");
        replacement.credentials.id_token = token("a@example.com", "new");
        chatgpt::identity::apply(&mut replacement).unwrap();
        let replaced = store::upsert(replacement.clone(), Some(&a.id)).unwrap();
        assert_eq!(
            replaced.login_credentials.as_ref().unwrap().password,
            "private-password"
        );
        assert!(replaced.reauthorization.auto);
        assert!(!serde_json::to_string(&AccountView::from(&replaced))
            .unwrap()
            .contains("private-password"));
        assert!(store::upsert_checked(replacement, Some(&a.id), Some(&a.revision)).is_err());
    }
    #[tokio::test]
    async fn job_resumes_from_the_vault_and_updates_only_the_original_account() {
        use axum::{
            body::Bytes,
            http::{HeaderMap, Uri},
            Json, Router,
        };
        let _sandbox = Sandbox::new();
        let a = saved();
        bind(
            a.provider,
            Some(&a.id),
            "a@example.com----private-password----JBSWY3DPEHPK3PXP",
            true,
        )
        .unwrap();
        let a = store::update(a.provider, &a.id, None, |a| {
            a.enabled = false;
            a.credential_dead = true;
            a.reauthorization.needed = true;
        })
        .unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, bytes: Bytes| {
            let log = log.clone();
            async move {
                let target = uri.path_and_query().unwrap().to_string();
                log.lock().unwrap().push(target.clone());
                let response = match target.as_str() {
                    "/api/batches" => {
                        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["input"], "a@example.com----private-password----JBSWY3DPEHPK3PXP");
                        json!({"batch_id":"job-1", "access_token":"job-secret"})
                    }
                    "/api/batches/job-1" => {
                        assert_eq!(headers["authorization"], "Bearer job-secret");
                        json!({"snapshot":{"status":"completed", "success":1, "total":1}})
                    }
                    "/api/batches/job-1/download?format=sub2" => {
                        assert_eq!(headers["authorization"], "Bearer job-secret");
                        json!({"accounts":[{"platform":"openai", "credentials":{
                            "access_token":token("a@example.com", "new"), "id_token":token("a@example.com", "new"), "refresh_token":"new-refresh"}}]})
                    }
                    _ if target.starts_with("/backend-api/codex/models") => json!({"models":[]}),
                    _ => panic!("unexpected request {target}"),
                };
                Json(response)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        std::env::set_var("ABV_CHATGPT_REAUTH_URL", &base);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                chatgpt::endpoints::set_test(None);
            }
        }
        let _reset = Reset;
        chatgpt::endpoints::set_test(Some(chatgpt::endpoints::Endpoints {
            auth: base.clone(),
            backend: format!("{base}/backend-api"),
        }));
        step(&a).await.unwrap();
        let resume =
            store::update(a.provider, &a.id, None, |a| a.reauthorization.retry_at = 0).unwrap();
        assert!(resume.reauthorization.batch.is_some());
        step(&resume).await.unwrap();
        let updated = store::get(a.provider, &a.id).unwrap();
        assert_eq!(store::all(a.provider).unwrap().len(), 1);
        assert!(updated.enabled && !updated.credential_dead);
        assert_eq!(updated.credentials.refresh_token, "new-refresh");
        assert!(updated.reauthorization.batch.is_none());
        assert!(updated.login_credentials.is_some() && updated.reauthorization.auto);
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|s| s.as_str() == "/api/batches")
                .count(),
            1
        );
        server.abort();
    }
    #[test]
    fn credentials_are_validated_without_echoing_secrets() {
        let entries = parse("\nA@example.com----pass-word----jbsw y3dp ehpk3pxp\n").unwrap();
        assert_eq!(entries[0].email, "a@example.com");
        assert_eq!(entries[0].two_factor, "JBSWY3DPEHPK3PXP");
        for input in ["bad----secret----123", "a@example.com----secret----12345678", "a@example.com----secret----JBSWY3DPEHPK3PXP\na@example.com----secret----JBSWY3DPEHPK3PXP"] {
            assert!(!parse(input).err().unwrap().contains("secret"));
        }
    }
    #[test]
    fn json_login_material_can_use_the_verified_token_email_and_remain_optional() {
        let value = json!({"credentials":{"id_token": token("a@example.com", "test"),
            "password":"private-password", "2fa":"JBSWY3DPEHPK3PXP"}});
        assert_eq!(from_json(&value).unwrap().unwrap().email, "a@example.com");
        assert!(from_json(&json!({"2fa":"JBSWY3DPEHPK3PXP"}))
            .unwrap()
            .is_none());
    }
    #[test]
    fn only_expired_or_rejected_credentials_start_automatic_jobs() {
        let mut a = Account::new(Provider::ChatGpt, super::super::Realm::Global);
        a.login_credentials = Some(
            parse("a@example.com----secret----JBSWY3DPEHPK3PXP")
                .unwrap()
                .remove(0),
        );
        a.reauthorization.auto = true;
        a.expires_at = 100;
        a.credentials.refresh_token = "renew-first".into();
        assert!(!due(&a, 200));
        a.reauthorization.needed = true;
        a.credential_dead = true;
        a.enabled = false;
        assert!(due(&a, 200));
        a.credential_dead = false;
        assert!(!due(&a, 200)); // administrator-disabled accounts stay disabled
        a.reauthorization.requested = true;
        assert!(due(&a, 200));
        a.reauthorization.retry_at = 300;
        assert!(!due(&a, 200));
        a.reauthorization.retry_at = 0;
        a.reauthorization.requested = false;
        a.enabled = true;
        a.reauthorization.attempts = MAX_ATTEMPTS;
        assert!(!due(&a, 200));
    }
    #[test]
    fn download_cannot_import_another_platform_or_multiple_accounts() {
        assert!(exported_account(&json!({"accounts": [{"platform":"openai"}]})).is_ok());
        for value in [
            json!({"accounts": []}),
            json!({"accounts": [{"platform":"anthropic"}]}),
            json!({"accounts": [{"platform":"openai"}, {"platform":"openai"}]}),
        ] {
            assert!(exported_account(&value).is_err());
        }
    }
}
