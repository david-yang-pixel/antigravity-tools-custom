//! Shared Web/Tauri application service. Discovery never generates model tokens.
use super::model_registry::{
    self, CandidateModel, ModelRegistryView, ModelVerificationResult, SaveModelRegistryRequest,
};
pub use super::model_registry::{
    ModelVerificationReport, RefreshModelCandidatesRequest, VerifyModelCandidatesRequest,
};
use crate::proxy::{upstream::client::UpstreamClient, TokenManager};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::LazyLock, time::Duration};
use tokio::sync::Mutex;

static VERIFICATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static DISCOVERY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

async fn context(
    manager: &TokenManager,
) -> Result<(Vec<String>, std::collections::HashMap<String, String>), String> {
    let config = super::config::load_app_config()?;
    let mapping = config.proxy.custom_mapping;
    let legacy = crate::proxy::common::model_mapping::get_legacy_dynamic_models(
        &tokio::sync::RwLock::new(mapping.clone()),
        Some(manager),
        config.proxy.only_raw_quota_models,
    )
    .await;
    Ok((legacy, mapping))
}

pub async fn view(manager: &TokenManager) -> Result<ModelRegistryView, String> {
    let (legacy, mapping) = context(manager).await?;
    model_registry::view(&legacy, &mapping)
}

/// Missing AG accounts are a valid WorkBuddy-only/first-run installation.
pub async fn load_local_accounts(manager: &TokenManager) -> Result<usize, String> {
    let path = super::account::get_data_dir()?.join("accounts");
    if path
        .try_exists()
        .map_err(|_| "Cannot inspect account directory")?
    {
        manager.load_accounts().await
    } else {
        Ok(0)
    }
}

pub async fn desktop_view() -> Result<ModelRegistryView, String> {
    let manager = TokenManager::new(super::account::get_data_dir()?);
    load_local_accounts(&manager).await?;
    view(&manager).await
}

pub async fn save(
    manager: &TokenManager,
    request: SaveModelRegistryRequest,
) -> Result<ModelRegistryView, String> {
    let (legacy, mapping) = context(manager).await?;
    model_registry::save(request, &legacy, &mapping)
}

/// Explicit metadata-only refresh, with independent bounds for each account and
/// the whole action. No warmup or model-verification task is started here.
pub async fn refresh(
    manager: &TokenManager,
    request: RefreshModelCandidatesRequest,
) -> Result<ModelRegistryView, String> {
    let _guard = DISCOVERY_LOCK
        .try_lock()
        .map_err(|_| "Model discovery is already running".to_string())?;
    model_registry::validate_refresh_request(&request)?;
    let before = view(manager).await?; // Initialize publication BEFORE discovering new candidates.
    if request
        .source_id
        .as_ref()
        .is_some_and(|id| !before.sources.iter().any(|source| &source.id == id))
    {
        return Err("Unknown upstream source".into());
    }
    let selected = |source: &str| request.source_id.as_deref().is_none_or(|id| id == source);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut refreshed = 0usize;
    let mut failed = 0usize;
    if selected(model_registry::ANTIGRAVITY_SOURCE) {
        for account in super::account::list_accounts()?
            .into_iter()
            .filter(|a| !a.disabled && !a.proxy_disabled)
            .take(100)
        {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                failed += 1;
                break;
            }
            let result = tokio::time::timeout(remaining.min(Duration::from_secs(15)), async {
                let (token, _, _, _, _) = manager.get_token_by_email(&account.email).await?;
                let (quota, _) =
                    super::quota::fetch_quota(&token, &account.email, Some(&account.id))
                        .await
                        .map_err(|e| e.to_string())?;
                super::account::update_account_quota(&account.id, quota)
            })
            .await;
            if matches!(result, Ok(Ok(()))) {
                refreshed += 1;
            } else {
                failed += 1;
            }
        }
        load_local_accounts(manager).await?;
    }
    if selected(model_registry::WORKBUDDY_SOURCE) {
        for account in super::workbuddy::accounts::list()?
            .into_iter()
            .filter(|a| a.enabled)
            .take(100)
        {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                failed += 1;
                break;
            }
            let result = tokio::time::timeout(
                remaining.min(Duration::from_secs(15)),
                super::workbuddy::accounts::sync(&account.id, false),
            )
            .await;
            if matches!(result, Ok(Ok(_))) {
                refreshed += 1;
            } else {
                failed += 1;
            }
        }
    }
    let mut registry = view(manager).await?;
    registry.notices.push(format!("目录同步完成：{refreshed} 个账号成功，{failed} 个失败或超时。未发起模型推理，未自动上架新模型。"));
    Ok(registry)
}

