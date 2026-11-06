//! Admission and affinity for the native pool, using the shared scheduling controls.
use super::*;
use crate::proxy::sticky_config::SchedulingMode;
use std::time::Duration;

pub use crate::proxy::sticky_config::AccountSchedulingPolicy as Policy;

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
///
/// `targets` are the native IDs one publication routes to, in binding order. Accounts
/// supporting any of them form one pool; the lease carries the first target the chosen
/// account supports and can schedule.
pub(super) fn try_acquire(
    targets: &[&str],
    session: &str,
    policy: &Policy,
) -> Result<Option<Lease>, String> {
    try_acquire_inner(targets, session, policy, false, &[])
}

/// Failover for a request whose earlier accounts failed before generation. It never
/// waits, skips `excluded` (so a session bound to a failed account cannot pin it, even
/// with the breaker off) and rebinds the session to the account it picks.
pub fn try_acquire_excluding(
    targets: &[&str],
    session: &str,
    policy: &Policy,
    excluded: &[String],
) -> Result<Option<Lease>, String> {
    try_acquire_inner(targets, session, policy, false, excluded)
}

/// Real requests skip models verification found unavailable; explicit verification
/// (`probe`) may retry any model in the account's catalog.
fn supports(account: &Account, model: &str, probe: bool) -> bool {
    if probe {
        account.models.iter().any(|m| m.id == model)
    } else {
        account.supports_model(model)
    }
}

fn eligible(
    targets: &[&str],
    policy: &Policy,
    probe: bool,
    excluded: &[String],
) -> Result<Vec<Account>, String> {
    Ok(all()?
        .into_iter()
        .filter(|a| a.enabled && targets.iter().any(|t| supports(a, t, probe)))
        .filter(|a| policy.preferred.as_ref().is_none_or(|id| &a.id == id))
        .filter(|a| !excluded.contains(&a.id))
        .collect())
}

