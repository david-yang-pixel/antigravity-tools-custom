//! Password/2FA imports before an OAuth account exists. Pending secrets and service jobs
//! use their own encrypted vault; only verified tokens enter the account pool.
use super::{
    now,
    reauthorize::{self, LoginCredentials, Progress, Service, ServiceConfig, State},
    store, Provider,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Failed,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Pending {
    pub id: String,
    pub revision: String,
    login: LoginCredentials,
    state: State,
    status: Status,
    created_at: i64,
}
#[derive(Serialize)]
pub struct View {
    pub id: String,
    pub email: String,
    pub reauthorization_service: Service,
    pub status: Status,
    pub error: Option<String>,
    pub created_at: i64,
}
impl From<&Pending> for View {
    fn from(job: &Pending) -> Self {
        Self {
            id: job.id.clone(),
            email: job.login.email.clone(),
            reauthorization_service: job.login.authorization.service,
            status: job.status,
            error: job.state.error.clone(),
            created_at: job.created_at,
        }
    }
}
#[derive(Serialize)]
pub struct Report {
    pub queued: Vec<View>,
    pub skipped: Vec<String>,
}

pub fn list(provider: Provider) -> Result<Vec<View>, String> {
    Ok(store::imports(provider)?.iter().map(Into::into).collect())
}
pub fn enqueue(
    provider: Provider,
    input: &str,
    auto: bool,
    authorization: Option<ServiceConfig>,
) -> Result<Report, String> {
    let mut entries = reauthorize::parse(input)?;
    let authorization = authorization.unwrap_or_default();
    authorization.validate()?;
    for entry in &mut entries {
        entry.authorization = authorization.clone();
    }
    let accounts = store::all(provider)?;
    store::change_imports(provider, |jobs| {
        let mut report = Report {
            queued: Vec::new(),
            skipped: Vec::new(),
        };
        for login in entries {
            if accounts
                .iter()
                .any(|a| reauthorize::check_email(a, &login).is_ok())
            {
                report.skipped.push(login.email);
                continue;
            }
            if let Some(job) = jobs.iter_mut().find(|job| job.login.email == login.email) {
                if job.status != Status::Failed && job.login != login {
                    return Err("A task with different login credentials or authorization service is already active for this email; wait for it or remove it first".into());
                }
                if job.status == Status::Failed {
                    job.login = login;
                    job.revision = uuid::Uuid::new_v4().to_string();
                    job.state = State {
                        auto,
                        ..Default::default()
                    };
                    job.status = Status::Queued;
                }
                // An active job is reused, never submitted again for the same email.
                report.queued.push((&*job).into());
            } else {
                let job = Pending {
                    id: uuid::Uuid::new_v4().to_string(),
                    revision: uuid::Uuid::new_v4().to_string(),
                    login,
                    state: State {
                        auto,
                        ..Default::default()
                    },
                    status: Status::Queued,
                    created_at: now(),
                };
                report.queued.push((&job).into());
                jobs.push(job);
            }
        }
        if accounts.len() + jobs.len() > 500 {
            return Err("At most 500 accounts and pending imports in total".into());
        }
        Ok(report)
    })
}
fn change<T>(
    provider: Provider,
    id: &str,
    revision: &str,
    update: impl FnOnce(&mut Pending) -> Result<T, String>,
) -> Result<T, String> {
    store::change_imports(provider, |jobs| {
        let job = jobs
            .iter_mut()
            .find(|job| job.id == id && job.revision == revision)
            .ok_or("Login import was removed or restarted")?;
        update(job)
    })
}
fn finish(provider: Provider, job: &Pending) -> Result<(), String> {
    store::change_imports(provider, |jobs| {
        jobs.retain(|j| j.id != job.id || j.revision != job.revision);
        Ok(())
    })
}
pub fn retry(provider: Provider, id: &str) -> Result<(), String> {
    store::change_imports(provider, |jobs| {
        let job = jobs
            .iter_mut()
            .find(|job| job.id == id)
            .ok_or("Login import no longer exists")?;
        if job.status != Status::Failed {
            return Err("Login import is already queued or running".into());
        }
        job.revision = uuid::Uuid::new_v4().to_string();
        job.state = State {
            auto: job.state.auto,
            ..Default::default()
        };
        job.status = Status::Queued;
        Ok(())
    })
}
pub fn remove(provider: Provider, id: &str) -> Result<(), String> {
    store::change_imports(provider, |jobs| {
        jobs.retain(|job| job.id != id);
        Ok(())
    })
}
fn due(job: &Pending) -> bool {
    job.status != Status::Failed && job.state.retry_at <= now()
}
pub(super) fn tick(provider: Provider) {
    let Ok(jobs) = store::imports(provider) else {
        return;
    };
    for job in jobs.into_iter().filter(due) {
        let Some(guard) = reauthorize::claim(&job.id) else {
            continue;
        };
        tauri::async_runtime::spawn(async move {
            let _guard = guard;
            process(provider, &job).await;
        });
    }
}
async fn process(provider: Provider, job: &Pending) {
    if let Err(error) = step(provider, job).await {
        let _ = change(provider, &job.id, &job.revision, |job| {
            job.state.error = Some(error);
            if job.state.batch.is_some() {
                job.state.retry_at = now() + 30;
            } else {
                job.status = Status::Failed;
            }
            Ok(())
        });
    }
}
async fn step(provider: Provider, original: &Pending) -> Result<(), String> {
    let job = store::imports(provider)?
        .into_iter()
        .find(|j| j.id == original.id && j.revision == original.revision)
        .ok_or("Login import was removed or restarted")?;
    if !due(&job) {
        return Ok(());
    }
    // Recover the small crash window between saving the account and removing this record.
    // A different existing authorization is never overwritten by an add-account task.
    if let Some(account) = store::all(provider)?
        .iter()
        .find(|a| reauthorize::check_email(a, &job.login).is_ok())
    {
        if account.login_credentials.as_ref().is_some_and(|login| {
            login.email == job.login.email
                && login.password == job.login.password
                && login.two_factor == job.login.two_factor
                && login.authorization == job.login.authorization
        }) {
            return finish(provider, &job);
        }
        change(provider, &job.id, &job.revision, |job| {
            job.state.batch = None;
            Ok(())
        })?;
        return Err("This email is already imported; use the saved account's own row".into());
    }
    let client = super::http::login_client(provider, &crate::modules::egress::Exit::Off)
        .map_err(|_| "Cannot initialize login import connection")?;
    let Some(mut batch) = job.state.batch.clone() else {
        if job.state.attempts > 0 {
            return Err(
                "Login submission was interrupted; retry explicitly to avoid duplicate jobs".into(),
            );
        }
        change(provider, &job.id, &job.revision, |job| {
            job.status = Status::Running;
            job.state.attempts = 1;
            job.state.retry_at = now() + 60;
            Ok(())
        })?;
        // A built-in login obtains its token on the node the new account will keep.
        // Merchant services perform login remotely and do not use this local browser.
        let route = if job.login.authorization.service == Service::BuiltIn {
            Some(
                crate::modules::egress::login_route(
                    crate::modules::egress::Pool::Native(provider),
                    None,
                )
                .await?,
            )
        } else {
            None
        };
        let exit = route
            .as_ref()
            .map(|route| &route.exit)
            .unwrap_or(&crate::modules::egress::Exit::Off);
        let submit_client = super::http::login_client(provider, exit)
            .map_err(|_| "Cannot initialize login import connection")?;
        let mut batch =
            reauthorize::create_batch(&submit_client, &job.login, &super::http::proxy_url(exit))
                .await?;
        batch.node = route.as_ref().and_then(|route| route.node.clone());
        change(provider, &job.id, &job.revision, |job| {
            job.state.batch = Some(batch);
            job.state.retry_at = now() + 5;
            job.state.error = None;
            Ok(())
        })?;
        return Ok(());
    };
    let progress = reauthorize::poll_batch(&client, &mut batch).await;
    let node = batch.node.clone();
    change(provider, &job.id, &job.revision, |job| {
        job.state.batch = Some(batch);
        Ok(())
    })?;
    match progress? {
        Progress::Pending => change(provider, &job.id, &job.revision, |job| {
            job.state.retry_at = now() + 5;
            job.state.error = None;
            Ok(())
        }),
        Progress::Failed(error) => {
            change(provider, &job.id, &job.revision, |job| {
                job.state.batch = None;
                Ok(())
            })?;
            Err(error)
        }
        Progress::Ready(mut auth) => {
            auth.as_object_mut()
                .ok_or("Invalid account export")?
                .insert(
                    "login_credentials".into(),
                    serde_json::to_value(&job.login)
                        .map_err(|_| "Cannot bind login credentials")?,
                );
            if super::login::import_new(
                provider,
                auth,
                job.state.auto,
                &job.id,
                &job.revision,
                node.as_deref(),
            )
            .await
            .is_err()
            {
                // An import may have consumed a rotating refresh token. Never download and
                // replay that same export automatically after a failed validation/save.
                change(provider, &job.id, &job.revision, |job| {
                    job.state.batch = None;
                    Ok(())
                })?;
                return Err("Cannot verify or save this account; check its identity, egress node and account limit".into());
            }
            finish(provider, &job)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{chatgpt, tests::Sandbox, Account, Realm};
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    const INPUT: &str = "new@example.com----private-password----JBSWY3DPEHPK3PXP";
    fn token(email: &str) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(json!({"email":email,"exp":now()+7200,
            "https://api.openai.com/auth":{"chatgpt_account_id":"new-account","chatgpt_user_id":"user"}}).to_string()))
    }
    fn account() -> Account {
        let mut a = Account::new(Provider::ChatGpt, Realm::Global);
        a.credentials.access_token = token("new@example.com");
        a.credentials.id_token = token("new@example.com");
        a.login_credentials = reauthorize::parse(INPUT).unwrap().pop();
        chatgpt::identity::apply(&mut a).unwrap();
        a
    }
    fn pending() -> Pending {
        store::imports(Provider::ChatGpt).unwrap().remove(0)
    }
    fn ready_to_poll() -> Pending {
        let job = pending();
        change(Provider::ChatGpt, &job.id, &job.revision, |job| {
            job.state.retry_at = 0;
            Ok(())
        })
        .unwrap();
        pending()
    }
    struct Server {
        seen: Arc<Mutex<Vec<String>>>,
        handle: tokio::task::JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.handle.abort();
            chatgpt::endpoints::set_test(None);
        }
    }
    async fn server(mode: &'static str) -> Server {
        use axum::{
            body::Bytes,
            http::{HeaderMap, Uri},
            Json, Router,
        };
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let log = log.clone();
            async move {
                let path = uri.path_and_query().unwrap().to_string();
                log.lock().unwrap().push(path.clone());
                let value = match path.as_str() {
                    "/api/submit" => {
                        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["text"], INPUT);
                        json!({"ok":true,"created":[{"task_id":1,"account_id":1}]})
                    }
                    "/api/tasks/1" => {
                        assert_eq!(headers["cookie"], "session=private-session");
                        json!({"ok":true,"task":{"id":1,"status":"success"}})
                    }
                    "/api/download/1?format=sub2api" => {
                        assert_eq!(headers["cookie"], "session=private-session");
                        json!({"accounts":[{"platform":"openai","credentials":{"access_token":token("new@example.com"),
                            "id_token":token("new@example.com"),"refresh_token":"new-refresh"}}]})
                    }
                    "/api/batches" => {
                        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["input"], INPUT);
                        json!({"batch_id":"job", "access_token":"private-job-token"})
                    }
                    "/api/batches/job" => {
                        assert_eq!(headers["authorization"], "Bearer private-job-token");
                        json!({"snapshot":{"status":"completed","total":1,"success":if mode=="rejected"{0}else{1},
                            "items":[{"errorCode":"ACCOUNT_BANNED"}]}})
                    }
                    "/api/batches/job/download?format=sub2" => {
                        assert_eq!(headers["authorization"], "Bearer private-job-token");
                        let email = if mode == "wrong-account" { "other@example.com" } else { "new@example.com" };
                        json!({"accounts":[{"platform":"openai","credentials":{"access_token":token(email),
                            "id_token":token(email),"refresh_token":"new-refresh"}}]})
                    }
                    _ if path.starts_with("/backend-api/codex/models") => json!({"models":[]}),
                    _ => panic!("unexpected request {path}"),
                };
                let mut out = HeaderMap::new();
                if path == "/api/submit" {
                    out.insert("set-cookie", "session=private-session; Path=/; HttpOnly".parse().unwrap());
                }
                (out, Json(value))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::env::set_var("ABV_CHATGPT_REAUTH_URL", &base);
        std::env::set_var("ABV_CHATGPT_RUOZHEN_URL", &base);
        chatgpt::endpoints::set_test(Some(chatgpt::endpoints::Endpoints {
            auth: base.clone(),
            backend: format!("{base}/backend-api"),
        }));
        let handle = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        Server { seen, handle }
    }
    #[test]
    fn enqueue_is_encrypted_and_repeated_input_reuses_the_job() {
        let _sandbox = Sandbox::new();
        let provider = Provider::ChatGpt;
        let first = enqueue(provider, INPUT, true, None).unwrap();
        let second = enqueue(provider, INPUT, true, None).unwrap();
        assert_eq!(first.queued[0].id, second.queued[0].id);
        assert_eq!(store::imports(provider).unwrap().len(), 1);
        assert!(store::all(provider).unwrap().is_empty());
        let disk = std::fs::read_to_string(
            crate::modules::account::get_data_dir()
                .unwrap()
                .join("chatgpt_login_imports.enc"),
        )
        .unwrap();
        let view = serde_json::to_string(&list(provider).unwrap()).unwrap();
        for secret in ["private-password", "JBSWY3DPEHPK3PXP"] {
            assert!(!disk.contains(secret));
            assert!(!view.contains(secret));
        }
        remove(provider, &first.queued[0].id).unwrap();
        assert!(store::imports(provider).unwrap().is_empty());
    }
    #[tokio::test]
    async fn ruozhen_import_keeps_the_selected_service_and_never_exports_its_cdk_or_session() {
        let _sandbox = Sandbox::new();
        let server = server("ruozhen").await;
        let provider = Provider::ChatGpt;
        enqueue(
            provider,
            INPUT,
            true,
            Some(ServiceConfig {
                service: Service::Ruozhen,
                cdk: "private-cdk".into(),
            }),
        )
        .unwrap();
        process(provider, &pending()).await;
        let view = serde_json::to_string(&list(provider).unwrap()).unwrap();
        let disk = std::fs::read_to_string(
            crate::modules::account::get_data_dir()
                .unwrap()
                .join("chatgpt_login_imports.enc"),
        )
        .unwrap();
        assert!(view.contains("ruozhen"));
        for secret in ["private-password", "private-cdk", "private-session"] {
            assert!(!view.contains(secret) && !disk.contains(secret));
        }
        process(provider, &ready_to_poll()).await;
        let saved = store::all(provider).unwrap().remove(0);
        assert!(reauthorize::service_for(&saved).unwrap().service == Service::Ruozhen);
        assert_eq!(reauthorize::service_for(&saved).unwrap().cdk, "private-cdk");
        assert!(saved.enabled && saved.reauthorization.auto);
        assert!(!server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.contains("batches")));
        assert!(list(provider).unwrap().is_empty());
    }
    #[tokio::test]
    async fn direct_login_import_adds_a_new_account_without_manual_json() {
        let _sandbox = Sandbox::new();
        let server = server("success").await;
        let provider = Provider::ChatGpt;
        enqueue(provider, INPUT, true, None).unwrap();
        process(provider, &pending()).await;
        assert!(pending().state.batch.is_some());
        let persisted_view = serde_json::to_string(&list(provider).unwrap()).unwrap();
        assert!(!persisted_view.contains("private-job-token"));
        process(provider, &ready_to_poll()).await;
        let accounts = store::all(provider).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].nickname, "new@example.com");
        assert!(accounts[0].reauthorization.auto && accounts[0].login_credentials.is_some());
        assert_eq!(accounts[0].credentials.refresh_token, "new-refresh");
        assert!(list(provider).unwrap().is_empty());
        assert_eq!(
            server
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.as_str() == "/api/batches")
                .count(),
            1
        );
        assert_eq!(
            enqueue(provider, INPUT, true, None).unwrap().skipped,
            ["new@example.com"]
        );
    }
    #[tokio::test]
    async fn rejected_or_wrong_identity_imports_stop_and_can_be_retried() {
        for mode in ["rejected", "wrong-account"] {
            let _sandbox = Sandbox::new();
            let _server = server(mode).await;
            let provider = Provider::ChatGpt;
            enqueue(provider, INPUT, true, None).unwrap();
            process(provider, &pending()).await;
            process(provider, &ready_to_poll()).await;
            let failed = pending();
            assert!(failed.status == Status::Failed);
            assert!(failed.state.batch.is_none());
            assert!(!due(&failed));
            assert!(store::all(provider).unwrap().is_empty());
            if mode == "rejected" {
                assert!(failed
                    .state
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("ACCOUNT_BANNED"));
            }
            retry(provider, &failed.id).unwrap();
            let queued = pending();
            assert!(queued.status == Status::Queued);
            assert_ne!(failed.revision, queued.revision);
        }
    }
    #[test]
    fn removed_and_restarted_jobs_cannot_add_accounts_and_existing_accounts_are_preserved() {
        let _sandbox = Sandbox::new();
        let provider = Provider::ChatGpt;
        enqueue(provider, INPUT, true, None).unwrap();
        let old = pending();
        remove(provider, &old.id).unwrap();
        assert!(store::upsert_new(account(), &old.id, &old.revision).is_err());
        enqueue(provider, INPUT, true, None).unwrap();
        let job = pending();
        let saved = store::upsert(account(), None).unwrap();
        assert!(store::upsert_new(account(), &job.id, &job.revision).is_err());
        assert_eq!(store::all(provider).unwrap()[0].revision, saved.revision);
    }
    #[tokio::test]
    async fn saved_accounts_clean_up_a_pending_record_after_a_restart() {
        let _sandbox = Sandbox::new();
        let provider = Provider::ChatGpt;
        enqueue(provider, INPUT, true, None).unwrap();
        let job = pending();
        store::upsert_new(account(), &job.id, &job.revision).unwrap();
        process(provider, &job).await;
        assert!(list(provider).unwrap().is_empty());
        assert_eq!(store::all(provider).unwrap().len(), 1);
    }
    #[tokio::test]
    async fn a_manual_import_during_authorization_stops_the_pending_job() {
        let _sandbox = Sandbox::new();
        let _server = server("success").await;
        let provider = Provider::ChatGpt;
        enqueue(provider, INPUT, true, None).unwrap();
        process(provider, &pending()).await;
        let mut manual = account();
        manual.login_credentials = None;
        let manual = store::upsert(manual, None).unwrap();
        process(provider, &ready_to_poll()).await;
        let job = pending();
        assert!(job.status == Status::Failed && job.state.batch.is_none());
        assert_eq!(store::all(provider).unwrap()[0].revision, manual.revision);
    }
}
