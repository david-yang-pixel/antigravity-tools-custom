use crate::modules::workbuddy::{self, SettingsUpdate, SettingsView};

#[derive(serde::Deserialize)]
pub struct ClientVersionRequest {
    #[serde(default)]
    pub target: workbuddy::releases::Target,
    #[serde(default)]
    pub refresh: bool,
}
#[derive(serde::Serialize)]
pub struct ClientVersionView {
    #[serde(flatten)]
    pub release: workbuddy::releases::ReleaseStatus,
    pub bundled_version: &'static str,
    pub bundled_cli_version: &'static str,
    pub legacy_transport: bool,
}
#[tauri::command]
pub async fn check_workbuddy_client_version(
    request: ClientVersionRequest,
) -> Result<ClientVersionView, String> {
    use std::sync::OnceLock;
    static CHECKER: OnceLock<workbuddy::releases::ReleaseChecker> = OnceLock::new();
    let proxy = crate::modules::config::load_app_config()?
        .proxy
        .upstream_proxy;
    let proxy_url = if proxy.enabled {
        proxy.url.trim().to_owned()
    } else {
        String::new()
    };
    Ok(ClientVersionView {
        release: CHECKER
            .get_or_init(|| {
                workbuddy::releases::ReleaseChecker::new(
                    workbuddy::identity::PRODUCT_RELEASE,
                    workbuddy::releases::Target::default(),
                )
            })
            .check(request.target, &proxy_url, request.refresh)
            .await,
        bundled_version: workbuddy::identity::PRODUCT_VERSION,
        bundled_cli_version: workbuddy::identity::CLI_VERSION,
        legacy_transport: workbuddy::transport::legacy_enabled(),
    })
}

#[tauri::command]
pub async fn get_workbuddy_settings() -> Result<SettingsView, String> {
    tokio::task::spawn_blocking(workbuddy::view)
        .await
        .map_err(|_| "WorkBuddy settings task failed")?
}

#[tauri::command]
pub async fn save_workbuddy_settings(request: SettingsUpdate) -> Result<SettingsView, String> {
    tokio::task::spawn_blocking(move || workbuddy::save(request))
        .await
        .map_err(|_| "WorkBuddy settings task failed")?
}

#[tauri::command]
pub async fn start_workbuddy_model_verification(
) -> Result<workbuddy::verification::Progress, String> {
    workbuddy::verification::start()
}
#[tauri::command]
pub async fn get_workbuddy_model_verification() -> Result<workbuddy::verification::Progress, String>
{
    workbuddy::verification::status()
}

use serde::Deserialize;
use workbuddy::{
    accounts::{self, AccountView, Realm},
    egress, login,
};

#[derive(Deserialize)]
pub struct ImportRequest {
    pub auth: serde_json::Value,
}
#[derive(Deserialize)]
pub struct AccountAction {
    pub id: String,
    pub enabled: Option<bool>,
    #[serde(default)]
    pub refresh: bool,
    pub managed: Option<bool>,
    pub auto_checkin: Option<bool>,
    pub keepalive: Option<bool>,
    pub auto_rewards: Option<bool>,
    pub auto_lottery: Option<bool>,
    pub auto_travel: Option<bool>,
    pub activity: Option<workbuddy::activity::Action>,
    #[serde(default)]
    pub checkin: bool,
    #[serde(default)]
    pub resume_checkin: bool,
}
#[derive(Deserialize)]
pub struct LoginRequest {
    pub realm: Realm,
    /// Reauthorize this saved account through its own egress node.
    #[serde(default)]
    pub account_id: Option<String>,
}
#[derive(Deserialize)]
pub struct LoginSession {
    pub id: String,
}

#[tauri::command]
pub async fn list_workbuddy_accounts() -> Result<Vec<AccountView>, String> {
    accounts::list()
}
#[tauri::command]
pub async fn import_workbuddy_account(request: ImportRequest) -> Result<AccountView, String> {
    let account = accounts::upsert(accounts::parse_import(request.auth)?)?;
    accounts::sync(&account.id, false).await
}
#[tauri::command]
pub async fn update_workbuddy_account(request: AccountAction) -> Result<AccountView, String> {
    let preferences = accounts::Preferences {
        managed: request.managed,
        auto_checkin: request.auto_checkin,
        keepalive: request.keepalive,
        auto_rewards: request.auto_rewards,
        auto_lottery: request.auto_lottery,
        auto_travel: request.auto_travel,
    };
    if !preferences.is_empty() {
        return accounts::configure(&request.id, preferences);
    }
    if let Some(action) = request.activity {
        return workbuddy::activity::start(&request.id, action);
    }
    if request.resume_checkin {
        return workbuddy::maintenance::resume_checkin(&request.id).await;
    }
    if request.checkin {
        return workbuddy::maintenance::run(&request.id, false, true, false).await;
    }
    match request.enabled {
        Some(enabled) => accounts::set_enabled(&request.id, enabled),
        None => accounts::sync(&request.id, request.refresh).await,
    }
}
#[tauri::command]
pub async fn delete_workbuddy_account(request: AccountAction) -> Result<(), String> {
    accounts::remove(&request.id)?;
    egress::forget(&request.id);
    Ok(())
}
#[tauri::command]
pub async fn start_workbuddy_login(request: LoginRequest) -> Result<login::Started, String> {
    login::start(request.realm, request.account_id.as_deref()).await
}
#[tauri::command]
pub async fn poll_workbuddy_login(request: LoginSession) -> Result<login::Poll, String> {
    login::poll(&request.id).await
}
#[tauri::command]
pub async fn cancel_workbuddy_login(request: LoginSession) -> Result<(), String> {
    login::cancel(&request.id).await
}

#[derive(Deserialize)]
pub struct AccountEgress {
    pub id: String,
    /// Node name, `DIRECT`, or null to let auto-assignment choose.
    pub node: Option<String>,
}
#[tauri::command]
pub async fn get_workbuddy_egress() -> Result<egress::View, String> {
    egress::view().await
}
#[tauri::command]
pub async fn save_workbuddy_egress(request: egress::Update) -> Result<egress::View, String> {
    egress::save(request).await
}
#[tauri::command]
pub async fn refresh_workbuddy_egress() -> Result<egress::View, String> {
    egress::refresh().await
}
#[tauri::command]
pub async fn test_workbuddy_egress_nodes() -> Result<egress::View, String> {
    egress::test_nodes().await
}
#[tauri::command]
pub async fn set_workbuddy_account_egress(request: AccountEgress) -> Result<egress::View, String> {
    egress::set_account(&request.id, request.node).await
}
#[tauri::command]
pub async fn test_workbuddy_account_egress(request: LoginSession) -> Result<egress::Probe, String> {
    egress::test_account(&request.id).await
}
