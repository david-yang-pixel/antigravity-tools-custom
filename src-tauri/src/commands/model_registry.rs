//! Thin desktop commands using the same application service as Web administration.
use crate::{
    commands::proxy::ProxyServiceState,
    modules::{model_registry::*, model_registry_service as service},
    proxy::{upstream::client::UpstreamClient, TokenManager},
};
use std::sync::Arc;
use tauri::State;

async fn runtime(
    state: &ProxyServiceState,
) -> Result<(Arc<TokenManager>, Arc<UpstreamClient>), String> {
    if let Some(instance) = state.instance.read().await.as_ref() {
        return Ok((
            instance.token_manager.clone(),
            instance.axum_server.upstream_client(),
        ));
    }
    if let Some(admin) = state.admin_server.read().await.as_ref() {
        return Ok((
            admin.axum_server.token_manager.clone(),
            admin.axum_server.upstream_client(),
        ));
    }
    let manager = Arc::new(TokenManager::new(crate::modules::account::get_data_dir()?));
    service::load_local_accounts(&manager).await?;
    let config = crate::modules::config::load_app_config()?;
    let upstream = Arc::new(UpstreamClient::new(Some(config.proxy.upstream_proxy), None));
    Ok((manager, upstream))
}

#[tauri::command]
pub async fn get_model_registry(
    state: State<'_, ProxyServiceState>,
) -> Result<ModelRegistryView, String> {
    let (manager, _) = runtime(&state).await?;
    service::view(&manager).await
}
#[tauri::command]
pub async fn save_model_registry(
    request: SaveModelRegistryRequest,
    state: State<'_, ProxyServiceState>,
) -> Result<ModelRegistryView, String> {
    let (manager, _) = runtime(&state).await?;
    service::save(&manager, request).await
}
#[tauri::command]
pub async fn refresh_model_candidates(
    request: RefreshModelCandidatesRequest,
    state: State<'_, ProxyServiceState>,
) -> Result<ModelRegistryView, String> {
    let (manager, _) = runtime(&state).await?;
    service::refresh(&manager, request).await
}
#[tauri::command]
pub async fn verify_model_candidates(
    request: VerifyModelCandidatesRequest,
    state: State<'_, ProxyServiceState>,
) -> Result<ModelVerificationReport, String> {
    let (manager, upstream) = runtime(&state).await?;
    service::verify(&manager, &upstream, request).await
}
