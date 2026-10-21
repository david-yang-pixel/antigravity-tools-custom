//! Explicit, bounded checks distinguish verified models from the candidate catalog.
use super::{accounts, client};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize)]
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
/// Retained for older clients; broad implicit inference scans are no longer allowed.
pub fn status() -> Result<Progress, String> {
    Ok(Progress::default())
}
pub fn start() -> Result<Progress, String> {
    Err("Bulk verification was retired. Select 1–5 candidates in model management and explicitly confirm upstream usage.".into())
}

/// A single explicit basic-text probe. The caller enforces consent and action limits.
/// Shares the runtime pool's capacity and never registers or publishes an alias.
pub(crate) async fn verify_selected_model(model: &str) -> Result<(String, Outcome), String> {
    let settings = super::load()?;
    if !settings.enabled || !super::valid_model_id(model) {
        return Err("Enable the source and select a valid native model".into());
    }
    let account = accounts::all()?
        .into_iter()
        .find(|account| {
            account.enabled
                && settings
                    .preferred_account_id
                    .as_ref()
                    .is_none_or(|id| id == &account.id)
                && account.models.iter().any(|candidate| candidate.id == model)
        })
        .ok_or("No eligible account declares the selected model")?;
    let lease = accounts::scheduling::acquire_probe(&account, model).await?;
    let outcome = probe(&lease.account, model).await;
    match outcome {
        Outcome::Available => accounts::record_model_verification(&lease.account, model, true)?,
        Outcome::Unavailable => accounts::record_model_verification(&lease.account, model, false)?,
        Outcome::Failed => {} // Transient failures never revoke other successful evidence.
    }
    Ok((lease.account.id.clone(), outcome))
}

#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
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
