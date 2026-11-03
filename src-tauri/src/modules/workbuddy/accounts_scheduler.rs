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
            cleared |= entry.cooldown_until > 0 || !entry.models.is_empty();
            entry.cooldown_until = 0;
            entry.cooldown_reason = None;
            entry.health.consecutive_failures = 0;
            entry.models.clear();
        }
    }
    drop(pool);
    if account_id.is_none() {
        cleared |= clear_model_circuits();
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

fn eligible(
    model: &str,
    policy: &Policy,
    probe: bool,
    excluded: &[String],
) -> Result<Vec<Account>, String> {
    Ok(all()?
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
        .collect())
}

fn try_acquire_inner(
    model: &str,
    session: &str,
    policy: &Policy,
    probe: bool,
    excluded: &[String],
) -> Result<Option<Lease>, String> {
    let accounts = eligible(model, policy, probe, excluded)?;
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
            entry.in_flight < 2 && (!policy.breaker.enabled || entry.schedulable(model, now));
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
        model: model.to_owned(),
        key,
        finished: std::sync::atomic::AtomicBool::new(false),
        breaker: policy.breaker.clone(),
    }))
}

/// Model-wide circuit for upstream rate limits. When several accounts get the same
/// HTTP 429 for one model in a short window, the limit is about the model (quota or
/// upstream throttling), not the accounts: further requests are held locally with a
/// Retry-After instead of each one spending another account. After the hold a single
/// probe is let through; any accepted response closes the circuit, another 429
/// reopens it at the next backoff step.
#[derive(Default)]
struct ModelCircuit {
    /// Recent 429s as (account, upstream signature, unix seconds); one per account.
    strikes: Vec<(String, String, i64)>,
    open_until: i64,
    /// Consecutive openings without an accepted response in between.
    trips: u64,
    reason: String,
    accounts: usize,
    probing: bool,
}
static CIRCUITS: LazyLock<Mutex<HashMap<String, ModelCircuit>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// How long a 429 counts toward opening the model circuit.
const STRIKE_WINDOW_SECONDS: i64 = 120;
/// Distinct accounts that must agree before the limit is treated as model-wide.
const STRIKE_ACCOUNTS: usize = 2;
/// Retry-After while another request is probing a half-open circuit.
const PROBE_RETRY_SECONDS: i64 = 5;

fn circuit_key(model: &str) -> Result<String, String> {
    Ok(format!("{}:{model}", path()?.display()))
}

fn clear_model_circuits() -> bool {
    let Ok(prefix) = circuit_key("") else {
        return false;
    };
    let Ok(mut circuits) = CIRCUITS.lock() else {
        return false;
    };
    let before = circuits.len();
    circuits.retain(|key, _| !key.starts_with(&prefix));
    circuits.len() != before
}

/// Why a model is being held locally.
#[derive(Debug)]
pub struct ModelLimited {
    pub retry_after: i64,
    pub accounts: usize,
    pub reason: String,
}

/// Held by the one request probing a half-open circuit; releases the probe slot on drop
/// whatever the outcome, so a probe that ends without a verdict cannot wedge the model.
#[derive(Debug)]
pub struct ModelProbe {
    key: String,
}
impl Drop for ModelProbe {
    fn drop(&mut self) {
        if let Ok(mut circuits) = CIRCUITS.lock() {
            if let Some(circuit) = circuits.get_mut(&self.key) {
                circuit.probing = false;
            }
        }
    }
}

/// Admission for a model before any account is leased. `Ok(Some(_))` marks the probe.
pub fn admit_model(model: &str, policy: &Policy) -> Result<Option<ModelProbe>, ModelLimited> {
    if !policy.breaker.enabled {
        return Ok(None);
    }
    let Ok(key) = circuit_key(model) else {
        return Ok(None);
    };
    let Ok(mut circuits) = CIRCUITS.lock() else {
        return Ok(None);
    };
    let Some(circuit) = circuits.get_mut(&key) else {
        return Ok(None);
    };
    let now = now();
    let limited = |circuit: &ModelCircuit, retry_after: i64| ModelLimited {
        retry_after: retry_after.max(1),
        accounts: circuit.accounts,
        reason: circuit.reason.clone(),
    };
    if circuit.open_until > now {
        return Err(limited(circuit, circuit.open_until - now));
    }
    if circuit.trips == 0 {
        return Ok(None);
    }
    if circuit.probing {
        return Err(limited(circuit, PROBE_RETRY_SECONDS));
    }
    circuit.probing = true;
    Ok(Some(ModelProbe { key }))
}

