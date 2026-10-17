use crate::modules::portal_settings::{PortalSettingsUpdate, PortalSettingsView};
use crate::modules::user_token_db::{self, TokenIpBinding, UserToken};
use crate::modules::{portal_account, portal_mail, portal_settings};
use serde::{Deserialize, Serialize};

/// Admin list row: the key plus who created it.
#[derive(Debug, Serialize)]
pub struct UserTokenView {
    #[serde(flatten)]
    pub token: UserToken,
    /// "admin" for keys created here, "registered" for usage-portal accounts.
    pub source: &'static str,
    pub owner_email: Option<String>,
    pub account_status: Option<String>,
    /// Registered key still waiting for a quota.
    pub pending_activation: bool,
}

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
pub async fn list_user_tokens() -> Result<Vec<UserTokenView>, String> {
    let mut accounts = portal_account::accounts_by_id()?;
    Ok(user_token_db::list_tokens()?
        .into_iter()
        .map(|token| {
            let account = token
                .owner_user_id
                .as_deref()
                .and_then(|id| accounts.remove(id));
            UserTokenView {
                source: if token.owner_user_id.is_some() {
                    "registered"
                } else {
                    "admin"
                },
                pending_activation: token.owner_user_id.is_some()
                    && token.token_quota == 0
                    && account.as_ref().is_some_and(|a| a.is_active()),
                owner_email: account.as_ref().map(|a| a.email.clone()),
                account_status: account.map(|a| a.status),
                token,
            }
        })
        .collect())
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
    #[serde(default)]
    pub registered_tokens: usize,
    #[serde(default)]
    pub pending_tokens: usize,
}

/// 获取简单的统计信息
#[tauri::command]
pub async fn get_user_token_summary() -> Result<UserTokenStats, String> {
    let views = list_user_tokens().await?;
    let registered_tokens = views.iter().filter(|v| v.source == "registered").count();
    let pending_tokens = views.iter().filter(|v| v.pending_activation).count();
    let tokens: Vec<UserToken> = views.into_iter().map(|v| v.token).collect();
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
        registered_tokens,
        pending_tokens,
    })
}

// --- Usage-portal accounts (registered users) ---

/// Disabling turns the account's key off and signs it out everywhere.
#[tauri::command]
pub async fn set_portal_account_enabled(user_id: String, enabled: bool) -> Result<(), String> {
    portal_account::set_account_enabled(&user_id, enabled)
}

#[tauri::command]
pub async fn revoke_portal_sessions(user_id: String) -> Result<usize, String> {
    portal_account::revoke_sessions(&user_id)
}

/// Emails the account a password reset code (passwords are never visible here).
#[tauri::command]
pub async fn send_portal_reset_email(user_id: String) -> Result<(), String> {
    let account =
        portal_account::get_account(&user_id)?.ok_or_else(|| "Account not found".to_string())?;
    if !account.is_active() {
        return Err("Enable the account before sending a reset email".into());
    }
    let settings = portal_settings::load()?;
    let now = chrono::Utc::now().timestamp();
    let email = account.email.clone();
    let delivery = tokio::task::spawn_blocking(move || {
        portal_account::request_code(&email, portal_account::CodePurpose::Reset, now)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| match e.retry_after() {
        Some(secs) => format!("A code was sent recently; try again in {secs}s"),
        None => format!("Cannot issue a reset code: {}", e.code()),
    })?;
    let portal_account::CodeDelivery::Code { id, code } = delivery else {
        return Err("Cannot issue a reset code for this account".into());
    };
    let mail = portal_mail::code_mail(&settings.smtp, portal_account::CodePurpose::Reset, &code);
    if let Err(error) = portal_mail::send(&settings.smtp, &account.email, mail).await {
        let _ = portal_account::discard_code(&id);
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
pub async fn get_portal_settings() -> Result<PortalSettingsView, String> {
    portal_settings::view()
}

#[tauri::command]
pub async fn save_portal_settings(
    request: PortalSettingsUpdate,
) -> Result<PortalSettingsView, String> {
    portal_settings::save(request)
}

#[tauri::command]
pub async fn send_portal_test_email(to: String) -> Result<(), String> {
    let to =
        portal_account::normalize_email(&to).map_err(|_| "Invalid email address".to_string())?;
    let settings = portal_settings::load()?;
    portal_mail::send(&settings.smtp, &to, portal_mail::test_mail(&settings.smtp)).await
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
