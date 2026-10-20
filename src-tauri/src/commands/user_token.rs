use crate::modules::billing::{self, ModelPrice};
use crate::modules::portal_settings::{PortalSettingsUpdate, PortalSettingsView};
use crate::modules::user_token_db::{self, TokenIpBinding, UserToken};
use crate::modules::{portal_account, portal_mail, portal_settings};
use serde::{Deserialize, Deserializer, Serialize};

/// An omitted balance preserves it; explicit null is never a billing opt-out.
fn balance_update<'de, D>(deserializer: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: Deserializer<'de>,
{
    i64::deserialize(deserializer).map(|value| Some(Some(value)))
}

/// Admin list row: the key plus who created it.
#[derive(Debug, Serialize)]
pub struct UserTokenView {
    #[serde(flatten)]
    pub token: UserToken,
    /// "admin" for keys created here, "registered" for usage-portal accounts.
    pub source: &'static str,
    pub owner_email: Option<String>,
    pub account_status: Option<String>,
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
    pub free_models: Vec<String>,
    pub curfew_start: Option<String>,
    pub curfew_end: Option<String>,
    pub custom_expires_at: Option<i64>, // 自定义过期时间戳 (秒)
    /// Opening balance in micro-yuan; absent starts at zero and null is invalid.
    #[serde(default)]
    pub balance_micros: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateTokenRequest {
    pub username: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub max_ips: Option<i32>,
    pub max_concurrency: Option<i32>,
    pub free_models: Option<Vec<String>>,
    pub curfew_start: Option<Option<String>>,
    pub curfew_end: Option<Option<String>>,
    /// Absent keeps the current expiry; "custom" requires `custom_expires_at`.
    pub expires_type: Option<String>,
    pub custom_expires_at: Option<i64>, // 自定义过期时间戳 (秒)
    /// Absent keeps the balance, a number sets it (micro-yuan), and null is invalid.
    #[serde(default, deserialize_with = "balance_update")]
    pub balance_micros: Option<Option<i64>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveModelPricesRequest {
    pub prices: Vec<ModelPrice>,
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
    let mut conn = user_token_db::connect_db()?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let token = user_token_db::insert_token(
        &tx,
        user_token_db::NewToken {
            username: request.username,
            description: request.description,
            expires_type: request.expires_type,
            custom_expires_at: request.custom_expires_at,
            max_ips: request.max_ips,
            curfew_start: request.curfew_start,
            curfew_end: request.curfew_end,
            max_concurrency: request.max_concurrency,
            token_quota: 0,
            allowed_models: Vec::new(),
            free_models: request.free_models,
            owner_user_id: None,
            enabled: true,
            balance_micros: Some(request.balance_micros),
        },
    )?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(token)
}

/// 更新令牌
#[tauri::command]
pub async fn update_user_token(id: String, request: UpdateTokenRequest) -> Result<(), String> {
    user_token_db::update_token_with_policy(
        &id,
        request.username,
        request.description,
        request.enabled,
        request.max_ips,
        request.curfew_start,
        request.curfew_end,
        request.max_concurrency,
        None,
        None,
        request
            .expires_type
            .map(|kind| (kind, request.custom_expires_at)),
        request.balance_micros,
        request.free_models,
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
}

/// 获取简单的统计信息
#[tauri::command]
pub async fn get_user_token_summary() -> Result<UserTokenStats, String> {
    let views = list_user_tokens().await?;
    let registered_tokens = views.iter().filter(|v| v.source == "registered").count();
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

// --- Model prices (balance billing) ---

#[tauri::command]
pub async fn get_model_prices() -> Result<Vec<ModelPrice>, String> {
    billing::load_prices()
}

/// Replaces the whole price list.
#[tauri::command]
pub async fn save_model_prices(request: SaveModelPricesRequest) -> Result<Vec<ModelPrice>, String> {
    billing::save_prices(request.prices)
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
    use crate::proxy::monitor::prompt_log_tests::TestDataDir;
    #[test]
    fn user_token_legacy_payload_cannot_grant_free_models_or_disable_billing() {
        let create: CreateTokenRequest = serde_json::from_value(serde_json::json!({
            "username": "legacy", "expires_type": "never", "max_ips": 0,
            "token_quota": 100, "allowed_models": ["legacy-model"]
        }))
        .unwrap();
        assert_eq!(create.max_concurrency, 0);
        assert!(create.free_models.is_empty());
        assert_eq!(create.balance_micros, 0);
        let update: UpdateTokenRequest = serde_json::from_value(serde_json::json!({
            "username": "edit", "token_quota": 0, "allowed_models": ["legacy-model"]
        }))
        .unwrap();
        assert_eq!(update.max_concurrency, None);
        assert_eq!(update.free_models, None);
        assert_eq!(update.balance_micros, None);
        assert!(
            serde_json::from_value::<CreateTokenRequest>(serde_json::json!({
                "username": "null", "expires_type": "never", "max_ips": 0,
                "balance_micros": null
            }))
            .is_err()
        );
    }

    #[test]
    fn user_token_balance_edit_preserves_absent_and_rejects_null() {
        let parse = |body: serde_json::Value| {
            serde_json::from_value::<UpdateTokenRequest>(body)
                .unwrap()
                .balance_micros
        };
        assert_eq!(parse(serde_json::json!({})), None);
        assert!(serde_json::from_value::<UpdateTokenRequest>(
            serde_json::json!({"balance_micros": null})
        )
        .is_err());
        assert_eq!(
            parse(serde_json::json!({"balance_micros": 5})),
            Some(Some(5))
        );
    }

    #[tokio::test]
    async fn user_token_commands_store_independent_free_policy_and_keep_omitted_balance() {
        let _dir = TestDataDir::new();
        user_token_db::init_db().unwrap();
        let request = serde_json::from_value(serde_json::json!({
            "username": "billing-policy", "expires_type": "never", "max_ips": 0,
            "free_models": ["models/free-model", "free-model"],
            "allowed_models": ["legacy-model"], "token_quota": 100
        }))
        .unwrap();
        let token = create_user_token(request).await.unwrap();
        assert_eq!(token.balance_micros, Some(0));
        assert_eq!(token.free_models, vec!["free-model"]);
        assert!(token.allowed_models.is_empty());
        assert_eq!(token.token_quota, 0);
        update_user_token(
            token.id.clone(),
            serde_json::from_value(serde_json::json!({
                "balance_micros": 12345, "free_models": []
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        update_user_token(
            token.id.clone(),
            serde_json::from_value(serde_json::json!({
                "username": "renamed", "allowed_models": ["legacy-model"], "token_quota": 0
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let updated = user_token_db::get_token_by_id(&token.id).unwrap().unwrap();
        assert_eq!(updated.balance_micros, Some(12345));
        assert!(updated.free_models.is_empty() && updated.allowed_models.is_empty());
        assert_eq!(updated.token, token.token);
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
