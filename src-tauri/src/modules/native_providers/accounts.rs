use super::{
    http::{self, Failure},
    *,
};
#[path = "accounts_scheduler.rs"]
pub mod scheduling;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex, Weak,
    },
};

#[derive(Clone, Default)]
pub struct Runtime {
    pub in_flight: usize,
    pub cooldown_until: i64,
    pub last_error: Option<String>,
    pub successes: u64,
    pub failures: u64,
    /// Last completed generation; background polls follow real use.
    pub last_generation_at: i64,
    consecutive_failures: u64,
    sequence: u64,
}
static RUNTIME: LazyLock<Mutex<HashMap<String, Runtime>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REFRESH_LOCKS: LazyLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn runtime(id: &str) -> Runtime {
    RUNTIME
        .lock()
        .ok()
        .and_then(|r| r.get(id).cloned())
        .unwrap_or_default()
}
fn refresh_lock(id: &str) -> Result<Arc<tokio::sync::Mutex<()>>, String> {
    let mut locks = REFRESH_LOCKS
        .lock()
        .map_err(|_| "Account refresh unavailable")?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = locks
        .get(id)
        .and_then(Weak::upgrade)
        .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
    locks.insert(id.into(), Arc::downgrade(&lock));
    Ok(lock)
}
pub fn list(provider: Provider) -> Result<Vec<AccountView>, String> {
    Ok(store::all(provider)?.iter().map(Into::into).collect())
}
pub fn configure(
    provider: Provider,
    id: &str,
    enabled: Option<bool>,
    auto_refresh: Option<bool>,
) -> Result<AccountView, String> {
    let a = store::update(provider, id, None, |a| {
        if let Some(value) = enabled {
            a.enabled = value;
        }
        if let Some(value) = auto_refresh {
            a.auto_refresh = value;
        }
    })?;
    scheduling::wake();
    Ok((&a).into())
}
pub async fn fresh(provider: Provider, id: &str, force: bool) -> Result<Account, Failure> {
    // A cancelled HTTP request must not cancel a rotating-token exchange before its new
    // refresh token reaches disk. The owned task finishes persistence under the account lock.
    let id = id.to_owned();
    tokio::spawn(async move { refresh_owned(provider, &id, force).await })
        .await
        .map_err(|_| Failure::new(503, "Account refresh task failed"))?
}
async fn refresh_owned(provider: Provider, id: &str, force: bool) -> Result<Account, Failure> {
    let lock = refresh_lock(id)?;
    let _guard = lock.lock().await;
    let mut a = store::get(provider, id)?;
    if !force && (a.expires_at == 0 || a.expires_at > now() + 300) {
        return Ok(a);
    }
    if a.credentials.refresh_token.is_empty() && a.credentials.personal_token.is_empty() {
        if a.expires_at > now() {
            return Ok(a);
        }
        return Err(Failure::new(
            401,
            "This account requires browser authorization again",
        ));
    }
    let result = match http::client_for(&a).await {
        Ok(client) => match provider {
            Provider::Qoder => qoder::refresh(&client, &mut a).await,
        },
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        let _ = store::update(provider, id, Some(&a.revision), |a| {
            a.auth_error = Some(error.to_string());
            a.next_sync_at = now() + 60;
        });
        return Err(error);
    }
    // Persist rotating refresh tokens BEFORE metadata requests, and never resurrect a deleted
    // account or overwrite a newer browser authorization.
    Ok(store::update(provider, id, Some(&a.revision), |stored| {
        stored.credentials = a.credentials.clone();
        stored.expires_at = a.expires_at;
        stored.auth_error = None;
    })?)
}
pub async fn sync(provider: Provider, id: &str, force: bool) -> Result<AccountView, String> {
    sync_metadata(provider, id, force, true).await
}
async fn fetch_metadata(
    client: &qoder_client::transport::Client,
    a: &Account,
    catalog: bool,
) -> (Option<Result<Vec<Model>, Failure>>, Result<Quota, Failure>) {
    match a.provider {
        Provider::Qoder => tokio::join!(
            async {
                if catalog {
                    Some(qoder::models(client, a).await)
                } else {
                    None
                }
            },
            qoder::quota(client, a)
        ),
    }
}
async fn sync_metadata(
    provider: Provider,
    id: &str,
    force: bool,
    catalog: bool,
) -> Result<AccountView, String> {
    let lock = refresh_lock(&format!("metadata:{id}"))?;
    let _guard = lock.lock().await;
    let mut a = fresh(provider, id, force)
        .await
        .map_err(|e| e.to_string())?;
    let client = http::client_for(&a).await.map_err(|e| e.to_string())?;
    let (mut models, mut quota) = fetch_metadata(&client, &a, catalog).await;
    if models
        .as_ref()
        .is_some_and(|r| r.as_ref().is_err_and(|e| e.status == 401))
        || quota.as_ref().err().is_some_and(|e| e.status == 401)
    {
        a = fresh(provider, id, true).await.map_err(|e| e.to_string())?;
        (models, quota) = fetch_metadata(&client, &a, catalog).await;
    }
    let updated = store::update(provider, id, Some(&a.revision), |stored| {
        match models {
            Some(Ok(models)) => {
                stored.models = models;
                stored.models_error = None;
                stored.models_updated_at = now();
            }
            Some(Err(error)) => stored.models_error = Some(error.to_string()),
            None => {}
        }
        match quota {
            Ok(quota) => {
                stored.quota = Some(quota);
                stored.quota_error = None;
            }
            Err(error) => stored.quota_error = Some(error.to_string()),
        }
        stored.last_sync_at = now();
        stored.next_sync_at = now() + sync_interval(id);
    })?;
    scheduling::wake();
    if updated.quota.as_ref().is_some_and(|q| !q.exhausted()) {
        if let Ok(mut r) = RUNTIME.lock() {
            if let Some(r) = r.get_mut(id) {
                if r.last_error.as_deref() == Some("Account quota exhausted") {
                    r.cooldown_until = 0;
                    r.last_error = None;
                }
            }
        }
    }
    Ok((&updated).into())
}

