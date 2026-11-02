//! Shared Web/Tauri application service. Discovery never generates model tokens.
use super::model_registry::{
    self, CandidateModel, ModelRegistryView, ModelVerificationResult, SaveModelRegistryRequest,
};
pub use super::model_registry::{
    DiscoverUpstreamModelsRequest, ModelVerificationReport, RefreshModelCandidatesRequest,
    UpstreamDiscoveryReport, VerifyModelCandidatesRequest,
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
    for upstream in super::custom_upstream::load()?
        .upstreams
        .into_iter()
        .filter(|u| u.enabled && selected(&u.source_id()))
    {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            failed += 1;
            break;
        }
        let result = tokio::time::timeout(
            remaining.min(Duration::from_secs(20)),
            super::custom_upstream::fetch_models(&upstream),
        )
        .await;
        match result {
            Ok(Ok(models)) => {
                super::custom_upstream::record_catalog(&upstream.id, &models)?;
                refreshed += 1;
            }
            _ => failed += 1,
        }
    }
    let mut registry = view(manager).await?;
    registry.notices.push(format!("目录同步完成：{refreshed} 个账号成功，{failed} 个失败或超时。未发起模型推理，未自动上架新模型。"));
    Ok(registry)
}

/// Accounts tried per discovery: the random sample plus fallbacks when it fails or lists nothing.
const MAX_DISCOVERY_ATTEMPTS: usize = 3;

struct Sampled {
    account_id: String,
    account_label: String,
    models: Vec<String>,
    attempts: usize,
}

fn shuffled<T>(mut items: Vec<T>) -> Vec<T> {
    use rand::seq::SliceRandom;
    items.shuffle(&mut rand::thread_rng());
    items
}

/// Explicit metadata-only discovery through one randomly sampled account of the selected
/// source (and WorkBuddy region). Another account is tried only when the sample fails or
/// lists no models. No inference is made and nothing is published.
pub async fn discover(
    manager: &TokenManager,
    request: DiscoverUpstreamModelsRequest,
) -> Result<UpstreamDiscoveryReport, String> {
    let _guard = DISCOVERY_LOCK
        .try_lock()
        .map_err(|_| "Model discovery is already running".to_string())?;
    model_registry::validate_discovery_request(&request)?;
    view(manager).await?; // Initialize publication BEFORE discovering new candidates.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let sampled = if request.source_id == model_registry::ANTIGRAVITY_SOURCE {
        discover_antigravity(manager, deadline).await?
    } else if let Some(id) = super::custom_upstream::upstream_id(&request.source_id) {
        discover_custom(id).await?
    } else {
        discover_workbuddy(request.realm, deadline).await?
    };
    let mut registry = view(manager).await?;
    let candidate_ids: Vec<String> = sampled
        .models
        .iter()
        .filter_map(|native| {
            registry
                .candidates
                .iter()
                .find(|c| c.source_id == request.source_id && &c.native_model == native)
                .map(|c| c.id.clone())
        })
        .collect();
    registry.notices.push(format!(
        "已从随机抽取的账号获取 {} 个上游模型（尝试 {} 个账号）。未发起模型推理，未自动上架新模型。",
        candidate_ids.len(),
        sampled.attempts
    ));
    Ok(UpstreamDiscoveryReport {
        source_id: request.source_id,
        realm: request.realm,
        account_id: sampled.account_id,
        account_label: sampled.account_label,
        attempts: sampled.attempts,
        candidate_ids,
        registry,
    })
}

async fn discover_antigravity(
    manager: &TokenManager,
    deadline: tokio::time::Instant,
) -> Result<Sampled, String> {
    let accounts = shuffled(
        super::account::list_accounts()?
            .into_iter()
            .filter(|a| !a.disabled && !a.proxy_disabled)
            .collect(),
    );
    if accounts.is_empty() {
        return Err("No enabled Antigravity account".into());
    }
    let mut attempts = 0;
    let mut found = None;
    for account in accounts.into_iter().take(MAX_DISCOVERY_ATTEMPTS) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        attempts += 1;
        let result = tokio::time::timeout(remaining.min(Duration::from_secs(15)), async {
            let (token, _, _, _, _) = manager.get_token_by_email(&account.email).await?;
            let (quota, _) = super::quota::fetch_quota(&token, &account.email, Some(&account.id))
                .await
                .map_err(|e| e.to_string())?;
            let models: Vec<String> = quota.models.iter().map(|m| m.name.clone()).collect();
            super::account::update_account_quota(&account.id, quota)?;
            Ok::<_, String>(models)
        })
        .await;
        if let Ok(Ok(models)) = result {
            if !models.is_empty() {
                found = Some((account, models));
                break;
            }
        }
    }
    load_local_accounts(manager).await?;
    let (account, models) = found.ok_or_else(|| {
        format!("No sampled Antigravity account returned models ({attempts} tried)")
    })?;
    Ok(Sampled {
        account_id: account.id,
        account_label: account.email,
        models,
        attempts,
    })
}

