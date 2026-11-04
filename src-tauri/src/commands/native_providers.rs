//! Desktop and authenticated Web admin commands share the same native provider service.
use crate::modules::native_providers::{
    self as native, AccountView, Provider, Realm, Settings, SettingsView,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ProviderRequest {
    pub provider: Provider,
}
#[derive(Deserialize)]
pub struct SettingsRequest {
    pub provider: Provider,
    pub enabled: bool,
    pub preferred_account_id: Option<String>,
}
#[derive(Deserialize)]
pub struct LoginRequest {
    pub provider: Provider,
    pub realm: Realm,
    pub account_id: Option<String>,
    #[serde(default)]
    pub solo: bool,
    #[serde(default)]
    pub server_browser: bool,
}
#[derive(Deserialize)]
pub struct SessionRequest {
    pub provider: Provider,
    pub id: String,
}
#[derive(Deserialize)]
pub struct CallbackRequest {
    pub provider: Provider,
    pub id: String,
    pub callback_url: String,
}
#[derive(Deserialize)]
pub struct BrowserInputRequest {
    pub provider: Provider,
    pub id: String,
    pub action: native::browser::Input,
}
#[derive(Deserialize)]
pub struct ImportRequest {
    pub provider: Provider,
    pub realm: Realm,
    pub auth: serde_json::Value,
}
#[derive(Deserialize)]
pub struct AccountRequest {
    pub provider: Provider,
    pub id: String,
    pub enabled: Option<bool>,
    pub auto_refresh: Option<bool>,
    #[serde(default)]
    pub refresh: bool,
}

#[tauri::command]
pub async fn list_native_accounts(request: ProviderRequest) -> Result<Vec<AccountView>, String> {
    native::accounts::list(request.provider)
}
#[tauri::command]
pub async fn get_native_provider_settings(
    request: ProviderRequest,
) -> Result<SettingsView, String> {
    native::store::settings_view(request.provider)
}
#[tauri::command]
pub async fn save_native_provider_settings(
    request: SettingsRequest,
) -> Result<SettingsView, String> {
    native::store::save_settings(
        request.provider,
        Settings {
            enabled: request.enabled,
            preferred_account_id: request.preferred_account_id,
        },
    )
}
#[tauri::command]
pub async fn start_native_login(request: LoginRequest) -> Result<native::login::Started, String> {
    native::login::start_with_browser(
        request.provider,
        request.realm,
        request.account_id.as_deref(),
        request.solo,
        request.server_browser,
    )
    .await
}
#[tauri::command]
pub async fn poll_native_login(request: SessionRequest) -> Result<native::login::Poll, String> {
    native::login::poll(request.provider, &request.id).await
}
#[tauri::command]
pub async fn cancel_native_login(request: SessionRequest) -> Result<(), String> {
    native::login::cancel(request.provider, &request.id).await
}
#[tauri::command]
pub async fn native_login_browser_snapshot(
    request: SessionRequest,
) -> Result<native::browser::Snapshot, String> {
    if request.provider != Provider::Trae {
        return Err("Only TRAE uses a server login browser".into());
    }
    native::browser::snapshot(&request.id).await
}
#[tauri::command]
pub async fn native_login_browser_input(request: BrowserInputRequest) -> Result<(), String> {
    if request.provider != Provider::Trae {
        return Err("Only TRAE uses a server login browser".into());
    }
    native::browser::input(&request.id, request.action).await
}
#[tauri::command]
pub async fn complete_native_login(request: CallbackRequest) -> Result<AccountView, String> {
    native::login::complete(request.provider, &request.id, &request.callback_url).await
}
#[tauri::command]
pub async fn import_native_account(request: ImportRequest) -> Result<AccountView, String> {
    native::login::import(request.provider, request.realm, request.auth).await
}
#[tauri::command]
pub async fn update_native_account(request: AccountRequest) -> Result<AccountView, String> {
    if request.enabled.is_some() || request.auto_refresh.is_some() {
        native::accounts::configure(
            request.provider,
            &request.id,
            request.enabled,
            request.auto_refresh,
        )
    } else {
        native::accounts::sync(request.provider, &request.id, request.refresh).await
    }
}
#[tauri::command]
pub async fn delete_native_account(request: AccountRequest) -> Result<(), String> {
    native::store::remove(request.provider, &request.id)
}