/// An in-flight permit lives through the complete response stream (including cancellation).
pub struct Lease {
    pub account: Account,
    /// The native model ID this account serves for the request.
    pub model: String,
    finished: bool,
    breaker: crate::models::CircuitBreakerConfig,
}
impl Lease {
    pub fn success(&mut self) {
        self.finish(None);
    }
    pub fn failure(&mut self, status: u16) {
        self.failure_after(status, None);
    }
    pub fn failure_after(&mut self, status: u16, retry_after: Option<u64>) {
        let message = if status == 402 {
            "Account quota exhausted"
        } else if status == 429 {
            "Provider rate limit"
        } else if status == 401 || status == 403 {
            "Provider authorization rejected"
        } else {
            "Provider request failed"
        };
        self.finish(Some((retry_after.unwrap_or(1).min(86400) as i64, message)));
    }
    /// Settles the lease by the provider's own classification, falling back to the
    /// status-derived breaker rules when the failure carries none.
    pub fn settle(&mut self, failure: &Failure) {
        use http::Penalty;
        match &failure.penalty {
            None => self.failure_after(failure.status, None),
            Some(Penalty::Breaker { retry_after }) => {
                self.failure_after(failure.status, *retry_after)
            }
            Some(Penalty::Until { at, message }) => {
                if self.finished {
                    return;
                }
                self.finish(Some((1, message)));
                if let Ok(mut r) = RUNTIME.lock() {
                    if let Some(r) = r.get_mut(&self.account.id) {
                        r.cooldown_until = r.cooldown_until.max(*at);
                    }
                }
            }
            Some(Penalty::Neutral) => self.finished = true,
            Some(Penalty::Disable { message }) => {
                let _ = store::update(self.account.provider, &self.account.id, None, |a| {
                    a.enabled = false;
                    a.auth_error = Some(message.clone());
                });
                self.failure_after(failure.status, None);
            }
        }
    }
    fn finish(&mut self, failure: Option<(i64, &str)>) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Ok(mut r) = RUNTIME.lock() {
            let r = r.entry(self.account.id.clone()).or_default();
            if let Some((minimum, message)) = failure {
                r.failures += 1;
                r.consecutive_failures += 1;
                r.last_error = Some(message.into());
                if self.breaker.enabled {
                    r.cooldown_until =
                        now().saturating_add(crate::proxy::sticky_config::account_backoff_seconds(
                            &self.breaker,
                            r.consecutive_failures,
                            minimum,
                        ));
                }
            } else {
                r.successes += 1;
                r.last_generation_at = now();
                r.consecutive_failures = 0;
                r.cooldown_until = 0;
                r.last_error = None;
            }
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut r) = RUNTIME.lock() {
            if let Some(r) = r.get_mut(&self.account.id) {
                r.in_flight = r.in_flight.saturating_sub(1);
            }
        }
        scheduling::wake();
    }
}

/// Stateless admission for explicit model probes. Public inference passes its live policy.
#[cfg(test)]
pub fn select(
    provider: Provider,
    model: &str,
    exclude: &HashSet<String>,
) -> Result<Lease, Failure> {
    select_for_probe(provider, model, exclude, None)
}

