//! Desktop commands for administrator-defined OpenAI-compatible upstreams. The Web admin
//! API (`/api/custom-upstreams`) calls the same functions.
use crate::modules::custom_upstream::{self, SettingsUpdate, SettingsView};

#[tauri::command]
pub async fn get_custom_upstreams() -> Result<SettingsView, String> {
    tokio::task::spawn_blocking(custom_upstream::view)
        .await
        .map_err(|_| "Custom upstream settings task failed")?
}

#[tauri::command]
pub async fn save_custom_upstreams(request: SettingsUpdate) -> Result<SettingsView, String> {
    tokio::task::spawn_blocking(move || custom_upstream::save(request))
        .await
        .map_err(|_| "Custom upstream settings task failed")?
}