/// A custom upstream has one credential, so "sampling" is a single `/models` call; declared
/// model IDs are listed even when the upstream has no model list.
async fn discover_custom(id: &str) -> Result<Sampled, String> {
    let upstream = super::custom_upstream::get(id)?.ok_or("Unknown custom upstream")?;
    let fetched = super::custom_upstream::fetch_models(&upstream).await;
    let fetched = match fetched {
        Ok(models) => {
            super::custom_upstream::record_catalog(&upstream.id, &models)?;
            models
        }
        // Declared models keep the upstream usable without a /models endpoint.
        Err(_) if !upstream.models.is_empty() => Vec::new(),
        Err(message) => return Err(message),
    };
    let mut models = upstream.models.clone();
    models.extend(fetched.into_iter().filter(|m| !upstream.models.contains(m)));
    Ok(Sampled {
        account_id: upstream.id.clone(),
        account_label: upstream.display_name().to_string(),
        models,
        attempts: 1,
    })
}

async fn discover_workbuddy(
    realm: Option<super::workbuddy::accounts::Realm>,
    deadline: tokio::time::Instant,
) -> Result<Sampled, String> {
    let preferred = super::workbuddy::load()?.preferred_account_id;
    // A preferred account restricts routing, so models of other accounts are not routable.
    let accounts = shuffled(
        super::workbuddy::accounts::list()?
            .into_iter()
            .filter(|a| {
                a.enabled
                    && realm.is_none_or(|realm| realm == a.realm)
                    && preferred.as_ref().is_none_or(|id| id == &a.id)
            })
            .collect(),
    );
    if accounts.is_empty() {
        return Err("No enabled WorkBuddy account in this region (or the preferred account is in another region)".into());
    }
    let mut attempts = 0;
    for account in accounts.into_iter().take(MAX_DISCOVERY_ATTEMPTS) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        attempts += 1;
        let result = tokio::time::timeout(
            remaining.min(Duration::from_secs(15)),
            super::workbuddy::accounts::sync(&account.id, false),
        )
        .await;
        // Sync reports credential and catalog failures through last_error, not Err.
        if !matches!(&result, Ok(Ok(view)) if view.last_error.is_none()) {
            continue;
        }
        let models: Vec<String> = super::workbuddy::accounts::get(&account.id)?
            .models
            .into_iter()
            .map(|m| m.id)
            .collect();
        if !models.is_empty() {
            return Ok(Sampled {
                account_label: if account.nickname.is_empty() {
                    account.uid
                } else {
                    account.nickname
                },
                account_id: account.id,
                models,
                attempts,
            });
        }
    }
    Err(format!(
        "No sampled WorkBuddy account returned models ({attempts} tried)"
    ))
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
    // WorkBuddy is never probed with a canned prompt. Refuse before anything is
    // recorded, so the registry does not show a failure that never happened.
    if selected
        .iter()
        .any(|candidate| candidate.source_id == model_registry::WORKBUDDY_SOURCE)
    {
        return Err(super::workbuddy::verification::PROBE_DISABLED.into());
    }
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
            probe(manager, upstream, &candidate, request.account_id.as_deref()),
        )
        .await;
        let (status, message, account_id) = match result {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                tracing::warn!(
                    "Model probe for {} did not complete: {}",
                    candidate.native_model,
                    crate::proxy::upstream::client::sanitize_error_for_log(&error)
                );
                (
                    "failed".to_string(),
                    "基础文本验证未完成：账号、网络或服务暂不可用；未判定模型永久不可用。"
                        .to_string(),
                    None,
                )
            }
            Err(_) => {
                tracing::warn!("Model probe for {} timed out", candidate.native_model);
                (
                    "failed".to_string(),
                    "基础文本验证超时；未判定模型永久不可用。".to_string(),
                    None,
                )
            }
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
    account_id: Option<&str>,
) -> Result<(String, String, Option<String>), String> {
    match candidate.source_id.as_str() {
        // `verify` refuses WorkBuddy candidates before probing.
        model_registry::WORKBUDDY_SOURCE => {
            Err(super::workbuddy::verification::PROBE_DISABLED.into())
        }
        model_registry::ANTIGRAVITY_SOURCE => {
            probe_antigravity(manager, upstream, candidate, account_id).await
        }
        source if super::custom_upstream::is_source(source) => probe_custom(candidate).await,
        _ => Err("This source has no basic-text verification adapter".into()),
    }
}

