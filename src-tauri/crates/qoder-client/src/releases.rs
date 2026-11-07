//! Read public npm release metadata. Never download or execute a release, and never
//! use an account's transport, credentials, or advertised protocol version.
use semver::Version;
use serde::Serialize;
use std::{
    future::Future,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const REGISTRY_URL: &str = "https://registry.npmjs.org/-/package/@qoder-ai%2fqodercli/dist-tags";
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
const RETRY_TTL: Duration = Duration::from_secs(5 * 60);
const MIN_INTERVAL: Duration = Duration::from_secs(60);
const MAX_METADATA: usize = 64 * 1024;

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
    pub bundled_version: &'static str,
    pub latest_version: Option<String>,
    pub update_available: Option<bool>,
    /// Last successful check; retained on failure so stale results remain labelled.
    pub checked_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub error: Option<CheckError>,
}
impl Default for ReleaseStatus {
    fn default() -> Self {
        Self {
            bundled_version: crate::CLI_VERSION,
            latest_version: None,
            update_available: None,
            checked_at: None,
            last_attempt_at: None,
            error: None,
        }
    }
}

#[derive(Default)]
struct Cache {
    attempted_at: Option<Instant>,
    status: ReleaseStatus,
}
impl Cache {
    fn fresh(&self, refresh: bool, now: Instant) -> bool {
        let ttl = if refresh {
            MIN_INTERVAL
        } else if self.status.error.is_some() {
            RETRY_TTL
        } else {
            CACHE_TTL
        };
        self.attempted_at
            .is_some_and(|at| now.saturating_duration_since(at) < ttl)
    }
}

#[derive(Default)]
pub struct ReleaseChecker {
    cache: Mutex<Cache>,
}
impl ReleaseChecker {
    /// Coalesce concurrent callers. Explicit refresh bypasses the daily cache,
    /// subject to a one-minute floor even for repeated administrator requests.
    pub async fn check(&self, proxy: &str, refresh: bool) -> ReleaseStatus {
        self.check_using(refresh, || async {
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
            fetch(&client, REGISTRY_URL).await
        })
        .await
    }

    async fn check_using<F, Fut>(&self, refresh: bool, fetch: F) -> ReleaseStatus
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Version, CheckError>>,
    {
        let Ok(mut cache) = tokio::time::timeout(Duration::from_secs(15), self.cache.lock()).await
        else {
            return ReleaseStatus {
                error: Some(CheckError::Busy),
                ..Default::default()
            };
        };
        if cache.fresh(refresh, Instant::now()) {
            return cache.status.clone();
        }
        let result = tokio::time::timeout(Duration::from_secs(12), fetch())
            .await
            .unwrap_or(Err(CheckError::Timeout));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        cache.attempted_at = Some(Instant::now());
        cache.status.last_attempt_at = Some(now);
        match result {
            Ok(version) => {
                cache.status.update_available = Some(is_newer(&version, crate::CLI_VERSION));
                cache.status.latest_version = Some(version.to_string());
                cache.status.checked_at = Some(now);
                cache.status.error = None;
            }
            Err(error) => cache.status.error = Some(error),
        }
        cache.status.clone()
    }
}

fn is_newer(latest: &Version, bundled: &str) -> bool {
    Version::parse(bundled).is_ok_and(|current| latest.cmp_precedence(&current).is_gt())
}
fn parse_metadata(bytes: &[u8]) -> Result<Version, CheckError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| CheckError::InvalidMetadata)?;
    let latest = value
        .get("latest")
        .and_then(serde_json::Value::as_str)
        .filter(|v| v.len() <= 128)
        .ok_or(CheckError::InvalidMetadata)?;
    Version::parse(latest).map_err(|_| CheckError::InvalidMetadata)
}
async fn fetch(client: &reqwest::Client, url: &str) -> Result<Version, CheckError> {
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
    fn npm_latest_uses_semver_precedence_and_rejects_invalid_metadata() {
        for (latest, current, expected) in [
            ("1.10.0", "1.9.9", true),
            ("1.1.83", "1.1.83", false),
            ("1.1.82", "1.1.83", false),
            ("1.1.83-beta.1", "1.1.83", false),
            ("1.1.83+new-build", "1.1.83", false),
            ("1.1.84", "1.1.83", true),
        ] {
            let version = parse_metadata(
                format!("{{\"latest\":\"{latest}\",\"next\":\"99.0.0\"}}").as_bytes(),
            )
            .unwrap();
            assert_eq!(is_newer(&version, current), expected);
        }
        for bytes in [
            b"{}".as_slice(),
            b"{\"latest\":null}",
            b"{\"latest\":\"latest\"}",
            b"<html>error</html>",
        ] {
            assert_eq!(parse_metadata(bytes), Err(CheckError::InvalidMetadata));
        }
    }

    #[tokio::test]
    async fn checks_coalesce_and_failed_refresh_retains_last_verified_version() {
        let checker = ReleaseChecker::default();
        let calls = AtomicUsize::new(0);
        let check = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(Version::new(1, 2, 0))
        };
        let (first, second) = tokio::join!(
            checker.check_using(false, check),
            checker.check_using(false, check)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.latest_version, second.latest_version);
        assert_eq!(first.update_available, Some(true));
        checker.check_using(true, check).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        checker.cache.lock().await.attempted_at = Some(Instant::now() - MIN_INTERVAL);
        let failed = checker
            .check_using(true, || async { Err(CheckError::Network) })
            .await;
        assert_eq!(failed.error, Some(CheckError::Network));
        assert_eq!(failed.latest_version, first.latest_version);
        assert_eq!(failed.checked_at, first.checked_at);
        checker.check_using(false, check).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        checker.cache.lock().await.attempted_at = Some(Instant::now() - RETRY_TTL);
        assert!(checker.check_using(false, check).await.error.is_none());
        let mut cache = checker.cache.lock().await;
        let now = Instant::now();
        cache.attempted_at = Some(now - CACHE_TTL + Duration::from_secs(1));
        assert!(cache.fresh(false, now));
        cache.attempted_at = Some(now - CACHE_TTL);
        assert!(!cache.fresh(false, now));
    }

    #[tokio::test]
    async fn registry_responses_are_bounded_and_redirects_are_not_followed() {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        for (response, expected) in [
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 18\r\n\r\n{\"latest\":\"1.2.0\"}".to_owned(),
                Ok(Version::new(1, 2, 0)),
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
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(!request.contains("authorization:") && !request.contains("cosy-"));
                let _ = stream.write_all(response.as_bytes()).await;
            });
            assert_eq!(fetch(&client, &url).await, expected);
            task.await.unwrap();
        }
    }
}
