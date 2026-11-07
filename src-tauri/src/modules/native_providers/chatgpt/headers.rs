//! One canonical Codex client identity on every chatgpt.com request.
use crate::modules::native_providers::Provider;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

pub const DEFAULT_CLIENT_VERSION: &str = "0.167.0";
/// The upstream answers 404 below this version.
const MIN_CLIENT_VERSION: (u64, u64, u64) = (0, 144, 0);
const ORIGINATOR: &str = "codex_cli_rs";

/// Strict `x.y.z`: a pre-release or build suffix is never a stable Codex release.
pub fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.').map(|p| p.parse::<u64>().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

/// `version` trimmed, if it parses and is not below the upstream floor.
pub fn valid(version: &str) -> Option<String> {
    parse(version)
        .is_some_and(|v| v >= MIN_CLIENT_VERSION)
        .then(|| version.trim().to_string())
}

pub fn min_version() -> String {
    let (major, minor, patch) = MIN_CLIENT_VERSION;
    format!("{major}.{minor}.{patch}")
}

/// Whether `candidate` is a higher version than `current`; `None` if either is unparsable.
pub fn newer(candidate: &str, current: &str) -> Option<bool> {
    Some(parse(candidate)? > parse(current)?)
}

#[cfg(test)]
fn pick(configured: Option<&str>) -> String {
    configured
        .and_then(valid)
        .unwrap_or_else(|| DEFAULT_CLIENT_VERSION.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionSource {
    /// `ABV_CHATGPT_CLIENT_VERSION`: headless deployments pin it; the UI cannot change it.
    Env,
    /// Saved from the accounts page.
    Saved,
    Default,
}

/// The advertised version and where it came from: a valid environment override, then the
/// saved choice, then the built-in default. Invalid or too-old values fall through.
pub fn resolve() -> (String, VersionSource) {
    if let Some(v) = std::env::var(Provider::ChatGpt.env("CLIENT_VERSION"))
        .ok()
        .as_deref()
        .and_then(valid)
    {
        return (v, VersionSource::Env);
    }
    if let Some(v) = super::release::saved() {
        return (v, VersionSource::Saved);
    }
    (DEFAULT_CLIENT_VERSION.into(), VersionSource::Default)
}

pub fn client_version() -> String {
    resolve().0
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
