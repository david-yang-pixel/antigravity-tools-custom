//! Explicit model probes no longer send a generated chat to WorkBuddy.
use serde::Serialize;

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

pub(crate) const PROBE_DISABLED: &str =
    "不再向上游发送生成的验证短句。模型列表只来自目录同步；是否可调用以下游真实请求为准。";

/// WorkBuddy availability is not checked with a canned prompt.
pub(crate) async fn verify_selected_model(
    model: &str,
    account_id: Option<&str>,
) -> Result<(), String> {
    let _ = (model, account_id);
    Err(PROBE_DISABLED.into())
}

#[cfg(test)]
#[path = "verification_tests.rs"]
mod tests;
