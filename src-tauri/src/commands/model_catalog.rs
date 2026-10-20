use crate::modules::model_catalog::{self, Candidate, Provider};
#[derive(serde::Deserialize)]
pub struct FetchRequest {
    pub provider: Provider,
    #[serde(default)]
    pub refresh: bool,
}
#[derive(serde::Deserialize)]
pub struct EditRequest {
    pub provider: Provider,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}
#[derive(serde::Deserialize)]
pub struct TestRequest {
    pub provider: Provider,
    pub model_ids: Vec<String>,
}
#[derive(serde::Deserialize)]
pub struct StatusRequest {
    pub provider: Provider,
}

#[tauri::command]
pub async fn get_model_candidates(request: FetchRequest) -> Result<Vec<Candidate>, String> {
    model_catalog::fetch(request.provider, request.refresh).await
}
#[tauri::command]
pub async fn edit_published_models(request: EditRequest) -> Result<Vec<Candidate>, String> {
    model_catalog::edit(request.provider, request.add, request.remove)
}
#[tauri::command]
pub async fn get_model_test_status(provider: Provider) -> Result<serde_json::Value, String> {
    model_catalog::test_status(provider)
}
#[tauri::command]
pub async fn start_model_tests(
    state: tauri::State<'_, crate::commands::proxy::ProxyServiceState>,
    request: TestRequest,
) -> Result<serde_json::Value, String> {
    let instance = state.instance.read().await;
    let Some(instance) = instance.as_ref() else {
        return Err("Start the proxy before testing models".into());
    };
    instance
        .axum_server
        .start_catalog_test(request.provider, request.model_ids)
}