pub fn validate_verification_request(request: &VerifyModelCandidatesRequest) -> Result<(), String> {
    if !request.confirm_upstream_usage {
        return Err("Confirm upstream quota/cost before verifying models".into());
    }
    if request.candidate_ids.is_empty() || request.candidate_ids.len() > 5 {
        return Err("Select between 1 and 5 candidates for basic-text verification".into());
    }
    let unique: HashSet<_> = request.candidate_ids.iter().collect();
    if unique.len() != request.candidate_ids.len() {
        return Err("Duplicate model candidates are not allowed".into());
    }
    Ok(())
}

/// This is an explicit admin action, never called by list/read or startup paths.
/// Only one candidate action runs per process; each model is sampled once, not
/// retried across the account pool. Historical evidence is never fabricated.
pub async fn verify(
    manager: &TokenManager,
    upstream: &UpstreamClient,
    request: VerifyModelCandidatesRequest,
) -> Result<ModelVerificationReport, String> {
    validate_verification_request(&request)?;
    let _guard = VERIFICATION_LOCK
        .try_lock()
        .map_err(|_| "Model verification is already running".to_string())?;
    let registry = view(manager).await?;
    model_registry::validate_verification_request(&request, &registry)?;
    let selected: Vec<_> = request
        .candidate_ids
        .iter()
        .map(|id| {
            registry
                .candidates
                .iter()
                .find(|candidate| &candidate.id == id)
                .cloned()
                .ok_or_else(|| "Unknown model candidate; refresh the directory".to_string())
        })
        .collect::<Result<_, _>>()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut results = Vec::new();
    for candidate in selected {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let checked_at = chrono::Utc::now().timestamp();
        let result = tokio::time::timeout(
            remaining.min(Duration::from_secs(30)),
            probe(manager, upstream, &candidate),
        )
        .await;
        let (status, message, account_id) = match result {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => (
                "failed".to_string(),
                "基础文本验证未完成：账号、网络或服务暂不可用；未判定模型永久不可用。".to_string(),
                None,
            ),
            Err(_) => (
                "failed".to_string(),
                "基础文本验证超时；未判定模型永久不可用。".to_string(),
                None,
            ),
        };
        let result = model_registry::sanitize_verification_result(ModelVerificationResult {
            candidate_id: candidate.id,
            status,
            message,
            checked_at,
            account_id,
        })?;
        model_registry::record_verification(result.clone())?;
        results.push(result);
    }
    let mut registry = view(manager).await?;
    registry.notices.push("验证仅证明所抽样账号的基础文本调用，不代表所有账号、思考控制、视觉或工具能力；验证不会自动上架。".into());
    if results.len() < request.candidate_ids.len() {
        registry
            .notices
            .push("已达到本次验证总时限，剩余模型未执行；已完成结果保留。".into());
    }
    Ok(ModelVerificationReport { results, registry })
}

async fn probe(
    manager: &TokenManager,
    upstream: &UpstreamClient,
    candidate: &CandidateModel,
) -> Result<(String, String, Option<String>), String> {
    match candidate.source_id.as_str() {
        model_registry::WORKBUDDY_SOURCE => {
            let (account_id, outcome) =
                super::workbuddy::verification::verify_selected_model(&candidate.native_model)
                    .await?;
            use super::workbuddy::verification::Outcome;
            let (status, message) = match outcome {
                Outcome::Available => (
                    "verified",
                    "一个启用账号的基础文本调用通过；其他能力未验证。",
                ),
                Outcome::Unavailable => (
                    "unavailable",
                    "抽样账号明确无此模型权限；不代表其他账号均不可用。",
                ),
                Outcome::Failed => (
                    "failed",
                    "抽样调用失败或暂不可用；未撤销其他账号的成功证据。",
                ),
            };
            Ok((status.into(), message.into(), Some(account_id)))
        }
        model_registry::ANTIGRAVITY_SOURCE => probe_antigravity(manager, upstream, candidate).await,
        _ => Err("This source has no basic-text verification adapter".into()),
    }
}

