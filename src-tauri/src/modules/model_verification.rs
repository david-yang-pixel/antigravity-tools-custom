//! Account-specific inference evidence, kept separate from the upstream candidate catalog.
use crate::{models::Account, proxy::TokenManager};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Verified,
    Unsupported,
    Inconclusive,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Observation {
    pub outcome: Outcome,
    pub checked_at: i64,
    pub last_success_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Evidence {
    pub identity: String,
    pub models: HashMap<String, Observation>,
}

// Access-token refresh does not change account identity; re-importing credentials
// or changing the project does. Never persist credentials in verification results.
fn identity(account: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!([
                account["id"],
                account["email"],
                account["created_at"],
                account["token"]["refresh_token"],
                account["token"]["project_id"],
                account["token"]["oauth_client_key"]
            ]))
            .expect("JSON values are serializable")
        )
    )
}

/// Older quota/token refreshes must not overwrite newer verification evidence.
pub(crate) fn preserve_newer_evidence(account: &mut Account, persisted: &Value) {
    // Generic account saves may be stale. Only the dedicated probe mutation can
    // introduce evidence; ordinary refresh/import writes retain persisted proof.
    account.model_verification = serde_json::to_value(&*account)
        .ok()
        .filter(|value| identity(value) == identity(persisted))
        .and_then(|_| current_evidence(persisted));
}