fn try_acquire_inner(
    targets: &[&str],
    session: &str,
    policy: &Policy,
    probe: bool,
    excluded: &[String],
) -> Result<Option<Lease>, String> {
    let accounts = eligible(targets, policy, probe, excluded)?;
    if accounts.is_empty() {
        return Err("No eligible WorkBuddy account for this model; check the fixed account, enablement and fetch verified models".into());
    }
    let mut pool = RUNTIME.lock().map_err(|_| "WorkBuddy pool unavailable")?;
    let mut sessions = SESSIONS
        .lock()
        .map_err(|_| "WorkBuddy sessions unavailable")?;
    let now = now();
    sessions.retain(|_, binding| now.saturating_sub(binding.touched) < 3600);
    let session_key = format!(
        "{}:{}:{session}",
        path()?.display(),
        crate::proxy::sticky_config::target_affinity_key(targets)
    );
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
        // The first target, in binding order, this account supports and can schedule.
        // Ranking below ignores which target matched: the group is one equal pool.
        let pick = targets
            .iter()
            .copied()
            .find(|t| {
                supports(&account, t, probe)
                    && (!policy.breaker.enabled || entry.schedulable(t, now))
            })
            .filter(|_| entry.in_flight < 2);
        if bound.as_ref() == Some(&account.id)
            && pick.is_none()
            && policy.sticky.mode == SchedulingMode::CacheFirst
        {
            return Ok(None);
        }
        if let Some(pick) = pick {
            candidates.push((
                bound.as_ref() != Some(&account.id),
                entry.in_flight,
                entry.last_used,
                account,
                key,
                pick,
            ));
        }
    }
    candidates.sort_by_key(|(unbound, load, last, _, _, _)| (*unbound, *load, *last));
    let Some((_, _, _, account, key, model)) = candidates.into_iter().next() else {
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

/// What a request may dispatch to after its targets pass their model circuits.
#[derive(Debug)]
pub struct Admission {
    /// Targets a lease may be taken for, in binding order. Includes the probe target.
    pub targets: Vec<String>,
    /// The one half-open target this request probes, with the guard holding its probe
    /// slot. Pass it to `settle_probe` after every lease; the guard otherwise lives until
    /// the request ends.
    pub probe: Option<(String, ModelProbe)>,
    /// Why each dropped target is held: its circuit is open, or its probe slot is taken.
    pub held: Vec<ModelLimited>,
}

impl Admission {
    /// The probe policy (no waiting while holding the probe slot) applies only when the
    /// probe target is all this request may lease; beside healthy targets the normal
    /// policy applies.
    pub fn lease_policy(&self, policy: &Policy) -> Policy {
        match &self.probe {
            Some((target, _)) if self.targets == [target.as_str()] => policy.for_probe(),
            _ => policy.clone(),
        }
    }
}

/// Admission for a publication's targets before any account is leased. Healthy targets
/// are admitted. The first half-open target whose probe slot is free is admitted beside
/// them as this request's probe, so a recovering native ID rejoins the pool even while a
/// sibling is healthy; other half-open targets wait as held. Held targets are dropped and
/// reported in `held`. When nothing is admitted, the shortest hold is the error.
pub fn admit_models(targets: &[&str], policy: &Policy) -> Result<Admission, ModelLimited> {
    let mut admitted = Vec::new();
    let mut probe = None;
    let mut held = Vec::new();
    for target in targets {
        match admit_model(target, policy, probe.is_none()) {
            Ok(None) => admitted.push((*target).to_owned()),
            Ok(Some(guard)) => {
                admitted.push((*target).to_owned());
                probe = Some(((*target).to_owned(), guard));
            }
            Err(limited) => held.push(limited),
        }
    }
    if admitted.is_empty() && !held.is_empty() {
        let shortest = held
            .into_iter()
            .min_by_key(|limited| limited.retry_after)
            .expect("held is not empty");
        return Err(shortest);
    }
    Ok(Admission {
        targets: admitted,
        probe,
        held,
    })
}

/// Call after each lease of a request (the first and every failover). The probe guard
/// stays only while the lease serves the probe target. Otherwise it is released at once,
/// freeing the slot for the next request, and the target leaves this request's
/// candidates so a failover cannot reach the half-open circuit without its slot.
pub fn settle_probe(
    probe: &mut Option<(String, ModelProbe)>,
    targets: &mut Vec<&str>,
    leased: &str,
) {
    if probe.as_ref().is_some_and(|(target, _)| target != leased) {
        if let Some((target, guard)) = probe.take() {
            drop(guard);
            targets.retain(|t| *t != target);
        }
    }
}

/// Admission for a model before any account is leased. `Ok(Some(_))` marks the probe.
/// A free half-open probe slot is claimed only with `claim`; otherwise that target is held
/// briefly, as if another request were probing it.
fn admit_model(
    model: &str,
    policy: &Policy,
    claim: bool,
) -> Result<Option<ModelProbe>, ModelLimited> {
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
    if circuit.probing || !claim {
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
    let threshold = eligible(&[model], policy, false, &[])
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

/// When no account can take `model` because every eligible one is cooling, and at least
/// one of them is cooling for this model, the model is rate-limited in effect. Hold it on
/// its circuit until the first account frees up, so callers get a 429 with Retry-After
/// rather than a 503, and a half-open circuit does not keep admitting a probe that has
/// no account to go to. Accounts that are only busy are not a rate limit: `None`.
fn hold_cooling_model(model: &str, policy: &Policy) -> Option<ModelLimited> {
    if !policy.breaker.enabled {
        return None;
    }
    let accounts = eligible(&[model], policy, false, &[]).ok()?;
    if accounts.is_empty() {
        return None;
    }
    let now = now();
    let mut free_at = i64::MAX;
    let mut cooling = 0usize;
    let mut cause = None;
    {
        let pool = RUNTIME.lock().ok()?;
        for account in &accounts {
            let entry = pool.get(&runtime_key(&account.id).ok()?);
            let account_until = entry.map_or(0, |e| e.cooldown_until);
            let limit = entry.and_then(|e| e.models.get(model));
            let model_until = limit.map_or(0, |m| m.until);
            let until = account_until.max(model_until);
            if until <= now {
                return None;
            }
            if model_until > now {
                cooling += 1;
                cause.get_or_insert_with(|| limit.map(|m| m.reason.clone()).unwrap_or_default());
            }
            free_at = free_at.min(until);
        }
    }
    if cooling == 0 {
        return None;
    }
    let key = circuit_key(model).ok()?;
    let mut circuits = CIRCUITS.lock().ok()?;
    let circuit = circuits.entry(key).or_default();
    circuit.open_until = circuit.open_until.max(free_at);
    circuit.probing = false;
    circuit.accounts = circuit.accounts.max(cooling);
    if circuit.reason.is_empty() {
        circuit.reason = cause.unwrap_or_default();
    }
    Some(ModelLimited {
        retry_after: circuit.open_until.saturating_sub(now).max(1),
        accounts: circuit.accounts,
        reason: circuit.reason.clone(),
    })
}

/// `hold_cooling_model` for a publication's targets: the request is rate-limited only
/// when every target is. Each target is evaluated, so every cooling target is held on
/// its own circuit; the shortest hold is reported.
pub fn hold_cooling_models(targets: &[&str], policy: &Policy) -> Option<ModelLimited> {
    let held: Vec<_> = targets
        .iter()
        .map(|target| hold_cooling_model(target, policy))
        .collect();
    held.into_iter()
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .min_by_key(|limited| limited.retry_after)
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
        if let Some(lease) = try_acquire_inner(&[model], "", &policy, true, &[])? {
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
    targets: &[&str],
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
        if let Some(lease) = try_acquire(targets, session, policy)? {
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
        seed_with(uid, &["model"])
    }

    fn seed_with(uid: &str, models: &[&str]) -> String {
        let mut account =
            parse_import(json!({"uid": uid, "accessToken": "test", "expiresAt": now()+3600}))
                .unwrap();
        for id in models {
            account.models.push(Model {
                id: (*id).into(),
                name: (*id).into(),
            });
        }
        upsert(account).unwrap().id
    }

    fn cool(id: &str, model: &str, until: i64) {
        RUNTIME
            .lock()
            .unwrap()
            .entry(runtime_key(id).unwrap())
            .or_default()
            .models
            .insert(
                model.into(),
                super::super::ModelCooldown {
                    until,
                    reason: "limited".into(),
                    consecutive: 1,
                },
            );
    }

    #[tokio::test]
    async fn workbuddy_group_pools_accounts_with_different_native_ids() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let cn = seed_with("cn", &["flash"]);
        let global = seed_with("global", &["flash-sg"]);
        let mut policy = Policy::default();
        policy.sticky.mode = SchedulingMode::PerformanceFirst;
        let group = ["flash", "flash-sg"];
        let first = acquire(&group, "session", &policy, 5).await.unwrap();
        let second = acquire(&group, "session", &policy, 5).await.unwrap();
        let mut picked = vec![
            (first.account.id.clone(), first.model().to_owned()),
            (second.account.id.clone(), second.model().to_owned()),
        ];
        picked.sort();
        let mut expected = vec![(cn, "flash".to_owned()), (global, "flash-sg".to_owned())];
        expected.sort();
        assert_eq!(picked, expected);
    }

    #[tokio::test]
    async fn workbuddy_group_prefers_first_target_on_accounts_listing_both() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        seed_with("a", &["flash", "flash-sg"]);
        let policy = Policy::default();
        for _ in 0..3 {
            let lease = acquire(&["flash", "flash-sg"], "", &policy, 5)
                .await
                .unwrap();
            assert_eq!(lease.model(), "flash");
        }
        // Binding order decides, not the account's catalog order.
        let lease = acquire(&["flash-sg", "flash"], "", &policy, 5)
            .await
            .unwrap();
        assert_eq!(lease.model(), "flash-sg");
    }

    #[tokio::test]
    async fn workbuddy_group_rate_limit_on_one_target_keeps_the_other() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let id = seed_with("a", &["flash", "flash-sg"]);
        let policy = Policy::default();
        let lease = acquire(&["flash", "flash-sg"], "", &policy, 5)
            .await
            .unwrap();
        assert_eq!(lease.model(), "flash");
        lease.model_failure("limited", 60);
        drop(lease);
        let lease = acquire(&["flash", "flash-sg"], "", &policy, 5)
            .await
            .unwrap();
        assert_eq!(lease.account.id, id);
        assert_eq!(lease.model(), "flash-sg");
        drop(lease);
        assert!(try_acquire(&["flash"], "", &policy).unwrap().is_none());
    }

    #[tokio::test]
    async fn workbuddy_single_target_session_key_is_unchanged() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        seed("a");
        seed("b");
        let policy = Policy::default();
        let first = acquire(&["model"], "session", &policy, 5).await.unwrap();
        let id = first.account.id.clone();
        drop(first);
        let key = format!("{}:model:session", path().unwrap().display());
        assert_eq!(
            SESSIONS
                .lock()
                .unwrap()
                .get(&key)
                .map(|b| b.account_id.clone()),
            Some(id.clone())
        );
        let same = acquire(&["model"], "session", &policy, 5).await.unwrap();
        assert_eq!(same.account.id, id);
    }

    #[tokio::test]
    async fn workbuddy_group_admission_skips_held_targets_and_reports_min_retry() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed_with("a", &["flash", "flash-sg"]);
        let b = seed_with("b", &["flash", "flash-sg"]);
        let policy = Policy::default();
        let group = ["flash", "flash-sg"];
        let admission = admit_models(&group, &policy).unwrap();
        assert_eq!(admission.targets, group);
        assert!(admission.probe.is_none());
        record_model_limit("flash", &a, "code 1", 120, &policy);
        assert_eq!(
            record_model_limit("flash", &b, "code 1", 120, &policy),
            Some(120)
        );
        let admission = admit_models(&group, &policy).unwrap();
        assert_eq!(admission.targets, ["flash-sg"]);
        assert!(admission.probe.is_none());
        record_model_limit("flash-sg", &a, "code 2", 60, &policy);
        assert_eq!(
            record_model_limit("flash-sg", &b, "code 2", 60, &policy),
            Some(60)
        );
        let limited = admit_models(&group, &policy).unwrap_err();
        assert!((59..=60).contains(&limited.retry_after));
        assert_eq!(limited.reason, "code 2");
        assert_eq!(limited.accounts, 2);
    }

    #[tokio::test]
    async fn workbuddy_group_claims_one_half_open_probe_per_request() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed_with("a", &["flash", "flash-sg"]);
        let b = seed_with("b", &["flash", "flash-sg"]);
        let policy = Policy::default();
        let group = ["flash", "flash-sg"];
        for model in group {
            record_model_limit(model, &a, "code 1", 60, &policy);
            record_model_limit(model, &b, "code 1", 60, &policy);
        }
        expire_model_circuits();
        // Both half-open: the first target is the probe; the other is held, not claimed.
        let admission = admit_models(&group, &policy).unwrap();
        assert_eq!(admission.targets, ["flash"]);
        assert_eq!(probe_target(&admission), Some("flash"));
        assert_eq!(admission.held.len(), 1);
        assert_eq!(admission.held[0].retry_after, PROBE_RETRY_SECONDS);
        // A lone probe target leases with the probe policy.
        let mut cache_first = policy.clone();
        cache_first.sticky.mode = SchedulingMode::CacheFirst;
        cache_first.sticky.max_wait_seconds = 30;
        assert_eq!(
            admission.lease_policy(&cache_first).sticky.max_wait_seconds,
            0
        );
        let solo = admit_models(&["flash-sg"], &policy).unwrap();
        assert!(solo.probe.is_some());
        drop(solo);
        drop(admission);
        // With a healthy sibling the half-open target still joins, beside it.
        model_accepted("flash-sg");
        let admission = admit_models(&group, &policy).unwrap();
        assert_eq!(admission.targets, group);
        assert_eq!(probe_target(&admission), Some("flash"));
        assert!(admission.held.is_empty());
        // A mixed admission leases with the normal policy.
        assert_eq!(
            admission.lease_policy(&cache_first).sticky.max_wait_seconds,
            30
        );
    }

    fn probe_target(admission: &Admission) -> Option<&str> {
        admission.probe.as_ref().map(|(target, _)| target.as_str())
    }

    #[tokio::test]
    async fn workbuddy_group_half_open_target_rejoins_the_pool_while_a_sibling_is_healthy() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed_with("a", &["flash"]);
        let b = seed_with("b", &["flash"]);
        seed_with("c", &["flash-sg"]);
        let policy = Policy::default();
        let group = ["flash", "flash-sg"];
        record_model_limit("flash", &a, "code 1", 60, &policy);
        assert_eq!(
            record_model_limit("flash", &b, "code 1", 60, &policy),
            Some(60)
        );
        expire_model_circuits();
        let first = admit_models(&group, &policy).unwrap();
        assert_eq!(first.targets, group);
        assert_eq!(probe_target(&first), Some("flash"));
        assert!(first.held.is_empty());
        // The probe slot is taken: a concurrent request keeps the healthy target only.
        let second = admit_models(&group, &policy).unwrap();
        assert_eq!(second.targets, ["flash-sg"]);
        assert!(second.probe.is_none());
        assert_eq!(second.held.len(), 1);
        assert_eq!(second.held[0].retry_after, PROBE_RETRY_SECONDS);
        drop(second);
        // A lease on the probe target keeps the guard; one elsewhere releases it at once
        // and takes the unguarded target out of the request's failover candidates.
        let Admission { mut probe, .. } = first;
        let mut targets = group.to_vec();
        settle_probe(&mut probe, &mut targets, "flash");
        assert!(probe.is_some());
        assert_eq!(targets, group);
        assert!(admit_model("flash", &policy, true).is_err());
        settle_probe(&mut probe, &mut targets, "flash-sg");
        assert!(probe.is_none());
        assert_eq!(targets, ["flash-sg"]);
        let third = admit_models(&group, &policy).unwrap();
        assert_eq!(probe_target(&third), Some("flash"));
        drop(third);
        // A probe that is served closes the circuit: the pool is whole again.
        model_accepted("flash");
        let healed = admit_models(&group, &policy).unwrap();
        assert_eq!(healed.targets, group);
        assert!(healed.probe.is_none());
    }

    #[tokio::test]
    async fn workbuddy_group_held_and_half_open_targets_admit_the_probe() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed_with("a", &["flash", "flash-sg"]);
        let b = seed_with("b", &["flash", "flash-sg"]);
        let policy = Policy::default();
        let group = ["flash", "flash-sg"];
        for model in group {
            record_model_limit(model, &a, "code 1", 60, &policy);
            record_model_limit(model, &b, "code 1", 60, &policy);
        }
        expire_model_circuits();
        // A failed probe reopens flash-sg; flash stays half-open.
        assert!(record_model_limit("flash-sg", &a, "code 1", 60, &policy).is_some());
        let admission = admit_models(&group, &policy).unwrap();
        assert_eq!(admission.targets, ["flash"]);
        assert_eq!(probe_target(&admission), Some("flash"));
        assert_eq!(admission.held.len(), 1);
        assert!(admission.held[0].retry_after > PROBE_RETRY_SECONDS);
    }

    #[tokio::test]
    async fn workbuddy_group_admission_reports_targets_it_held() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed_with("a", &["flash"]);
        let b = seed_with("b", &["flash"]);
        seed_with("c", &["flash"]);
        let d = seed_with("d", &["flash-sg"]);
        let policy = Policy::default();
        let group = ["flash", "flash-sg"];
        record_model_limit("flash", &a, "code 1", 120, &policy);
        record_model_limit("flash", &b, "code 1", 120, &policy);
        cool(&d, "flash-sg", now() + 60);
        let admission = admit_models(&group, &policy).unwrap();
        assert_eq!(admission.targets, ["flash-sg"]);
        assert_eq!(admission.held.len(), 1);
        assert!((119..=120).contains(&admission.held[0].retry_after));
        assert_eq!(admission.held[0].reason, "code 1");
        // The open circuit's account c is free, so only the admitted targets tell the
        // truth about the lease failure.
        assert!(hold_cooling_models(&group, &policy).is_none());
        let admitted: Vec<&str> = admission.targets.iter().map(String::as_str).collect();
        let limited = hold_cooling_models(&admitted, &policy).unwrap();
        assert!((59..=60).contains(&limited.retry_after));
    }

    #[tokio::test]
    async fn workbuddy_group_hold_requires_every_target_cooling() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed_with("a", &["flash"]);
        let b = seed_with("b", &["flash-sg"]);
        let policy = Policy::default();
        cool(&a, "flash", now() + 300);
        // Every target is evaluated, so the cooling one is held on its circuit even
        // though another target still has a free account.
        assert!(hold_cooling_models(&["flash-sg", "flash"], &policy).is_none());
        assert!(admit_model("flash", &policy, true).is_err());
        assert!(hold_cooling_models(&["flash", "flash-sg"], &policy).is_none());
        cool(&b, "flash-sg", now() + 90);
        let limited = hold_cooling_models(&["flash", "flash-sg"], &policy).unwrap();
        assert!((89..=90).contains(&limited.retry_after));
    }

    #[tokio::test]
    async fn workbuddy_balance_affinity_failover_and_performance_rotation() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        seed("a");
        seed("b");
        let mut policy = Policy::default();
        let first = acquire(&["model"], "user-a:conversation", &policy, 5)
            .await
            .unwrap();
        let id = first.account.id.clone();
        drop(first);
        let same = acquire(&["model"], "user-a:conversation", &policy, 5)
            .await
            .unwrap();
        assert_eq!(same.account.id, id);
        let other_user = acquire(&["model"], "user-b:conversation", &policy, 5)
            .await
            .unwrap();
        assert_ne!(other_user.account.id, id);
        drop(other_user);
        same.failure("limited", 60);
        drop(same);
        let fallback = acquire(&["model"], "user-a:conversation", &policy, 5)
            .await
            .unwrap();
        assert_ne!(fallback.account.id, id);
        drop(fallback);
        clear_cooldowns(None);
        policy.sticky.mode = SchedulingMode::PerformanceFirst;
        let a = acquire(&["model"], "same", &policy, 5)
            .await
            .unwrap()
            .account
            .id
            .clone();
        let b = acquire(&["model"], "same", &policy, 5)
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
        let first = acquire(&["model"], "session", &policy, 5).await.unwrap();
        let id = first.account.id.clone();
        let second = acquire(&["model"], "session", &policy, 5).await.unwrap();
        assert_eq!(id, second.account.id);
        let release = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(first);
        };
        let (result, _) = tokio::join!(acquire(&["model"], "session", &policy, 5), release);
        assert_eq!(result.as_ref().unwrap().account.id, id);
        drop(result);
        drop(second);
        let limited = acquire(&["model"], "session", &policy, 5).await.unwrap();
        limited.failure("limited", 60);
        drop(limited);
        let started = tokio::time::Instant::now();
        assert!(acquire(&["model"], "session", &policy, 5).await.is_err());
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert!(started.elapsed() < Duration::from_secs(3));
        policy.sticky.max_wait_seconds = 0;
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            acquire(&["model"], "session", &policy, 5)
        )
        .await
        .unwrap()
        .is_err());
        clear_sessions();
        assert_ne!(
            acquire(&["model"], "session", &policy, 5)
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
        let lease = acquire(&["model"], "session", &policy, 5).await.unwrap();
        assert_eq!(lease.account.id, id);
        lease.failure("first", 1);
        drop(lease);
        assert!(acquire(&["model"], "session", &policy, 5).await.is_err());
        // Expire the first cooldown without resetting the failure count.
        RUNTIME
            .lock()
            .unwrap()
            .get_mut(&runtime_key(&id).unwrap())
            .unwrap()
            .cooldown_until = 0;
        let lease = acquire(&["model"], "session", &policy, 5).await.unwrap();
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
        assert!(acquire(&["model"], "session", &policy, 5).await.is_ok());
        policy.breaker.enabled = false;
        let lease = acquire(&["model"], "session", &policy, 5).await.unwrap();
        lease.failure("no breaker", 60);
        drop(lease);
        assert!(acquire(&["model"], "session", &policy, 5).await.is_ok());
        set_enabled(&id, false).unwrap();
        assert!(acquire(&["model"], "session", &policy, 5).await.is_err());
        remove(&id).unwrap();
        assert!(acquire(&["model"], "session", &policy, 5).await.is_err());
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
        let lease = acquire(&["model"], "", &policy, 5).await.unwrap();
        assert_eq!(lease.model_failure("limited", 60), 60);
        drop(lease);
        // The limited model has no account left, the other model keeps its account.
        assert!(acquire(&["model"], "", &policy, 5).await.is_err());
        let other = acquire(&["other"], "", &policy, 5).await.unwrap();
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
        let lease = acquire(&["model"], "", &policy, 5).await.unwrap();
        assert_eq!(lease.model_failure("limited", 60), 300);
        drop(lease);
        assert!(clear_cooldowns(None));
        let lease = acquire(&["model"], "", &policy, 5).await.unwrap();
        lease.success();
        drop(lease);
        assert!(list().unwrap()[0].model_cooldowns.is_empty());
        // With the breaker off nothing is cooled.
        let mut off = Policy::default();
        off.breaker.enabled = false;
        let lease = acquire(&["model"], "", &off, 5).await.unwrap();
        assert_eq!(lease.model_failure("limited", 60), 0);
        drop(lease);
        assert!(acquire(&["model"], "", &policy, 5).await.is_ok());
    }

    #[tokio::test]
    async fn workbuddy_model_circuit_opens_on_agreeing_accounts_and_probes_once() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed("a");
        let b = seed("b");
        seed("c");
        let policy = Policy::default();
        assert!(admit_model("model", &policy, true).unwrap().is_none());
        assert_eq!(record_model_limit("model", &a, "code 1", 60, &policy), None);
        // A different upstream reason, or the same account again, does not add up.
        assert_eq!(record_model_limit("model", &b, "code 2", 60, &policy), None);
        assert_eq!(record_model_limit("model", &a, "code 1", 60, &policy), None);
        assert!(admit_model("model", &policy, true).unwrap().is_none());
        assert_eq!(
            record_model_limit("model", &b, "code 1", 60, &policy),
            Some(60)
        );
        let limited = admit_model("model", &policy, true).unwrap_err();
        assert!((59..=60).contains(&limited.retry_after));
        assert_eq!(limited.accounts, 2);
        assert_eq!(limited.reason, "code 1");
        // Other models are untouched; late rejections confirm the hold without escalating.
        assert!(admit_model("other", &policy, true).unwrap().is_none());
        assert_eq!(record_model_limit("model", &a, "code 1", 60, &policy), None);
        // Half-open: exactly one probe at a time.
        expire_model_circuits();
        let probe = admit_model("model", &policy, true).unwrap().unwrap();
        assert_eq!(
            admit_model("model", &policy, true).unwrap_err().retry_after,
            PROBE_RETRY_SECONDS
        );
        drop(probe);
        let _probe = admit_model("model", &policy, true).unwrap().unwrap();
        // The probe's rejection reopens at the next backoff step.
        assert_eq!(
            record_model_limit("model", &a, "code 1", 60, &policy),
            Some(300)
        );
        assert!(admit_model("model", &policy, true).is_err());
        // Any accepted response closes it.
        model_accepted("model");
        assert!(admit_model("model", &policy, true).unwrap().is_none());
        // The shared reset clears circuits too.
        record_model_limit("model", &a, "code 1", 60, &policy);
        record_model_limit("model", &b, "code 1", 60, &policy);
        assert!(admit_model("model", &policy, true).is_err());
        assert!(clear_cooldowns(None));
        assert!(admit_model("model", &policy, true).unwrap().is_none());
        // With the breaker off the circuit never engages.
        let mut off = Policy::default();
        off.breaker.enabled = false;
        record_model_limit("model", &a, "code 1", 60, &policy);
        record_model_limit("model", &b, "code 1", 60, &policy);
        assert!(admit_model("model", &off, true).unwrap().is_none());
        assert_eq!(record_model_limit("model", &a, "code 1", 60, &off), None);
    }

    #[tokio::test]
    async fn workbuddy_every_account_cooling_for_a_model_holds_it_until_the_first_frees() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = seed("a");
        let b = seed("b");
        let policy = Policy::default();
        let set = |id: &str, until: i64| {
            RUNTIME
                .lock()
                .unwrap()
                .entry(runtime_key(id).unwrap())
                .or_default()
                .models
                .insert(
                    "model".into(),
                    super::super::ModelCooldown {
                        until,
                        reason: "limited".into(),
                        consecutive: 1,
                    },
                );
        };
        // One account still free: not held.
        set(&a, now() + 300);
        assert!(hold_cooling_model("model", &policy).is_none());
        // Both cooling: held until the earlier one frees, whatever the circuit said.
        set(&b, now() + 90);
        let limited = hold_cooling_model("model", &policy).unwrap();
        assert!((89..=90).contains(&limited.retry_after));
        assert_eq!(limited.accounts, 2);
        assert_eq!(limited.reason, "limited");
        assert!((89..=90).contains(&admit_model("model", &policy, true).unwrap_err().retry_after));
        // Busy is not a rate limit, nor is an account-wide cooldown alone.
        clear_cooldowns(None);
        let first = acquire(&["model"], "", &policy, 5).await.unwrap();
        let second = acquire(&["model"], "", &policy, 5).await.unwrap();
        let third = acquire(&["model"], "", &policy, 5).await.unwrap();
        let fourth = acquire(&["model"], "", &policy, 5).await.unwrap();
        assert!(acquire(&["model"], "", &policy, 5).await.is_err());
        assert!(hold_cooling_model("model", &policy).is_none());
        first.failure("down", 60);
        second.failure("down", 60);
        third.failure("down", 60);
        fourth.failure("down", 60);
        drop((first, second, third, fourth));
        assert!(acquire(&["model"], "", &policy, 5).await.is_err());
        assert!(hold_cooling_model("model", &policy).is_none());
        // With the breaker off nothing is held.
        clear_cooldowns(None);
        set(&a, now() + 60);
        set(&b, now() + 60);
        let mut off = Policy::default();
        off.breaker.enabled = false;
        assert!(hold_cooling_model("model", &off).is_none());
    }

    #[test]
    fn workbuddy_probe_policy_never_waits_for_its_session_account() {
        let mut policy = Policy::default();
        policy.sticky.mode = SchedulingMode::CacheFirst;
        policy.sticky.max_wait_seconds = 120;
        let probe = policy.for_probe();
        assert!(probe.sticky.mode == SchedulingMode::Balance);
        assert_eq!(probe.sticky.max_wait_seconds, 0);
        policy.sticky.mode = SchedulingMode::PerformanceFirst;
        assert!(policy.for_probe().sticky.mode == SchedulingMode::PerformanceFirst);
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
        assert!(admit_model("model", &policy, true).is_err());
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
