//! Desktop and authenticated Web admin commands share the same native provider service.
use crate::modules::egress::{self, Pool};
use crate::modules::native_providers::{
    self as native, AccountView, Provider, Settings, SettingsView,
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
pub struct SessionRequest {
    pub provider: Provider,
    pub id: String,
}
#[derive(Deserialize)]
pub struct ImportRequest {
    pub provider: Provider,
    pub auth: serde_json::Value,
    /// Re-import onto this saved account, through its own egress node.
    #[serde(default)]
    pub account_id: Option<String>,
}
#[derive(Deserialize)]
pub struct AccountRequest {
    pub provider: Provider,
    pub id: String,
    pub enabled: Option<bool>,
    pub auto_refresh: Option<bool>,
    pub auto_reauthorize: Option<bool>,
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
/// Imports every account of a multi-account export (ChatGPT, e.g. a sub2api export) and
/// reports each one; a failed account never stops the rest.
#[tauri::command]
pub async fn import_native_accounts_batch(
    request: ImportRequest,
) -> Result<Vec<native::login::BatchItem>, String> {
    native::login::import_batch(request.provider, request.auth).await
}
#[tauri::command]
pub async fn import_native_account(request: ImportRequest) -> Result<AccountView, String> {
    native::login::import(
        request.provider,
        request.auth,
        request.account_id.as_deref(),
    )
    .await
}
#[tauri::command]
pub async fn update_native_account(request: AccountRequest) -> Result<AccountView, String> {
    if let Some(auto) = request.auto_reauthorize {
        let a = native::store::update(request.provider, &request.id, None, |a| {
            a.reauthorization.auto = auto
        })?;
        return Ok((&a).into());
    }
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

#[derive(Deserialize)]
pub struct LoginCredentialsRequest {
    pub provider: Provider,
    pub id: Option<String>,
    pub input: String,
    #[serde(default)]
    pub auto_reauthorize: bool,
}
#[tauri::command]
pub async fn save_native_login_credentials(
    request: LoginCredentialsRequest,
) -> Result<usize, String> {
    native::reauthorize::bind(
        request.provider,
        request.id.as_deref(),
        &request.input,
        request.auto_reauthorize,
    )
}
/// Only the authenticated administrator explicitly requesting one row receives its secrets.
#[tauri::command]
pub async fn get_native_login_credentials(
    request: SessionRequest,
) -> Result<Option<native::reauthorize::LoginCredentials>, String> {
    Ok(native::store::get(request.provider, &request.id)?.login_credentials)
}
#[tauri::command]
pub async fn reauthorize_native_account(request: SessionRequest) -> Result<AccountView, String> {
    native::reauthorize::request(request.provider, &request.id)
}
#[tauri::command]
pub async fn import_native_login_accounts(
    request: LoginCredentialsRequest,
) -> Result<native::login_imports::Report, String> {
    native::login_imports::enqueue(request.provider, &request.input, request.auto_reauthorize)
}
#[tauri::command]
pub async fn list_native_login_imports(
    request: ProviderRequest,
) -> Result<Vec<native::login_imports::View>, String> {
    native::login_imports::list(request.provider)
}
#[tauri::command]
pub async fn retry_native_login_import(request: SessionRequest) -> Result<(), String> {
    native::login_imports::retry(request.provider, &request.id)
}
#[tauri::command]
pub async fn remove_native_login_import(request: SessionRequest) -> Result<(), String> {
    native::login_imports::remove(request.provider, &request.id)
}
#[tauri::command]
pub async fn delete_native_account(request: AccountRequest) -> Result<(), String> {
    native::store::remove(request.provider, &request.id)?;
    egress::forget(Pool::Native(request.provider), &request.id);
    native::http::forget(&request.id);
    Ok(())
}

#[derive(Deserialize)]
pub struct EgressSaveRequest {
    pub provider: Provider,
    #[serde(flatten)]
    pub update: egress::Update,
}
#[derive(Deserialize)]
pub struct AccountEgressRequest {
    pub provider: Provider,
    pub id: String,
    /// Node name, `DIRECT`, or null to let auto-assignment choose.
    pub node: Option<String>,
}
#[tauri::command]
pub async fn get_native_egress(request: ProviderRequest) -> Result<egress::View, String> {
    egress::view(Pool::Native(request.provider)).await
}
#[tauri::command]
pub async fn save_native_egress(request: EgressSaveRequest) -> Result<egress::View, String> {
    egress::save(Pool::Native(request.provider), request.update).await
}
#[tauri::command]
pub async fn refresh_native_egress(request: ProviderRequest) -> Result<egress::View, String> {
    egress::refresh(Pool::Native(request.provider)).await
}
#[tauri::command]
pub async fn test_native_egress_nodes(request: ProviderRequest) -> Result<egress::View, String> {
    egress::test_nodes(Pool::Native(request.provider)).await
}
#[tauri::command]
pub async fn set_native_account_egress(
    request: AccountEgressRequest,
) -> Result<egress::View, String> {
    egress::set_account(Pool::Native(request.provider), &request.id, request.node).await
}
#[tauri::command]
pub async fn test_native_account_egress(request: SessionRequest) -> Result<egress::Probe, String> {
    let account = native::store::get(request.provider, &request.id)?;
    let url = match request.provider {
        Provider::ChatGpt => "https://chatgpt.com/".to_owned(),
    };
    let direct_url = url.clone();
    egress::test_account(
        Pool::Native(request.provider),
        &request.id,
        &url,
        |exit| async move {
            let started = std::time::Instant::now();
            let timeout = std::time::Duration::from_secs(8);
            let reached = native::http::client_on(&account, &exit)
                .map_err(|e| e.to_string())?
                .head(&direct_url)
                .timeout(timeout)
                .send()
                .await
                .is_ok();
            reached
                .then(|| started.elapsed().as_millis() as u64)
                .ok_or_else(|| {
                    format!(
                        "{} is unreachable from this server",
                        request.provider.name()
                    )
                })
        },
    )
    .await
}

#[derive(Deserialize)]
pub struct ClientVersionCheck {
    #[serde(default)]
    pub refresh: bool,
}
#[derive(Deserialize)]
pub struct ClientVersionSave {
    /// A version to advertise, or null to return to the built-in default.
    pub version: Option<String>,
}
/// Advertised Codex CLI client version and the latest public Codex release.
#[tauri::command]
pub async fn check_chatgpt_client_version(
    request: ClientVersionCheck,
) -> Result<native::chatgpt::release::VersionView, String> {
    Ok(native::chatgpt::release::status(true, request.refresh).await)
}
/// Saves the advertised Codex client version. Each new request uses it at once, and every
/// account's model catalog is refetched on the next maintenance pass.
#[tauri::command]
pub async fn save_chatgpt_client_version(
    request: ClientVersionSave,
) -> Result<native::chatgpt::release::VersionView, String> {
    let version = request.version;
    tokio::task::spawn_blocking(move || {
        if native::chatgpt::release::save(version.as_deref())? {
            native::accounts::expire_catalogs(Provider::ChatGpt)?;
        }
        Ok::<_, String>(())
    })
    .await
    .map_err(|_| "Codex client version task failed")??;
    Ok(native::chatgpt::release::status(false, false).await)
}