pub(crate) fn current_evidence(account: &Value) -> Option<Evidence> {
    account
        .get("model_verification")
        .and_then(|value| serde_json::from_value::<Evidence>(value.clone()).ok())
        .filter(|evidence| evidence.identity == identity(account))
}
pub fn verified_model_ids(account: &Value) -> HashSet<String> {
    let Some(evidence) = current_evidence(account) else {
        return HashSet::new();
    };
    account["quota"]["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["name"].as_str())
        .filter(|model| {
            evidence.models.get(*model).is_some_and(|observation| {
                observation.outcome != Outcome::Unsupported && observation.last_success_at.is_some()
            })
        })
        .map(str::to_owned)
        .collect()
}

pub(super) fn record(original: &Account, model: &str, outcome: Outcome) -> Result<(), String> {
    let _lock = super::account::lock_account_file_updates()?;
    super::account::mutate_account_file(&original.id, |current| {
        let expected = identity(&serde_json::to_value(original).map_err(|e| e.to_string())?);
        let current_identity =
            identity(&serde_json::to_value(&current).map_err(|e| e.to_string())?);
        if expected != current_identity || !eligible(&current) {
            return Err(
                "Account changed during model verification; please run verification again".into(),
            );
        }
        if !current
            .quota
            .as_ref()
            .is_some_and(|quota| quota.models.iter().any(|item| item.name == model))
        {
            return Err("Candidate model changed during verification".into());
        }
        let evidence = current
            .model_verification
            .get_or_insert_with(Evidence::default);
        if evidence.identity != expected {
            *evidence = Evidence {
                identity: expected,
                ..Evidence::default()
            };
        }
        let now = chrono::Utc::now().timestamp_millis().max(
            evidence
                .models
                .values()
                .map(|o| o.checked_at)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        );
        let last_success_at = match outcome {
            Outcome::Verified => Some(now),
            Outcome::Unsupported => None,
            // Throttling, bad parameters, expired credentials and transport failures
            // cannot prove that a previously working model lost its permission.
            Outcome::Inconclusive => evidence.models.get(model).and_then(|o| o.last_success_at),
        };
        evidence.models.insert(
            model.into(),
            Observation {
                outcome,
                checked_at: now,
                last_success_at,
            },
        );
        Ok(())
    })?;
    crate::proxy::server::trigger_account_reload(&original.id);
    Ok(())
}

pub(crate) fn eligible(account: &Account) -> bool {
    !account.disabled
        && !account.proxy_disabled
        && !account.validation_blocked
        && !account
            .quota
            .as_ref()
            .is_some_and(|quota| quota.is_forbidden)
}

#[derive(Clone, Default, Serialize)]
pub struct Progress {
    pub running: bool,
    pub total: usize,
    pub completed: usize,
    pub verified: usize,
    pub unsupported: usize,
    pub inconclusive: usize,
    pub current_model: Option<String>,
    pub started_at: i64,
    pub finished_at: i64,
    pub errors: Vec<String>,
}

static JOBS: LazyLock<Mutex<HashMap<PathBuf, Progress>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn update(path: &PathBuf, edit: impl FnOnce(&mut Progress)) {
    if let Ok(mut jobs) = JOBS.lock() {
        if let Some(job) = jobs.get_mut(path) {
            edit(job);
        }
    }
}

pub fn status() -> Result<Progress, String> {
    let path = super::account::get_data_dir()?;
    Ok(JOBS
        .lock()
        .map_err(|_| "Model verification unavailable")?
        .get(&path)
        .cloned()
        .unwrap_or_default())
}

pub fn start(
    manager: Arc<TokenManager>,
    upstream: Arc<crate::proxy::upstream::client::UpstreamClient>,
    account_ids: Option<Vec<String>>,
    model_ids: Option<Vec<String>>,
) -> Result<Progress, String> {
    let path = super::account::get_data_dir()?;
    let accounts: Vec<_> = super::account::list_accounts()?
        .into_iter()
        .filter(|account| {
            eligible(account)
                && account_ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&account.id))
        })
        .collect();
    let total = accounts
        .iter()
        .map(|account| {
            account.quota.as_ref().map_or(0, |quota| {
                quota
                    .models
                    .iter()
                    .filter(|model| {
                        model_ids
                            .as_ref()
                            .is_none_or(|ids| ids.contains(&model.name))
                    })
                    .count()
            })
        })
        .sum::<usize>();
    if total == 0 {
        return Err(
            "Enable an Antigravity account and refresh its candidate models before verification"
                .into(),
        );
    }
    if total > 500 {
        return Err(
            "Select fewer accounts: at most 500 account/model pairs per verification".into(),
        );
    }
    let initial = Progress {
        running: true,
        total,
        started_at: chrono::Utc::now().timestamp(),
        ..Progress::default()
    };
    {
        let mut jobs = JOBS.lock().map_err(|_| "Model verification unavailable")?;
        if let Some(job) = jobs.get(&path).filter(|job| job.running) {
            return Ok(job.clone());
        }
        jobs.insert(path.clone(), initial.clone());
    }
    tauri::async_runtime::spawn(async move {
        // One probe at a time, bounded by both a per-request and whole-job deadline.
        let result = tokio::time::timeout(Duration::from_secs(1800), async {
            manager.refresh_pending_accounts().await;
            for account in accounts {
                let candidates: Vec<_> = account
                    .quota
                    .as_ref()
                    .map(|q| q.models.clone())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|model| {
                        model_ids
                            .as_ref()
                            .is_none_or(|ids| ids.contains(&model.name))
                    })
                    .collect();
                if candidates.is_empty() {
                    continue;
                }
                let account =
                    match tokio::time::timeout(Duration::from_secs(30), prepare(&account)).await {
                        Ok(Ok(account)) => account,
                        _ => {
                            update(&path, |job| {
                                job.completed += candidates.len();
                                job.inconclusive += candidates.len();
                                job.errors
                                    .push(format!("Could not prepare account {}", account.id));
                            });
                            continue;
                        }
                    };
                manager.reload_account(&account.id).await?;
                for model in candidates {
                    let current = super::account::load_account(&account.id)?;
                    if !eligible(&current)
                        || identity(&serde_json::to_value(&current).map_err(|e| e.to_string())?)
                            != identity(&serde_json::to_value(&account).map_err(|e| e.to_string())?)
                    {
                        return Err("Account changed during verification".to_string());
                    }
                    update(&path, |job| job.current_model = Some(model.name.clone()));
                    let outcome = probe(&upstream, &account, &model.name).await;
                    record(&account, &model.name, outcome)?;
                    update(&path, |job| {
                        job.completed += 1;
                        match outcome {
                            Outcome::Verified => job.verified += 1,
                            Outcome::Unsupported => job.unsupported += 1,
                            Outcome::Inconclusive => job.inconclusive += 1,
                        }
                    });
                }
            }
            Ok::<_, String>(())
        })
        .await;
        update(&path, |job| {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => job.errors.push(error),
                Err(_) => job
                    .errors
                    .push("Verification timed out; completed results were saved".into()),
            }
            job.running = false;
            job.current_model = None;
            job.finished_at = chrono::Utc::now().timestamp();
        });
    });
    Ok(initial)
}