async fn probe_antigravity(
    manager: &TokenManager,
    upstream: &UpstreamClient,
    candidate: &CandidateModel,
) -> Result<(String, String, Option<String>), String> {
    // Never silently invoke image generation under a basic-text verification action.
    let lower = candidate.native_model.to_lowercase();
    if lower.contains("image") || lower.contains("dall-e") || lower.contains("midjourney") {
        return Ok((
            "failed".into(),
            "该模型不能用安全的基础文本探测确认；未发起图片生成，仍需专项验证。".into(),
            None,
        ));
    }
    let accounts = super::account::list_accounts()?;
    let account = accounts
        .into_iter()
        .find(|account| {
            !account.disabled
                && !account.proxy_disabled
                && account.quota.as_ref().is_some_and(|quota| {
                    quota.models.iter().any(|model| {
                        model.name == candidate.native_model
                            || model.model.as_deref() == Some(candidate.native_model.as_str())
                    })
                })
        })
        .ok_or("No enabled account declares this model")?;
    let native = account
        .quota
        .as_ref()
        .and_then(|quota| {
            quota
                .models
                .iter()
                .find(|model| model.name == candidate.native_model)
        })
        .and_then(|model| model.model.clone())
        .unwrap_or_else(|| candidate.native_model.clone());
    let (token, project, _, account_id, _) = manager.get_token_by_email(&account.email).await?;
    let body = json!({
        "project": project, "requestId": format!("registry-verify-{}", uuid::Uuid::new_v4()),
        "model": native, "userAgent": "antigravity", "requestType": "agent",
        "request": {
            "contents": [{"role": "user", "parts": [{"text": "Reply with only OK."}]}],
            "generationConfig": {"maxOutputTokens": 512, "temperature": 0}
        }
    });
    let response = upstream
        .call_v1_internal_once("generateContent", &token, body, Some(&account_id))
        .await?
        .response;
    let status = response.status();
    if !status.is_success() {
        // An HTTP endpoint error alone is not conclusive model-permission evidence.
        let state = "failed";
        return Ok((
            state.into(),
            format!(
                "抽样账号基础文本验证返回 HTTP {}；未自动上架或更改其他账号。",
                status.as_u16()
            ),
            Some(account_id),
        ));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Verification response interrupted")?
    {
        if bytes.len().saturating_add(chunk.len()) > 256 * 1024 {
            return Err("Verification response exceeded size bound".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Verification response was not valid JSON")?;
    let inner = value.get("response").unwrap_or(&value);
    let text = inner
        .get("candidates")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.pointer("/content/parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part.get("thought").and_then(Value::as_bool) != Some(true)
                                && part
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| !text.trim().is_empty())
                        })
                    })
            })
        });
    Ok((
        if text { "verified" } else { "failed" }.into(),
        if text {
            "一个启用账号的基础文本调用通过；未验证思考控制、图片或工具。"
        } else {
            "上游未返回可确认的文本；未判定模型永久不可用。"
        }
        .into(),
        Some(account_id),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_registry_verification_requires_explicit_bounded_confirmation() {
        let request = |ids, confirm| VerifyModelCandidatesRequest {
            candidate_ids: ids,
            confirm_upstream_usage: confirm,
        };
        assert!(validate_verification_request(&request(vec!["a".into()], false)).is_err());
        assert!(validate_verification_request(&request(vec![], true)).is_err());
        assert!(validate_verification_request(&request(vec!["a".into(); 2], true)).is_err());
        assert!(validate_verification_request(&request(
            (0..6).map(|i| i.to_string()).collect(),
            true
        ))
        .is_err());
        assert!(
            validate_verification_request(&request(vec!["a".into(), "b".into()], true)).is_ok()
        );
    }
}

#[cfg(test)]
#[path = "model_registry_service_tests.rs"]
mod integration_tests;
