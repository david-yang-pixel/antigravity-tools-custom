use crate::modules::workbuddy::{self, SettingsUpdate, SettingsView};

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
    login,
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
    pub activity: Option<workbuddy::activity::Action>,
    #[serde(default)]
    pub checkin: bool,
}
#[derive(Deserialize)]
pub struct LoginRequest {
    pub realm: Realm,
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
    if request.managed.is_some()
        || request.auto_checkin.is_some()
        || request.keepalive.is_some()
        || request.auto_rewards.is_some()
        || request.auto_lottery.is_some()
    {
        return accounts::configure(
            &request.id,
            request.managed,
            request.auto_checkin,
            request.keepalive,
            request.auto_rewards,
            request.auto_lottery,
        );
    }
    if let Some(action) = request.activity {
        return workbuddy::activity::start(&request.id, action);
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
    accounts::remove(&request.id)
}
#[tauri::command]
pub async fn start_workbuddy_login(request: LoginRequest) -> Result<login::Started, String> {
    login::start(request.realm).await
}
#[tauri::command]
pub async fn poll_workbuddy_login(request: LoginSession) -> Result<login::Poll, String> {
    login::poll(&request.id).await
}
#[tauri::command]
pub async fn cancel_workbuddy_login(request: LoginSession) -> Result<(), String> {
    login::cancel(&request.id).await
}