fn request_body(model: &str, project: &str, account: &str) -> Value {
    // This exercises the same Gemini adapter and pipeline while keeping the ID exact.
    let mut body = crate::proxy::mappers::gemini::wrapper::wrap_request_with_routing(
        &json!({
            "contents": [{"role":"user","parts":[{"text":"Reply with only OK."}]}],
            "generationConfig": {"maxOutputTokens":512}
        }),
        project,
        model,
        Some(account),
        Some(&uuid::Uuid::new_v4().to_string()),
        None,
        None,
        None,
        None,
        crate::proxy::common::model_mapping::ModelRoutingMode::Exact,
    );
    // The adapter may expand an incoming output limit for thinking. Bound the
    // final envelope instead, preserving maxOutputTokens > thinkingBudget.
    if let Some(config) = body
        .pointer_mut("/request/generationConfig")
        .and_then(Value::as_object_mut)
    {
        if let Some(thinking) = config
            .get_mut("thinkingConfig")
            .and_then(Value::as_object_mut)
        {
            thinking.remove("thinkingLevel");
            thinking.insert("thinkingBudget".into(), json!(1024));
            config.insert("maxOutputTokens".into(), json!(2048));
        } else {
            config.insert("maxOutputTokens".into(), json!(512));
        }
    }
    body
}

async fn prepare(original: &Account) -> Result<Account, String> {
    let mut token =
        crate::modules::oauth::ensure_fresh_token(&original.token, Some(&original.id)).await?;
    if token
        .project_id
        .as_ref()
        .is_none_or(|project| project.is_empty())
    {
        token.project_id = crate::modules::quota::fetch_project_id(
            &token.access_token,
            &original.email,
            Some(&original.id),
        )
        .await
        .0;
    }
    if token
        .project_id
        .as_ref()
        .is_none_or(|project| project.is_empty())
    {
        return Err("Account has no verified project".into());
    }
    let _lock = super::account::lock_account_file_updates()?;
    super::account::mutate_account_file(&original.id, |current| {
        let expected = identity(&serde_json::to_value(original).map_err(|e| e.to_string())?);
        if identity(&serde_json::to_value(&*current).map_err(|e| e.to_string())?) != expected
            || !eligible(current)
        {
            return Err("Account changed during verification".into());
        }
        current.token = token;
        if identity(&serde_json::to_value(&*current).map_err(|e| e.to_string())?) != expected {
            current.model_verification = None;
        }
        Ok(())
    })
}

fn classify_error(status: u16, body: &Value) -> Outcome {
    if !matches!(status, 400 | 403 | 404) {
        return Outcome::Inconclusive;
    }
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let explicit_reason = body
        .pointer("/error/details")
        .and_then(Value::as_array)
        .is_some_and(|details| {
            details.iter().any(|item| {
                matches!(
                    item["reason"].as_str(),
                    Some("MODEL_NOT_FOUND" | "MODEL_NOT_SUPPORTED" | "MODEL_ACCESS_DENIED")
                )
            })
        });
    let model_rejection = [
        "model not found",
        "model is not found",
        "model is not supported",
        "model not supported",
        "model does not exist",
        "model is not available",
        "not authorized to access model",
        "no access to model",
        "permission denied for model",
    ]
    .iter()
    .any(|pattern| message.contains(pattern));
    if explicit_reason || model_rejection {
        Outcome::Unsupported
    } else {
        Outcome::Inconclusive
    }
}

