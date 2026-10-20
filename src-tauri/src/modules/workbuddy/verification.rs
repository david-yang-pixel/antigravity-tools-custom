//! Explicit, bounded checks distinguish verified models from the candidate catalog.
use super::{accounts, client};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{LazyLock, Mutex},
    time::Duration,
};

const MAX_CHECKS: usize = 500;
static JOBS: LazyLock<Mutex<HashMap<PathBuf, Progress>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Default, Serialize)]
pub struct Progress {
    pub running: bool,
    pub total: usize,
    pub completed: usize,
    pub available: usize,
    pub unavailable: usize,
    pub failed: usize,
    pub current_model: Option<String>,
    pub started_at: i64,
    pub finished_at: i64,
    pub errors: Vec<String>,
}
fn update(path: &PathBuf, edit: impl FnOnce(&mut Progress)) {
    if let Ok(mut jobs) = JOBS.lock() {
        if let Some(job) = jobs.get_mut(path) {
            edit(job);
        }
    }
}
pub fn status() -> Result<Progress, String> {
    let path = crate::modules::account::get_data_dir()?;
    Ok(JOBS
        .lock()
        .map_err(|_| "WorkBuddy verification unavailable")?
        .get(&path)
        .cloned()
        .unwrap_or_default())
}
pub fn start() -> Result<Progress, String> {
    let path = crate::modules::account::get_data_dir()?;
    let settings = super::load()?;
    let accounts: Vec<_> = accounts::all()?
        .into_iter()
        .filter(|a| {
            a.enabled
                && settings
                    .preferred_account_id
                    .as_ref()
                    .is_none_or(|id| id == &a.id)
        })
        .collect();
    if accounts.is_empty() {
        return Err("Enable a WorkBuddy account before fetching models".into());
    }
    let initial = Progress {
        running: true,
        started_at: accounts::now(),
        ..Progress::default()
    };
    {
        let mut jobs = JOBS
            .lock()
            .map_err(|_| "WorkBuddy verification unavailable")?;
        if let Some(job) = jobs.get(&path).filter(|job| job.running) {
            return Ok(job.clone());
        }
        jobs.insert(path.clone(), initial.clone());
    }
    tauri::async_runtime::spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(1800), scan(&path, accounts)).await;
        update(&path, |job| {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => job.errors.push(error),
                Err(_) => job
                    .errors
                    .push("Model verification timed out; completed accounts were saved".into()),
            }
            job.running = false;
            job.current_model = None;
            job.finished_at = accounts::now();
        });
    });
    Ok(initial)
}

async fn scan(path: &PathBuf, accounts: Vec<accounts::Account>) -> Result<(), String> {
    let mut prepared = Vec::new();
    let mut total = 0;
    // Catalog and credits reads do not generate tokens. Finish discovery before probing.
    for original in accounts {
        if let Err(error) = accounts::sync(&original.id, false).await {
            update(path, |job| job.errors.push(error));
            continue;
        }
        let account = accounts::get(&original.id)?;
        if !account.enabled {
            continue;
        }
        if let Some(error) = &account.last_error {
            update(path, |job| job.errors.push(error.clone()));
            continue;
        }
        total += account.models.len();
        if total > MAX_CHECKS {
            return Err("Too many account/model pairs (maximum 500); select a fixed account and fetch again".into());
        }
        prepared.push(account);
    }
    update(path, |job| job.total = total);
    for account in prepared {
        let mut available = Vec::new();
        let mut unavailable = Vec::new();
        for model in &account.models {
            let current = accounts::get(&account.id)?;
            if !current.enabled || current.revision != account.revision {
                return Err(
                    "WorkBuddy account changed during verification; fetch models again".into(),
                );
            }
            update(path, |job| job.current_model = Some(model.id.clone()));
            let outcome = match accounts::scheduling::acquire_probe(&account, &model.id).await {
                Ok(lease) => {
                    // The lease reserves real pool capacity; model permission failures
                    // must not trip an account-wide circuit breaker or skew call health.
                    probe(&lease.account, &model.id).await
                }
                Err(_) => Outcome::Failed,
            };
            if matches!(outcome, Outcome::Available) {
                available.push(model.clone());
            } else if matches!(outcome, Outcome::Unavailable) {
                unavailable.push(model.id.clone());
            }
            update(path, |job| {
                job.completed += 1;
                match outcome {
                    Outcome::Available => job.available += 1,
                    Outcome::Unavailable => job.unavailable += 1,
                    Outcome::Failed => job.failed += 1,
                }
            });
        }
        accounts::save_verified_models(&account, &available, &unavailable)?;
        super::register_models(&available)?;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Available,
    Unavailable,
    Failed,
}
async fn probe(account: &accounts::Account, model: &str) -> Outcome {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    match tokio::time::timeout_at(deadline, async {
        let response = client::chat(account, model, json!({
            "messages":[{"role":"system","content":"You are a helpful assistant. Answer briefly."},
                {"role":"user","content":"Reply with only OK. Do not explain."}],
            "max_tokens":512,
        }), "model-verification", deadline).await.map_err(|_| ())?;
        if !response.status().is_success() {
            let body = axum::body::to_bytes(
                axum::body::Body::from_stream(response.bytes_stream()),
                64 * 1024,
            )
            .await
            .map_err(|_| ())?;
            let code = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| client::error_code(&v));
            // Permission denial / nonexistent model are conclusive. Network,
            // authentication, throttling and other failures must not revoke access.
            return Ok(if matches!(code, Some(11158 | 11102)) {
                Outcome::Unavailable
            } else {
                Outcome::Failed
            });
        }
        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
        {
            return Ok(Outcome::Failed);
        }
        let mut size = 0usize;
        let bytes = response.bytes_stream().map(move |chunk| {
            let chunk = chunk.map_err(|_| std::io::Error::other("interrupted"))?;
            size = size.saturating_add(chunk.len());
            if size > 256 * 1024 {
                return Err(std::io::Error::other("too large"));
            }
            Ok(chunk)
        });
        let mut events = bytes.eventsource();
        let mut text = false;
        let mut finished = false;
        while let Some(event) = events.next().await {
            let data = event.map_err(|_| ())?.data;
            if data == "[DONE]" {
                break;
            }
            if data.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&data).map_err(|_| ())?;
            let code = client::error_code(&value);
            if value.get("error").is_some_and(|error| !error.is_null())
                || code.is_some_and(|code| code != 0)
            {
                return Ok(if matches!(code, Some(11158 | 11102)) {
                    Outcome::Unavailable
                } else {
                    Outcome::Failed
                });
            }
            if let Some(choice) = value
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|v| v.first())
            {
                text |= choice
                    .pointer("/delta/content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty());
                finished |= choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason == "stop");
            }
        }
        Ok::<_, ()>(if text && finished {
            Outcome::Available
        } else {
            Outcome::Failed
        })
    })
    .await
    {
        Ok(Ok(outcome)) => outcome,
        _ => Outcome::Failed,
    }
}

#[cfg(test)]
#[path = "verification_tests.rs"]
mod tests;