/// Records an upstream HTTP 429 for `model` on `account_id`. `signature` identifies the
/// upstream reason (its result code) so only matching rejections add up. Returns the
/// hold in seconds when this rejection opened the circuit.
pub fn record_model_limit(
    model: &str,
    account_id: &str,
    signature: &str,
    seconds: i64,
    policy: &Policy,
) -> Option<i64> {
    if !policy.breaker.enabled {
        return None;
    }
    // A pool of one has nobody left to try, so its first rejection is already model-wide.
    let threshold = eligible(model, policy, false, &[])
        .map(|accounts| accounts.len())
        .unwrap_or(0)
        .clamp(1, STRIKE_ACCOUNTS);
    let key = circuit_key(model).ok()?;
    let mut circuits = CIRCUITS.lock().ok()?;
    let circuit = circuits.entry(key).or_default();
    let now = now();
    // Requests admitted before the circuit opened may still land here; they confirm the
    // hold, not a new opening.
    if circuit.open_until > now {
        return None;
    }
    circuit
        .strikes
        .retain(|(id, _, at)| id != account_id && now.saturating_sub(*at) < STRIKE_WINDOW_SECONDS);
    circuit
        .strikes
        .push((account_id.to_owned(), signature.to_owned(), now));
    let agreeing = circuit
        .strikes
        .iter()
        .filter(|(_, s, _)| s == signature)
        .count();
    // While half-open (an earlier opening, hold elapsed) one more rejection reopens it.
    let half_open = circuit.trips > 0 && circuit.open_until <= now;
    if !half_open && agreeing < threshold {
        return None;
    }
    circuit.trips += 1;
    let hold = super::backoff_seconds(&policy.breaker, circuit.trips, seconds);
    circuit.open_until = now.saturating_add(hold);
    circuit.accounts = agreeing.max(1);
    circuit.reason = signature.to_owned();
    circuit.strikes.clear();
    circuit.probing = false;
    tracing::warn!(
        "WorkBuddy model {} is rate-limited upstream ({}) across {} account(s); holding requests for {}s",
        model,
        signature,
        circuit.accounts,
        hold
    );
    Some(hold)
}

#[cfg(test)]
pub(crate) fn expire_model_circuits() {
    if let Ok(mut circuits) = CIRCUITS.lock() {
        for circuit in circuits.values_mut() {
            circuit.open_until = 0;
        }
    }
}

