//! Public release metadata only: no account route, credentials, SDK or downloads.
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
const RETRY_TTL: Duration = Duration::from_secs(5 * 60);
const MIN_INTERVAL: Duration = Duration::from_secs(60);
const MAX_METADATA: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    #[default]
    Cn,
    Global,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum Platform {
    #[default]
    #[serde(rename = "linux-x64-deb")]
    LinuxX64,
    #[serde(rename = "linux-arm64-deb")]
    LinuxArm64,
    #[serde(rename = "darwin-x64")]
    MacX64,
    #[serde(rename = "darwin-arm64")]
    MacArm64,
    #[serde(rename = "win32-x64-user")]
    WindowsX64,
}
impl Platform {
    fn name(self) -> &'static str {
        match self {
            Self::LinuxX64 => "linux-x64-deb",
            Self::LinuxArm64 => "linux-arm64-deb",
            Self::MacX64 => "darwin-x64",
            Self::MacArm64 => "darwin-arm64",
            Self::WindowsX64 => "win32-x64-user",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::LinuxX64 => 0,
            Self::LinuxArm64 => 1,
            Self::MacX64 => 2,
            Self::MacArm64 => 3,
            Self::WindowsX64 => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Target {
    #[serde(default)]
    pub region: Region,
    #[serde(default)]
    pub platform: Platform,
}
impl Target {
    fn index(self) -> usize {
        self.platform.index() + if self.region == Region::Global { 5 } else { 0 }
    }
    fn url(self) -> String {
        let host = match self.region {
            Region::Cn => "www.codebuddy.cn",
            Region::Global => "www.workbuddy.ai",
        };
        format!(
            "https://{host}/v2/update?platform=workbuddy-{}",
            self.platform.name()
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckError {
    Network,
    Http,
    InvalidMetadata,
    Proxy,
    Timeout,
    Busy,
}

#[derive(Clone, Serialize)]
pub struct ReleaseStatus {
    pub target: Target,
    pub verified_target: Target,
    pub verified_for_target: bool,
    pub bundled_release: &'static str,
    pub latest_version: Option<String>,
    /// Unknown for a different distribution: its version is not our validated baseline.
    pub update_available: Option<bool>,
    pub checked_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub error: Option<CheckError>,
}

#[derive(Default)]
struct Cache {
    attempted_at: Option<Instant>,
    latest_version: Option<String>,
    checked_at: Option<u64>,
    last_attempt_at: Option<u64>,
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

pub struct ReleaseChecker {
    bundled_release: &'static str,
    verified_target: Target,
    // Fixed bounded channel set. Region/platform caches and locks never mix.
    caches: [Mutex<Cache>; 10],
}
impl ReleaseChecker {
    pub fn new(bundled_release: &'static str, verified_target: Target) -> Self {
        Self {
            bundled_release,
            verified_target,
            caches: std::array::from_fn(|_| Mutex::new(Cache::default())),
        }
    }

    pub async fn check(&self, target: Target, proxy: &str, refresh: bool) -> ReleaseStatus {
        self.check_using(target, refresh, || async {
            let builder = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10));
            let builder = if proxy.is_empty() {
                builder
            } else {
                builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| CheckError::Proxy)?)
            };
            let client = builder.build().map_err(|_| CheckError::Network)?;
            fetch(&client, &target.url()).await
        })
        .await
    }

    fn view(&self, target: Target, cache: &Cache) -> ReleaseStatus {
        let verified_for_target = target == self.verified_target;
        let update_available = if verified_for_target {
            cache
                .latest_version
                .as_deref()
                .and_then(version_parts)
                .zip(version_parts(self.bundled_release))
                .map(|(latest, current)| latest > current)
        } else {
            None
        };
        ReleaseStatus {
            target,
            verified_target: self.verified_target,
            verified_for_target,
            bundled_release: self.bundled_release,
            latest_version: cache.latest_version.clone(),
            update_available,
            checked_at: cache.checked_at,
            last_attempt_at: cache.last_attempt_at,
            error: cache.error,
        }
    }

    async fn check_using<F, Fut>(&self, target: Target, refresh: bool, fetch: F) -> ReleaseStatus
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String, CheckError>>,
    {
        let Ok(mut cache) =
            tokio::time::timeout(Duration::from_secs(15), self.caches[target.index()].lock()).await
        else {
            return self.view(
                target,
                &Cache {
                    error: Some(CheckError::Busy),
                    ..Default::default()
                },
            );
        };
        if cache.fresh(refresh, Instant::now()) {
            return self.view(target, &cache);
        }
        let result = tokio::time::timeout(Duration::from_secs(12), fetch())
            .await
            .unwrap_or(Err(CheckError::Timeout));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        cache.attempted_at = Some(Instant::now());
        cache.last_attempt_at = Some(now);
        match result {
            Ok(version) => {
                cache.latest_version = Some(version);
                cache.checked_at = Some(now);
                cache.error = None;
            }
            Err(error) => cache.error = Some(error),
        }
        self.view(target, &cache)
    }
}

/// Official releases include a fourth numeric build component; SemVer build
/// metadata and string ordering would miss same-product-version rebuilds.
fn version_parts(value: &str) -> Option<[u64; 4]> {
    if value.len() > 128 {
        return None;
    }
    let parts: Vec<_> = value.split('.').collect();
    if !(3..=4).contains(&parts.len()) {
        return None;
    }
    let mut parsed = [0; 4];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parsed[index] = part.parse().ok()?;
    }
    Some(parsed)
}
fn parse_metadata(bytes: &[u8]) -> Result<String, CheckError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| CheckError::InvalidMetadata)?;
    let version = value["version"]
        .as_str()
        .filter(|s| version_parts(s).is_some())
        .ok_or(CheckError::InvalidMetadata)?;
    Ok(version.into())
}
async fn fetch(client: &reqwest::Client, url: &str) -> Result<String, CheckError> {
    let mut response = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| CheckError::Network)?;
    if !response.status().is_success() {
        return Err(CheckError::Http);
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_METADATA as u64)
    {
        return Err(CheckError::InvalidMetadata);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| CheckError::Network)? {
        if bytes.len() + chunk.len() > MAX_METADATA {
            return Err(CheckError::InvalidMetadata);
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_metadata(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn workbuddy_release_targets_are_bounded_and_do_not_accept_user_supplied_hosts() {
        assert_eq!(
            serde_json::from_str::<Target>("{}").unwrap(),
            Target::default()
        );
        for body in [
            r#"{"region":"https://untrusted.invalid"}"#,
            r#"{"platform":"https://untrusted.invalid"}"#,
            r#"{"region":"cn","platform":"linux-x64-deb/../other"}"#,
        ] {
            assert!(serde_json::from_str::<Target>(body).is_err());
        }
        let mut indexes = std::collections::HashSet::new();
        for region in [Region::Cn, Region::Global] {
            for platform in [
                Platform::LinuxX64,
                Platform::LinuxArm64,
                Platform::MacX64,
                Platform::MacArm64,
                Platform::WindowsX64,
            ] {
                let target = Target { region, platform };
                assert!(indexes.insert(target.index()));
                assert!(target.index() < 10);
                assert!(
                    target.url().starts_with("https://www.codebuddy.cn/")
                        || target.url().starts_with("https://www.workbuddy.ai/")
                );
            }
        }
        assert_eq!(indexes.len(), 10);
    }

    #[test]
    fn workbuddy_release_numbers_compare_numeric_builds_and_reject_invalid_values() {
        for (latest, current, expected) in [
            ("5.10.0.1", "5.9.9.99999999", true),
            ("5.6.2.40046328", "5.6.2.40046327", true),
            ("5.6.2.40046327", "5.6.2.40046327", false),
            ("5.6.1.99999999", "5.6.2.40046327", false),
        ] {
            assert_eq!(
                version_parts(latest).unwrap() > version_parts(current).unwrap(),
                expected
            );
        }
        for body in [
            r#"{}"#,
            r#"{"version":null}"#,
            r#"{"version":"latest"}"#,
            r#"{"version":"5.6.2-beta"}"#,
            r#"{"version":"5.6.2.9999999999999999999999999999999999999"}"#,
        ] {
            assert_eq!(
                parse_metadata(body.as_bytes()),
                Err(CheckError::InvalidMetadata)
            );
        }
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/linux-5.6.2.json")).unwrap();
        assert_eq!(
            parse_metadata(fixture["release"].to_string().as_bytes()).unwrap(),
            "5.6.2.40046327"
        );
    }

    #[tokio::test]
    async fn workbuddy_release_cache_coalesces_keeps_stale_results_and_separates_distributions() {
        let checker = ReleaseChecker::new("5.6.2.40046327", Target::default());
        let target = Target::default();
        let calls = AtomicUsize::new(0);
        let check = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok("5.6.2.40046328".into())
        };
        let (a, b) = tokio::join!(
            checker.check_using(target, false, check),
            checker.check_using(target, false, check)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(a.latest_version, b.latest_version);
        assert_eq!(a.update_available, Some(true));
        checker.check_using(target, true, check).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        checker.caches[target.index()].lock().await.attempted_at =
            Some(Instant::now() - MIN_INTERVAL);
        let failed = checker
            .check_using(target, true, || async { Err(CheckError::Network) })
            .await;
        assert_eq!(failed.error, Some(CheckError::Network));
        assert_eq!(failed.latest_version, a.latest_version);
        assert_eq!(failed.checked_at, a.checked_at);
        checker.check_using(target, false, check).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        checker.caches[target.index()].lock().await.attempted_at = Some(Instant::now() - RETRY_TTL);
        assert!(checker
            .check_using(target, false, check)
            .await
            .error
            .is_none());
        let global = Target {
            region: Region::Global,
            ..target
        };
        let mac = Target {
            platform: Platform::MacArm64,
            ..target
        };
        for other in [global, mac] {
            let status = checker
                .check_using(other, false, || async { Ok("5.9.4.42593613".into()) })
                .await;
            assert!(!status.verified_for_target);
            assert_eq!(status.update_available, None);
            assert_ne!(other.url(), target.url());
        }
        assert_eq!(
            checker
                .check_using(target, false, check)
                .await
                .latest_version
                .as_deref(),
            Some("5.6.2.40046328")
        );
        let mut cache = checker.caches[target.index()].lock().await;
        let now = Instant::now();
        cache.attempted_at = Some(now - CACHE_TTL);
        assert!(!cache.fresh(false, now));
    }

    #[tokio::test]
    async fn workbuddy_release_fetch_bounds_bodies_rejects_redirects_and_sends_no_identity() {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        for (response, expected) in [
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 28\r\n\r\n{\"version\":\"5.6.2.40046327\"}"
                    .to_owned(),
                Ok("5.6.2.40046327".to_owned()),
            ),
            (
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n"
                    .to_owned(),
                Err(CheckError::Http),
            ),
            (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    MAX_METADATA + 1
                ),
                Err(CheckError::InvalidMetadata),
            ),
            (
                format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                    MAX_METADATA + 1,
                    "x".repeat(MAX_METADATA + 1)
                ),
                Err(CheckError::InvalidMetadata),
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                let request = String::from_utf8(request).unwrap().to_lowercase();
                for header in [
                    "authorization:",
                    "cookie:",
                    "x-user-id:",
                    "x-device-token:",
                    "x-machine-id:",
                ] {
                    assert!(!request.contains(header));
                }
                let _ = stream.write_all(response.as_bytes()).await;
            });
            assert_eq!(fetch(&client, &url).await, expected);
            task.await.unwrap();
        }
    }
}
