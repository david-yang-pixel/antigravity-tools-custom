//! Admission and affinity for the native pool, using the shared scheduling controls.
use super::*;
use crate::proxy::sticky_config::{SchedulingMode, StickySessionConfig};
use std::time::Duration;

#[derive(Clone, Default)]
pub struct Policy {
    pub sticky: StickySessionConfig,
    pub preferred: Option<String>,
    pub breaker: crate::models::CircuitBreakerConfig,
}

struct Binding {
    account_id: String,
    touched: i64,
}
static SESSIONS: LazyLock<Mutex<HashMap<String, Binding>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CHANGED: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

pub(super) fn wake() {
    CHANGED.notify_waiters();
}

pub fn clear_sessions() {
    if let Ok(mut sessions) = SESSIONS.lock() {
        sessions.clear();
    }
    wake();
}

pub fn clear_cooldowns(account_id: Option<&str>) -> bool {
    let Ok(prefix) = runtime_key("") else {
        return false;
    };
    let Ok(mut pool) = RUNTIME.lock() else {
        return false;
    };
    let mut cleared = false;
    for (key, entry) in pool.iter_mut() {
        if key.starts_with(&prefix) && account_id.is_none_or(|id| key == &format!("{prefix}{id}")) {
            cleared |= entry.cooldown_until > 0;
            entry.cooldown_until = 0;
            entry.cooldown_reason = None;
            entry.health.consecutive_failures = 0;
        }
    }
    wake();
    cleared
}

/// None means eligible accounts exist but are busy or cooling down.
pub(super) fn try_acquire(
    model: &str,
    session: &str,
    policy: &Policy,
) -> Result<Option<Lease>, String> {
    try_acquire_inner(model, session, policy, false, &[])
}

/// Failover for a request whose earlier accounts failed before generation. It never
/// waits, skips `excluded` (so a session bound to a failed account cannot pin it, even
/// with the breaker off) and rebinds the session to the account it picks.
pub fn try_acquire_excluding(
    model: &str,
    session: &str,
    policy: &Policy,
    excluded: &[String],
) -> Result<Option<Lease>, String> {
    try_acquire_inner(model, session, policy, false, excluded)
}

fn try_acquire_inner(
    model: &str,
    session: &str,
    policy: &Policy,
    probe: bool,
    excluded: &[String],
) -> Result<Option<Lease>, String> {
    let accounts: Vec<_> = all()?
        .into_iter()
        .filter(|a| {
            a.enabled
                && if probe {
                    a.models.iter().any(|m| m.id == model)
                } else {
                    a.supports_model(model)
                }
        })
        .filter(|a| policy.preferred.as_ref().is_none_or(|id| &a.id == id))
        .filter(|a| !excluded.contains(&a.id))
        .collect();
    if accounts.is_empty() {
        return Err("No eligible WorkBuddy account for this model; check the fixed account, enablement and fetch verified models".into());
    }
    let mut pool = RUNTIME.lock().map_err(|_| "WorkBuddy pool unavailable")?;
    let mut sessions = SESSIONS
        .lock()
        .map_err(|_| "WorkBuddy sessions unavailable")?;
    let now = now();
    sessions.retain(|_, binding| now.saturating_sub(binding.touched) < 3600);
    let session_key = format!("{}:{model}:{session}", path()?.display());
    let sticky = policy.sticky.mode != SchedulingMode::PerformanceFirst && !session.is_empty();
    if !sticky {
        sessions.remove(&session_key);
    }
    let bound = if sticky {
        sessions.get(&session_key).map(|s| s.account_id.clone())
    } else {
        None
    };
    let bound = bound.filter(|id| accounts.iter().any(|a| &a.id == id));
    let mut candidates = Vec::new();
    for account in accounts {
        let key = runtime_key(&account.id)?;
        let entry = pool.entry(key.clone()).or_default();
        let available =
            entry.in_flight < 2 && (!policy.breaker.enabled || entry.cooldown_until <= now);
        if bound.as_ref() == Some(&account.id)
            && !available
            && policy.sticky.mode == SchedulingMode::CacheFirst
        {
            return Ok(None);
        }
        if available {
            candidates.push((
                bound.as_ref() != Some(&account.id),
                entry.in_flight,
                entry.last_used,
                account,
                key,
            ));
        }
    }
    candidates.sort_by_key(|(unbound, load, last, _, _)| (*unbound, *load, *last));
    let Some((_, _, _, account, key)) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let next = pool
        .values()
        .map(|e| e.last_used)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let entry = pool.get_mut(&key).expect("selected account exists");
    entry.in_flight += 1;
    entry.last_used = next;
    if sticky {
        if sessions.len() >= 10000 && !sessions.contains_key(&session_key) {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, b)| b.touched)
                .map(|(k, _)| k.clone())
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            session_key,
            Binding {
                account_id: account.id.clone(),
                touched: now,
            },
        );
    }
    Ok(Some(Lease {
        account,
        key,
        finished: std::sync::atomic::AtomicBool::new(false),
        breaker: policy.breaker.clone(),
    }))
}

