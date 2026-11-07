//! One canonical Codex client identity on every chatgpt.com request.
use crate::modules::native_providers::Provider;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

pub const DEFAULT_CLIENT_VERSION: &str = "0.167.0";
/// The upstream answers 404 below this version.
const MIN_CLIENT_VERSION: (u64, u64, u64) = (0, 144, 0);
const ORIGINATOR: &str = "codex_cli_rs";

fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.').map(|p| p.parse::<u64>().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

fn pick(configured: Option<&str>) -> String {
    configured
        .filter(|v| parse(v).is_some_and(|v| v >= MIN_CLIENT_VERSION))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| DEFAULT_CLIENT_VERSION.into())
}

pub fn client_version() -> String {
    pick(
        std::env::var(Provider::ChatGpt.env("CLIENT_VERSION"))
            .ok()
            .as_deref(),
    )
}

pub fn user_agent(version: &str) -> String {
    format!("codex_cli_rs/{version} (Ubuntu 22.4.0; x86_64) xterm-256color")
}

/// Replaces any client-supplied identity headers. `version` is omitted for auth.openai.com.
pub fn apply_identity(h: &mut HeaderMap, with_version: bool) {
    let version = client_version();
    h.insert("originator", HeaderValue::from_static(ORIGINATOR));
    if let Ok(ua) = HeaderValue::from_str(&user_agent(&version)) {
        h.insert(reqwest::header::USER_AGENT, ua);
    }
    let name = HeaderName::from_static("version");
    match HeaderValue::from_str(&version) {
        Ok(v) if with_version => {
            h.insert(name, v);
        }
        _ => {
            h.remove(name);
        }
    }
}

#[cfg(test)]
pub(super) fn pick_version(configured: Option<&str>) -> String {
    pick(configured)
}
