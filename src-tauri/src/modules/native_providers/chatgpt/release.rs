//! The Codex CLI client version advertised to chatgpt.com: the administrator's saved choice
//! behind `headers::client_version`, and a check of the latest public Codex release
//! (npm registry first, GitHub releases as fallback). Public metadata only: nothing is
//! downloaded or installed, and no account credential is sent.
use super::headers::{self, VersionSource};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

const CACHE_TTL: Duration = Duration::from_secs(6 * 3600);
const RETRY_TTL: Duration = Duration::from_secs(5 * 60);
const MIN_INTERVAL: Duration = Duration::from_secs(30);
const MAX_METADATA: usize = 1024 * 1024;
const NPM_LATEST: &str = "https://registry.npmjs.org/@openai/codex/latest";
const GITHUB_LATEST: &str = "https://api.github.com/repos/openai/codex/releases/latest";
const FILE: &str = "chatgpt_client.json";

#[derive(Default, Deserialize, Serialize)]
struct Stored {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_version: Option<String>,
}

/// Saved version, cached per data directory so a data-dir switch (or a test sandbox)
/// never serves another directory's choice.
static SAVED: Mutex<Option<(PathBuf, Option<String>)>> = Mutex::new(None);

fn path() -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join(FILE))
}
fn read(path: &PathBuf) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let stored: Stored = serde_json::from_slice(&bytes).ok()?;
    stored.client_version.as_deref().and_then(headers::valid)
}

/// The saved client version, if any and still above the upstream floor.
pub fn saved() -> Option<String> {
    let path = path().ok()?;
    let mut slot = SAVED.lock().ok()?;
    if let Some((cached_for, version)) = slot.as_ref() {
        if *cached_for == path {
            return version.clone();
        }
    }
    let version = read(&path);
    *slot = Some((path, version.clone()));
    version
}

/// Saves `version` (or clears it with `None`, returning to the built-in default).
/// Returns whether the advertised version changed.
pub fn save(version: Option<&str>) -> Result<bool, String> {
    let (before, source) = headers::resolve();
    if source == VersionSource::Env {
        return Err(format!(
            "Codex client version is managed by {}",
            crate::modules::native_providers::Provider::ChatGpt.env("CLIENT_VERSION")
        ));
    }
    let version = match version.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => Some(headers::valid(v).ok_or_else(|| {
            format!(
                "Codex client version must be x.y.z and at least {}",
                headers::min_version()
            )
        })?),
        None => None,
    };
    let path = path()?;
    let bytes = serde_json::to_vec_pretty(&Stored {
        client_version: version.clone(),
    })
    .map_err(|_| "Cannot encode Codex client version")?;
    let mut slot = SAVED
        .lock()
        .map_err(|_| "Codex client version unavailable")?;
    crate::utils::fs::write_atomic(&path, &bytes)?;
    *slot = Some((path, version));
    drop(slot);
    Ok(headers::client_version() != before)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckError {
    Network,
    Http,
    InvalidMetadata,
    Proxy,
    Timeout,
}

#[derive(Default)]
struct Cache {
    attempted_at: Option<Instant>,
    latest_version: Option<String>,
    checked_at: Option<i64>,
    error: Option<CheckError>,
}
impl Cache {
    fn fresh(&self, refresh: bool, now: Instant) -> bool {
        let ttl = if refresh {
            MIN_INTERVAL
        } else if self.error.is_some() {
            RETRY_TTL
        } else {
            CACHE_TTL
        };
        self.attempted_at
            .is_some_and(|at| now.saturating_duration_since(at) < ttl)
    }
}
static CACHE: LazyLock<tokio::sync::Mutex<Cache>> = LazyLock::new(Default::default);

#[derive(Serialize)]
pub struct VersionView {
    /// The version every chatgpt.com request advertises right now.
    pub current: String,
    pub source: VersionSource,
    pub default_version: &'static str,
    pub min_version: String,
    pub latest_version: Option<String>,
    /// Unknown until a check has succeeded.
    pub update_available: Option<bool>,
    pub checked_at: Option<i64>,
    pub error: Option<CheckError>,
}

fn view(cache: &Cache) -> VersionView {
    let (current, source) = headers::resolve();
    VersionView {
        update_available: cache
            .latest_version
            .as_deref()
            .and_then(|latest| headers::newer(latest, &current)),
        current,
        source,
        default_version: headers::DEFAULT_CLIENT_VERSION,
        min_version: headers::min_version(),
        latest_version: cache.latest_version.clone(),
        checked_at: cache.checked_at,
        error: cache.error,
    }
}

/// Current state with the cached release check; `check` queries upstream when the cache is
/// stale (`refresh` shortens the cache to a short anti-hammering interval).
pub async fn status(check: bool, refresh: bool) -> VersionView {
    let mut cache = CACHE.lock().await;
    if check && !cache.fresh(refresh, Instant::now()) {
        let result = tokio::time::timeout(Duration::from_secs(20), fetch_latest())
            .await
            .unwrap_or(Err(CheckError::Timeout));
        cache.attempted_at = Some(Instant::now());
        match result {
            Ok(version) => {
                cache.latest_version = Some(version);
                cache.checked_at = Some(chrono::Utc::now().timestamp());
                cache.error = None;
            }
            Err(error) => cache.error = Some(error),
        }
    }
    view(&cache)
}

fn proxy() -> String {
    crate::modules::config::load_app_config()
        .ok()
        .map(|config| config.proxy.upstream_proxy)
        .filter(|proxy| proxy.enabled)
        .map(|proxy| proxy.url.trim().to_owned())
        .unwrap_or_default()
}

async fn fetch_latest() -> Result<String, CheckError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(8))
        .user_agent("antigravity-tools");
    let proxy = proxy();
    if !proxy.is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(&proxy).map_err(|_| CheckError::Proxy)?);
    }
    let client = builder.build().map_err(|_| CheckError::Network)?;
    match fetch(&client, NPM_LATEST).await.and_then(|b| parse_npm(&b)) {
        Ok(version) => Ok(version),
        Err(npm_error) => fetch(&client, GITHUB_LATEST)
            .await
            .and_then(|b| parse_github(&b))
            .map_err(|_| npm_error),
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, CheckError> {
    let mut response = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                CheckError::Timeout
            } else {
                CheckError::Network
            }
        })?;
    if !response.status().is_success() {
        return Err(CheckError::Http);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| CheckError::Network)? {
        if body.len() + chunk.len() > MAX_METADATA {
            return Err(CheckError::InvalidMetadata);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Stable releases only: a pre-release tag (`0.170.0-alpha.1`) is never offered.
fn stable(version: &str) -> Result<String, CheckError> {
    headers::parse(version)
        .map(|_| version.trim().to_string())
        .ok_or(CheckError::InvalidMetadata)
}
pub(super) fn parse_npm(bytes: &[u8]) -> Result<String, CheckError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| CheckError::InvalidMetadata)?;
    stable(value["version"].as_str().unwrap_or_default())
}
/// Codex tags its CLI releases `rust-v<version>`.
pub(super) fn parse_github(bytes: &[u8]) -> Result<String, CheckError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| CheckError::InvalidMetadata)?;
    if value["prerelease"].as_bool() == Some(true) {
        return Err(CheckError::InvalidMetadata);
    }
    let tag = value["tag_name"].as_str().unwrap_or_default();
    stable(
        tag.strip_prefix("rust-v")
            .or_else(|| tag.strip_prefix('v'))
            .unwrap_or(tag),
    )
}