/// Explicit verification shares the real pool's capacity and cooldown state, but
/// can retry models previously rejected for this account after permissions change.
pub async fn acquire_probe(account: &Account, model: &str) -> Result<Lease, String> {
    let policy = Policy {
        preferred: Some(account.id.clone()),
        breaker: crate::modules::config::load_app_config()?.circuit_breaker,
        ..Policy::default()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(lease) = try_acquire_inner(model, "", &policy, true, &[])? {
            if lease.account.revision != account.revision {
                return Err("WorkBuddy credentials changed while waiting to verify models".into());
            }
            return Ok(lease);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("WorkBuddy account is busy or cooling down; fetch models later".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn acquire(
    model: &str,
    session: &str,
    policy: &Policy,
    timeout: u64,
) -> Result<Lease, String> {
    let wait = if policy.sticky.mode == SchedulingMode::CacheFirst {
        policy.sticky.max_wait_seconds.min(300).min(timeout)
    } else {
        0
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        // Register before selecting, so a release cannot be missed between selection and wait.
        let notified = CHANGED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(lease) = try_acquire(model, session, policy)? {
            return Ok(lease);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(
                "WorkBuddy accounts are busy or cooling down; scheduling wait limit reached".into(),
            );
        }
        // Recheck disk enablement, elapsed cooldowns and model availability at least once a second.
        let _ = tokio::time::timeout(remaining.min(Duration::from_secs(1)), notified).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed(uid: &str) -> String {
        let mut account =
            parse_import(json!({"uid": uid, "accessToken": "test", "expiresAt": now()+3600}))
                .unwrap();
        account.models.push(Model {
            id: "model".into(),
            name: "Model".into(),
        });
        upsert(account).unwrap().id
    }

    #[tokio::test]
    async fn workbuddy_balance_affinity_failover_and_performance_rotation() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        seed("a");
        seed("b");
        let mut policy = Policy::default();
        let first = acquire("model", "user-a:conversation", &policy, 5)
            .await
            .unwrap();
        let id = first.account.id.clone();
        drop(first);
        let same = acquire("model", "user-a:conversation", &policy, 5)
            .await
            .unwrap();
        assert_eq!(same.account.id, id);
        let other_user = acquire("model", "user-b:conversation", &policy, 5)
            .await
            .unwrap();
        assert_ne!(other_user.account.id, id);
        drop(other_user);
        same.failure("limited", 60);
        drop(same);
        let fallback = acquire("model", "user-a:conversation", &policy, 5)
            .await
            .unwrap();
        assert_ne!(fallback.account.id, id);
        drop(fallback);
        clear_cooldowns(None);
        policy.sticky.mode = SchedulingMode::PerformanceFirst;
        let a = acquire("model", "same", &policy, 5)
            .await
            .unwrap()
            .account
            .id
            .clone();
        let b = acquire("model", "same", &policy, 5)
            .await
            .unwrap()
            .account
            .id
            .clone();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn workbuddy_cache_wait_release_timeout_and_zero_wait() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        seed("a");
        seed("b");
        let mut policy = Policy::default();
        policy.sticky.mode = SchedulingMode::CacheFirst;
        policy.sticky.max_wait_seconds = 1;
        let first = acquire("model", "session", &policy, 5).await.unwrap();
        let id = first.account.id.clone();
        let second = acquire("model", "session", &policy, 5).await.unwrap();
        assert_eq!(id, second.account.id);
        let release = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(first);
        };
        let (result, _) = tokio::join!(acquire("model", "session", &policy, 5), release);
        assert_eq!(result.as_ref().unwrap().account.id, id);
        drop(result);
        drop(second);
        let limited = acquire("model", "session", &policy, 5).await.unwrap();
        limited.failure("limited", 60);
        drop(limited);
        let started = tokio::time::Instant::now();
        assert!(acquire("model", "session", &policy, 5).await.is_err());
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert!(started.elapsed() < Duration::from_secs(3));
        policy.sticky.max_wait_seconds = 0;
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            acquire("model", "session", &policy, 5)
        )
        .await
        .unwrap()
        .is_err());
        clear_sessions();
        assert_ne!(
            acquire("model", "session", &policy, 5)
                .await
                .unwrap()
                .account
                .id,
            id
        );
    }

    #[tokio::test]
    async fn workbuddy_fixed_account_breaker_backoff_and_shared_reset() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let id = seed("a");
        seed("b");
        let mut policy = Policy {
            preferred: Some(id.clone()),
            ..Default::default()
        };
        policy.breaker.backoff_steps = vec![2, 10];
        let lease = acquire("model", "session", &policy, 5).await.unwrap();
        assert_eq!(lease.account.id, id);
        lease.failure("first", 1);
        drop(lease);
        assert!(acquire("model", "session", &policy, 5).await.is_err());
        // Expire the first cooldown without resetting the failure count.
        RUNTIME
            .lock()
            .unwrap()
            .get_mut(&runtime_key(&id).unwrap())
            .unwrap()
            .cooldown_until = 0;
        let lease = acquire("model", "session", &policy, 5).await.unwrap();
        lease.failure("second", 1);
        drop(lease);
        assert!(
            list()
                .unwrap()
                .into_iter()
                .find(|a| a.id == id)
                .unwrap()
                .cooldown_until
                >= now() + 9
        );
        let manager =
            crate::proxy::TokenManager::new(crate::modules::account::get_data_dir().unwrap());
        manager.clear_all_rate_limits();
        assert!(acquire("model", "session", &policy, 5).await.is_ok());
        policy.breaker.enabled = false;
        let lease = acquire("model", "session", &policy, 5).await.unwrap();
        lease.failure("no breaker", 60);
        drop(lease);
        assert!(acquire("model", "session", &policy, 5).await.is_ok());
        set_enabled(&id, false).unwrap();
        assert!(acquire("model", "session", &policy, 5).await.is_err());
        remove(&id).unwrap();
        assert!(acquire("model", "session", &policy, 5).await.is_err());
    }

    #[test]
    fn workbuddy_session_identity_is_scoped_by_downstream_user() {
        let payload = json!({"messages":[{"role":"user","content":"hello"}]});
        let continuation = json!({"messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"Hi"},{"role":"user","content":"continue"}]});
        let key = crate::modules::workbuddy::client::session_key;
        assert_eq!(key("user-a", &payload), key("user-a", &continuation));
        assert_ne!(key("user-a", &payload), key("user-b", &payload));
    }
}