fn has_output(value: &Value) -> bool {
    let response = value.get("response").unwrap_or(value);
    response["candidates"].as_array().is_some_and(|candidates| {
        candidates.iter().any(|candidate| {
            candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part["thought"] != true
                            && (part["text"]
                                .as_str()
                                .is_some_and(|text| !text.trim().is_empty())
                                || part
                                    .pointer("/inlineData/data")
                                    .and_then(Value::as_str)
                                    .is_some_and(|data| !data.is_empty()))
                    })
                })
        })
    })
}

async fn read_response(response: rquest::Response) -> Outcome {
    let status = response.status().as_u16();
    let stream = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let mut size = 0usize;
    let bytes = response.bytes_stream().map(move |chunk| {
        let chunk = chunk.map_err(|_| std::io::Error::other("interrupted"))?;
        size = size.saturating_add(chunk.len());
        if size > 1024 * 1024 {
            return Err(std::io::Error::other("response too large"));
        }
        Ok(chunk)
    });
    if !(200..300).contains(&status) || !stream {
        let Ok(body) =
            axum::body::to_bytes(axum::body::Body::from_stream(bytes), 1024 * 1024).await
        else {
            return Outcome::Inconclusive;
        };
        let Ok(body) = serde_json::from_slice::<Value>(&body) else {
            return Outcome::Inconclusive;
        };
        return if (200..300).contains(&status) && body.get("error").is_none() && has_output(&body) {
            Outcome::Verified
        } else {
            classify_error(status, &body)
        };
    }
    let mut events = bytes.eventsource();
    let mut output = false;
    let mut finished = false;
    while let Some(event) = events.next().await {
        let Ok(event) = event else {
            return Outcome::Inconclusive;
        };
        if event.data.trim() == "[DONE]" {
            break;
        }
        if event.data.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            return Outcome::Inconclusive;
        };
        if value.get("error").is_some() {
            return classify_error(
                value
                    .pointer("/error/code")
                    .and_then(Value::as_u64)
                    .unwrap_or(500) as u16,
                &value,
            );
        }
        output |= has_output(&value);
        finished |= value.get("response").unwrap_or(&value)["candidates"]
            .as_array()
            .is_some_and(|items| {
                items.iter().any(|item| {
                    matches!(item["finishReason"].as_str(), Some("STOP" | "MAX_TOKENS"))
                })
            });
    }
    if output && finished {
        Outcome::Verified
    } else {
        Outcome::Inconclusive
    }
}

async fn probe(
    upstream: &crate::proxy::upstream::client::UpstreamClient,
    account: &Account,
    model: &str,
) -> Outcome {
    tokio::time::timeout(Duration::from_secs(30), async {
        let Some(project) = account
            .token
            .project_id
            .as_deref()
            .filter(|id| !id.is_empty())
        else {
            return Outcome::Inconclusive;
        };
        let body = request_body(model, project, &account.id);
        match upstream
            .call_v1_internal(
                "streamGenerateContent",
                &account.token.access_token,
                body,
                Some("alt=sse"),
                Some(&account.id),
            )
            .await
        {
            Ok(result) => read_response(result.response).await,
            Err(_) => Outcome::Inconclusive,
        }
    })
    .await
    .unwrap_or(Outcome::Inconclusive)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn seed_verified_fixture(account: &mut Value, models: &[&str]) {
    let evidence = Evidence {
        identity: identity(account),
        models: models
            .iter()
            .map(|model| {
                (
                    model.to_string(),
                    Observation {
                        outcome: Outcome::Verified,
                        checked_at: 1,
                        last_success_at: Some(1),
                    },
                )
            })
            .collect(),
    };
    account["model_verification"] = serde_json::to_value(evidence).unwrap();
}
