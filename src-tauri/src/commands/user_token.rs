use crate::modules::user_token_db::{self, TokenIpBinding, UserToken};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTokenRequest {
    pub username: String,
    pub expires_type: String,
    pub description: Option<String>,
    pub max_ips: i32,
    #[serde(default)]
    pub max_concurrency: i32,
    #[serde(default)]
    pub token_quota: i64,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    pub curfew_start: Option<String>,
    pub curfew_end: Option<String>,
    pub custom_expires_at: Option<i64>, // 自定义过期时间戳 (秒)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateTokenRequest {
    pub username: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub max_ips: Option<i32>,
    pub max_concurrency: Option<i32>,
    pub token_quota: Option<i64>,
    pub allowed_models: Option<Vec<String>>,
    pub curfew_start: Option<Option<String>>,
    pub curfew_end: Option<Option<String>>,
    /// Absent keeps the current expiry; "custom" requires `custom_expires_at`.
    pub expires_type: Option<String>,
    pub custom_expires_at: Option<i64>, // 自定义过期时间戳 (秒)
}

// 命令实现

/// 列出所有令牌
#[tauri::command]
pub async fn list_user_tokens() -> Result<Vec<UserToken>, String> {
    user_token_db::list_tokens()
}

/// 创建新令牌
#[tauri::command]
pub async fn create_user_token(request: CreateTokenRequest) -> Result<UserToken, String> {
    user_token_db::create_token(
        request.username,
        request.expires_type,
        request.description,
        request.max_ips,
        request.curfew_start,
        request.curfew_end,
        request.custom_expires_at,
        request.max_concurrency,
        request.token_quota,
        request.allowed_models,
    )
}

/// 更新令牌
#[tauri::command]
pub async fn update_user_token(id: String, request: UpdateTokenRequest) -> Result<(), String> {
    user_token_db::update_token(
        &id,
        request.username,
        request.description,
        request.enabled,
        request.max_ips,
        request.curfew_start,
        request.curfew_end,
        request.max_concurrency,
        request.token_quota,
        request.allowed_models,
        request
            .expires_type
            .map(|kind| (kind, request.custom_expires_at)),
    )
}

/// 删除令牌
#[tauri::command]
pub async fn delete_user_token(id: String) -> Result<(), String> {
    user_token_db::delete_token(&id)
}

/// 续期令牌
#[tauri::command]
pub async fn renew_user_token(id: String, expires_type: String) -> Result<(), String> {
    user_token_db::renew_token(&id, &expires_type)
}

/// 获取令牌 IP 绑定
#[tauri::command]
pub async fn get_token_ip_bindings(token_id: String) -> Result<Vec<TokenIpBinding>, String> {
    user_token_db::get_token_ips(&token_id)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserTokenStats {
    pub total_tokens: usize,
    pub active_tokens: usize,
    pub total_users: usize,
    pub today_requests: i64,
}

/// 获取简单的统计信息
#[tauri::command]
pub async fn get_user_token_summary() -> Result<UserTokenStats, String> {
    let tokens = user_token_db::list_tokens()?;
    let active_tokens = tokens.iter().filter(|t| t.enabled).count();

    // 统计唯一用户
    let mut users = std::collections::HashSet::new();
    for t in &tokens {
        users.insert(t.username.clone());
    }

    // 这里简单返回一些数据，请求数最好从数据库聚合查询
    // 目前仅作为演示，请求数暂不精确统计今日的

    Ok(UserTokenStats {
        total_tokens: tokens.len(),
        active_tokens,
        total_users: users.len(),
        today_requests: 0, // TODO: Implement daily stats query
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn user_token_legacy_payload_defaults_to_unlimited() {
        let create: CreateTokenRequest = serde_json::from_value(serde_json::json!({
            "username": "legacy", "expires_type": "never", "max_ips": 0
        }))
        .unwrap();
        assert_eq!((create.max_concurrency, create.token_quota), (0, 0));
        let update: UpdateTokenRequest =
            serde_json::from_value(serde_json::json!({"username": "edit"})).unwrap();
        assert_eq!((update.max_concurrency, update.token_quota), (None, None));
    }
}

#[tauri::command]
pub async fn get_user_token_models(
    state: tauri::State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<Vec<String>, String> {
    let config = crate::modules::config::load_app_config()?;
    let mapping = tokio::sync::RwLock::new(config.proxy.custom_mapping);
    let instance = state.instance.read().await;
    let manager = instance
        .as_ref()
        .map(|instance| instance.token_manager.as_ref());
    Ok(crate::proxy::common::model_mapping::get_all_dynamic_models(
        &mapping,
        manager,
        config.proxy.only_raw_quota_models,
    )
    .await)
}
