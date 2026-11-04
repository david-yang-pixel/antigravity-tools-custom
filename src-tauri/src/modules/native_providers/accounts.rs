use super::{http::Failure, *};
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
    let result = match provider {
        Provider::Qoder => qoder::refresh(&mut a).await,
        Provider::Trae => trae::refresh(&mut a).await,
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
    let lock = refresh_lock(&format!("metadata:{id}"))?;
    let _guard = lock.lock().await;
    let mut a = fresh(provider, id, force)
        .await
        .map_err(|e| e.to_string())?;
    let (mut models, mut quota) = match provider {
        Provider::Qoder => tokio::join!(qoder::models(&a), qoder::quota(&a)),
        Provider::Trae => tokio::join!(trae::models(&a), trae::quota(&a)),
    };
    if models.as_ref().err().is_some_and(|e| e.status == 401)
        || quota.as_ref().err().is_some_and(|e| e.status == 401)
    {
        a = fresh(provider, id, true).await.map_err(|e| e.to_string())?;
        (models, quota) = match provider {
            Provider::Qoder => tokio::join!(qoder::models(&a), qoder::quota(&a)),
            Provider::Trae => tokio::join!(trae::models(&a), trae::quota(&a)),
        };
    }
    let updated = store::update(provider, id, Some(&a.revision), |stored| {
        match models {
            Ok(models) => {
                stored.models = models;
                stored.models_error = None;
                stored.models_updated_at = now();
            }
            Err(error) => stored.models_error = Some(error.to_string()),
        }
        match quota {
            Ok(quota) => {
                stored.quota = Some(quota);
                stored.quota_error = None;
            }
            Err(error) => stored.quota_error = Some(error.to_string()),
        }
        stored.last_sync_at = now();
        stored.next_sync_at = now() + 300;
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
    scheduling::try_acquire(provider, model, "", &policy, exclude)?
        .ok_or_else(|| Failure::new(429, "Accounts are busy or cooling down"))
}
/// Oldest-due first, so accounts rescheduled after a failure (+60s) cannot keep
/// monopolizing the bounded batch and starve accounts later in the vault.
pub(super) fn due_for_maintenance(accounts: Vec<Account>, at: i64) -> Vec<Account> {
    let mut due: Vec<_> = accounts
        .into_iter()
        .filter(|a| a.enabled && a.auto_refresh && a.next_sync_at <= at)
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
                for a in due_for_maintenance(accounts, now()) {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(50),
                        sync(provider, &a.id, false),
                    )
                    .await;
                    if !matches!(result, Ok(Ok(_))) {
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