async fn probe_custom(
    candidate: &CandidateModel,
) -> Result<(String, String, Option<String>), String> {
    let id = super::custom_upstream::upstream_id(&candidate.source_id)
        .ok_or("Unknown custom upstream")?;
    let upstream = super::custom_upstream::get(id)?.ok_or("Unknown custom upstream")?;
    if !upstream.enabled {
        return Err("Enable the upstream before verifying its models".into());
    }
    use super::custom_upstream::Probe;
    let (status, message) =
        match super::custom_upstream::probe(&upstream, &candidate.native_model).await? {
            Probe::Verified => ("verified", "上游基础文本调用通过；其他能力未验证。"),
            Probe::Unavailable => ("unavailable", "上游明确返回模型不存在。"),
            Probe::Failed(detail) => {
                tracing::warn!(
                    "Model probe for {} on custom upstream {} failed: {}",
                    candidate.native_model,
                    upstream.id,
                    detail
                );
                ("failed", "上游调用失败或暂不可用；未判定模型永久不可用。")
            }
        };
    Ok((status.into(), message.into(), Some(upstream.id)))
}

/// Logs at most this much of a probe response; enough to see the upstream's reason.
const PROBE_LOG_BYTES: usize = 4096;

fn probe_body_for_log(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(PROBE_LOG_BYTES)]);
    match text.trim() {
        "" => "<empty body>".into(),
        text => crate::proxy::upstream::client::sanitize_error_for_log(text),
    }
}

/// Reads a bounded prefix of a rejected probe response; the caller's timeout bounds the wait.
async fn probe_detail(mut response: rquest::Response) -> String {
    let mut bytes = Vec::new();
    while bytes.len() < PROBE_LOG_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(_) => return "<body interrupted>".into(),
        }
    }
    probe_body_for_log(&bytes)
}

async fn probe_antigravity(
    manager: &TokenManager,
    upstream: &UpstreamClient,
    candidate: &CandidateModel,
    account_id: Option<&str>,
) -> Result<(String, String, Option<String>), String> {
    // Never silently invoke image generation under a basic-text verification action.
    let lower = candidate.native_model.to_lowercase();
    if lower.contains("image") || lower.contains("dall-e") || lower.contains("midjourney") {
        tracing::info!(
            "Model probe skipped {}: image models need a dedicated check",
            candidate.native_model
        );
        return Ok((
            "failed".into(),
            "该模型不能用安全的基础文本探测确认；未发起图片生成，仍需专项验证。".into(),
            None,
        ));
    }
    let accounts = super::account::list_accounts()?;
    // Candidates are keyed by the quota entry's `name`, which is the API model ID.
    // Its `model` field is an internal enum (e.g. MODEL_PLACEHOLDER_M35) that the
    // upstream rejects as a model name, so it must never be sent.
    let account = accounts
        .into_iter()
        .find(|account| {
            account_id.is_none_or(|id| id == account.id)
                && !account.disabled
                && !account.proxy_disabled
                && account.quota.as_ref().is_some_and(|quota| {
                    quota
                        .models
                        .iter()
                        .any(|model| model.name == candidate.native_model)
                })
        })
        .ok_or("No enabled account declares this model")?;
    let (token, project, _, account_id, _) = manager.get_token_by_email(&account.email).await?;
    let body = json!({
        "project": project, "requestId": format!("registry-verify-{}", uuid::Uuid::new_v4()),
        "model": candidate.native_model, "userAgent": "antigravity", "requestType": "agent",
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
        tracing::warn!(
            "Model probe for {} on account {} got HTTP {}: {}",
            candidate.native_model,
            account_id,
            status.as_u16(),
            probe_detail(response).await
        );
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
    if !text {
        tracing::warn!(
            "Model probe for {} on account {} returned no answer text: {}",
            candidate.native_model,
            account_id,
            probe_body_for_log(&bytes)
        );
    }
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
            account_id: None,
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

    #[test]
    fn model_probe_log_is_bounded_and_redacted() {
        assert_eq!(probe_body_for_log(b"  \n"), "<empty body>");
        let logged = probe_body_for_log(br#"{"error":"bad model","auth":"Bearer ya29.secret"}"#);
        assert!(logged.contains("bad model") && !logged.contains("ya29.secret"));
        assert!(probe_body_for_log(&[b'x'; PROBE_LOG_BYTES * 2]).len() <= PROBE_LOG_BYTES);
    }
}

#[cfg(test)]
#[path = "model_registry_service_tests.rs"]
mod integration_tests;