/// Any response the upstream accepted proves the model is being served: close its circuit.
pub fn model_accepted(model: &str) {
    let Ok(key) = circuit_key(model) else {
        return;
    };
    if let Ok(mut circuits) = CIRCUITS.lock() {
        circuits.remove(&key);
    }
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

    #[tokio::test]
    async fn workbuddy_rate_limit_cools_only_that_model_on_the_account() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let mut account =
            parse_import(json!({"uid": "a", "accessToken": "test", "expiresAt": now()+3600}))
                .unwrap();
        for id in ["model", "other"] {
            account.models.push(Model {
                id: id.into(),
                name: id.into(),
            });
        }
        let id = upsert(account).unwrap().id;
        let policy = Policy::default();
        let lease = acquire("model", "", &policy, 5).await.unwrap();
        assert_eq!(lease.model_failure("limited", 60), 60);
        drop(lease);
        // The limited model has no account left, the other model keeps its account.
        assert!(acquire("model", "", &policy, 5).await.is_err());
        let other = acquire("other", "", &policy, 5).await.unwrap();
        assert_eq!(other.account.id, id);
        drop(other);
        let view = list().unwrap().remove(0);
        assert_eq!(view.status, "ready");
        assert_eq!(view.cooldown_until, 0);
        assert_eq!(view.health.consecutive_failures, 0);
        assert_eq!(view.health.failures, 1);
        assert_eq!(view.model_cooldowns.len(), 1);
        assert_eq!(view.model_cooldowns[0].model, "model");
        assert!(view.model_cooldowns[0].until >= now() + 59);
        // A second consecutive limit backs off to the next configured step.
        RUNTIME
            .lock()
            .unwrap()
            .get_mut(&runtime_key(&id).unwrap())
            .unwrap()
            .models
            .get_mut("model")
            .unwrap()
            .until = 0;
        let lease = acquire("model", "", &policy, 5).await.unwrap();
        assert_eq!(lease.model_failure("limited", 60), 300);
        drop(lease);
        assert!(clear_cooldowns(None));
        let lease = acquire("model", "", &policy, 5).await.unwrap();
        lease.success();
        drop(lease);
        assert!(list().unwrap()[0].model_cooldowns.is_empty());
        // With the breaker off nothing is cooled.
        let mut off = Policy::default();
        off.breaker.enabled = false;
        let lease = acquire("model", "", &off, 5).await.unwrap();
        assert_eq!(lease.model_failure("limited", 60), 0);
        drop(lease);
        assert!(acquire("model", "", &policy, 5).await.is_ok());
    }

    #[tokio::test]
    async fn workbuddy_model_circuit_opens_on_agreeing_accounts_and_probes_once() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed("a");
        let b = seed("b");
        seed("c");
        let policy = Policy::default();
        assert!(admit_model("model", &policy).unwrap().is_none());
        assert_eq!(record_model_limit("model", &a, "code 1", 60, &policy), None);
        // A different upstream reason, or the same account again, does not add up.
        assert_eq!(record_model_limit("model", &b, "code 2", 60, &policy), None);
        assert_eq!(record_model_limit("model", &a, "code 1", 60, &policy), None);
        assert!(admit_model("model", &policy).unwrap().is_none());
        assert_eq!(
            record_model_limit("model", &b, "code 1", 60, &policy),
            Some(60)
        );
        let limited = admit_model("model", &policy).unwrap_err();
        assert!((59..=60).contains(&limited.retry_after));
        assert_eq!(limited.accounts, 2);
        assert_eq!(limited.reason, "code 1");
        // Other models are untouched; late rejections confirm the hold without escalating.
        assert!(admit_model("other", &policy).unwrap().is_none());
        assert_eq!(record_model_limit("model", &a, "code 1", 60, &policy), None);
        // Half-open: exactly one probe at a time.
        expire_model_circuits();
        let probe = admit_model("model", &policy).unwrap().unwrap();
        assert_eq!(
            admit_model("model", &policy).unwrap_err().retry_after,
            PROBE_RETRY_SECONDS
        );
        drop(probe);
        let _probe = admit_model("model", &policy).unwrap().unwrap();
        // The probe's rejection reopens at the next backoff step.
        assert_eq!(
            record_model_limit("model", &a, "code 1", 60, &policy),
            Some(300)
        );
        assert!(admit_model("model", &policy).is_err());
        // Any accepted response closes it.
        model_accepted("model");
        assert!(admit_model("model", &policy).unwrap().is_none());
        // The shared reset clears circuits too.
        record_model_limit("model", &a, "code 1", 60, &policy);
        record_model_limit("model", &b, "code 1", 60, &policy);
        assert!(admit_model("model", &policy).is_err());
        assert!(clear_cooldowns(None));
        assert!(admit_model("model", &policy).unwrap().is_none());
        // With the breaker off the circuit never engages.
        let mut off = Policy::default();
        off.breaker.enabled = false;
        record_model_limit("model", &a, "code 1", 60, &policy);
        record_model_limit("model", &b, "code 1", 60, &policy);
        assert!(admit_model("model", &off).unwrap().is_none());
        assert_eq!(record_model_limit("model", &a, "code 1", 60, &off), None);
    }

    #[tokio::test]
    async fn workbuddy_single_account_rate_limit_opens_the_model_circuit() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed("a");
        seed("b");
        let policy = Policy::default();
        let fixed = Policy {
            preferred: Some(a.clone()),
            ..Default::default()
        };
        // A fixed account has nobody else to try: one rejection is already model-wide.
        assert_eq!(
            record_model_limit("model", &a, "code 1", 60, &fixed),
            Some(60)
        );
        assert!(admit_model("model", &policy).is_err());
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