pub fn select_for_probe(
    provider: Provider,
    model: &str,
    exclude: &HashSet<String>,
    account_id: Option<&str>,
) -> Result<Lease, Failure> {
    use crate::proxy::sticky_config::{
        AccountSchedulingPolicy, SchedulingMode, StickySessionConfig,
    };
    let policy = AccountSchedulingPolicy {
        sticky: StickySessionConfig {
            mode: SchedulingMode::PerformanceFirst,
            max_wait_seconds: 0,
        },
        breaker: crate::modules::config::load_app_config()
            .map(|c| c.circuit_breaker)
            .unwrap_or_default(),
        preferred: account_id.map(str::to_owned),
    };
    scheduling::try_acquire(provider, &[model], "", &policy, exclude)?
        .ok_or_else(|| Failure::new(429, "Accounts are busy or cooling down"))
}
/// Background model and quota polls stop once the last real generation is this old,
/// so an idle pool does not heartbeat the upstream on a fixed timer.
const QUIET_AFTER_SECS: i64 = 6 * 3600;
/// Tokens are renewed in the background only this close to expiry (the same window
/// [`fresh`] uses), not on a daily timer.
const RENEW_WINDOW_SECS: i64 = 300;
/// Per-account offset so accounts never poll in lockstep; stable across restarts.
fn jitter(id: &str, range: u64) -> i64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("native-sync:{id}").as_bytes());
    let value = u64::from_le_bytes(digest[..8].try_into().unwrap_or([0; 8]));
    (value % range.max(1)) as i64
}
/// About 15–18 minutes between polls of an account in use.
pub(super) fn sync_interval(id: &str) -> i64 {
    900 + jitter(id, 180)
}
fn recently_active(id: &str, at: i64) -> bool {
    let last = runtime(id).last_generation_at;
    last > 0 && at.saturating_sub(last) < QUIET_AFTER_SECS
}
/// An idle account with a last-known zero balance still needs to discover resets
/// and top-ups. Do not use `Quota::exhausted`: its 15-minute admission TTL must not
/// disable recovery checks as the snapshot ages (including after a restart).
fn needs_quota_recovery(a: &Account) -> bool {
    a.quota
        .as_ref()
        .is_some_and(|q| !q.unlimited && q.remaining.is_some_and(|n| n <= 0.0))
}
/// Healthy idle accounts pause polls; zero-balance accounts keep quota recovery.
pub fn sync_paused(a: &Account, at: i64) -> bool {
    a.models_updated_at > 0 && !recently_active(&a.id, at) && !needs_quota_recovery(a)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Work {
    /// Renew if needed, then refresh the catalog and quota.
    Sync,
    /// Recover an idle account's balance without polling its catalog or running UMID.
    Quota,
    /// Only renew a token that is about to expire.
    Renew,
}
/// What background maintenance should do for an account now. A catalog that was never
/// fetched is always due (an account without models cannot be scheduled, so it would
/// never become active). Idle zero-balance accounts retain quota-only recovery at
/// the same bounded, jittered interval; other polls only follow recent use.
pub(super) fn work(a: &Account, at: i64) -> Option<Work> {
    if !a.enabled || !a.auto_refresh || a.next_sync_at > at {
        return None;
    }
    if a.models_updated_at == 0 || recently_active(&a.id, at) {
        return Some(Work::Sync);
    }
    if needs_quota_recovery(a) {
        return Some(Work::Quota);
    }
    let refreshable =
        !a.credentials.refresh_token.is_empty() || !a.credentials.personal_token.is_empty();
    (refreshable && a.expires_at > 0 && a.expires_at <= at + RENEW_WINDOW_SECS)
        .then_some(Work::Renew)
}
/// Oldest-due first, so accounts rescheduled after a failure (+60s) cannot keep
/// monopolizing the bounded batch and starve accounts later in the vault.
pub(super) fn due_for_maintenance(accounts: Vec<Account>, at: i64) -> Vec<Account> {
    let mut due: Vec<_> = accounts
        .into_iter()
        .filter(|a| work(a, at).is_some())
        .collect();
    due.sort_by_key(|a| a.next_sync_at);
    due.truncate(20);
    due
}
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Desktop setup may run outside a Tokio context; use the application's runtime just
    // like the existing WorkBuddy maintenance service.
    tauri::async_runtime::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            for provider in Provider::ALL {
                if !store::settings(provider).is_ok_and(|s| s.enabled) {
                    continue;
                }
                let Ok(accounts) = store::all(provider) else {
                    tracing::warn!("{} account vault is unavailable", provider.name());
                    continue;
                };
                let at = now();
                for a in due_for_maintenance(accounts, at) {
                    let limit = std::time::Duration::from_secs(50);
                    let ok = match work(&a, at) {
                        Some(Work::Sync) => matches!(
                            tokio::time::timeout(limit, sync(provider, &a.id, false)).await,
                            Ok(Ok(_))
                        ),
                        Some(Work::Quota) => matches!(
                            tokio::time::timeout(
                                limit,
                                sync_metadata(provider, &a.id, false, false)
                            )
                            .await,
                            Ok(Ok(_))
                        ),
                        Some(Work::Renew) => matches!(
                            tokio::time::timeout(limit, fresh(provider, &a.id, false)).await,
                            Ok(Ok(_))
                        ),
                        None => true,
                    };
                    if !ok {
                        tracing::warn!(
                            "{} account maintenance failed for {}",
                            provider.name(),
                            a.id
                        );
                        let _ = store::update(provider, &a.id, Some(&a.revision), |a| {
                            a.next_sync_at = now() + 60
                        });
                    }
                }
            }
        }
    });
}
