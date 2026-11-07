//! Native account admission uses the same live policy as the other pools.
use super::*;
use crate::proxy::sticky_config::{AccountSchedulingPolicy as Policy, SchedulingMode};
use std::time::Duration;

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

pub fn clear_provider_sessions(provider: Provider) {
    let prefix = format!(
        "{}:{}:",
        crate::modules::account::get_data_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        provider.key()
    );
    if let Ok(mut sessions) = SESSIONS.lock() {
        sessions.retain(|key, _| !key.starts_with(&prefix));
    }
    wake();
}

pub fn clear_cooldowns(account_id: Option<&str>) -> bool {
    let Ok(mut runtime) = RUNTIME.lock() else {
        return false;
    };
    let mut cleared = false;
    for (id, entry) in runtime.iter_mut() {
        if account_id.is_none_or(|wanted| wanted == id) {
            cleared |= entry.cooldown_until > 0;
            entry.cooldown_until = 0;
            entry.consecutive_failures = 0;
            entry.last_error = None;
        }
    }
    drop(runtime);
    wake();
    cleared
}

/// None means otherwise eligible accounts are occupied or cooling down.
pub fn try_acquire(
    provider: Provider,
    targets: &[&str],
    session: &str,
    policy: &Policy,
    exclude: &HashSet<String>,
) -> Result<Option<Lease>, Failure> {
    let settings = store::settings(provider)?;
    if !settings.enabled {
        return Err(Failure::new(503, "This model source is disabled"));
    }
    let fixed = policy
        .preferred
        .as_deref()
        .or(settings.preferred_account_id.as_deref());
    // The first target an account supports is the native ID it serves.
    let picks: Vec<_> = store::all(provider)?
        .into_iter()
        .filter_map(|a| {
            let pick = targets.iter().find(|t| {
                a.models.iter().any(|m| {
                    m.id == **t && (m.is_free || !a.quota.as_ref().is_some_and(Quota::exhausted))
                })
            })?;
            Some((a, pick.to_string()))
        })
        .filter(|(a, _)| {
            a.enabled
                && !exclude.contains(&a.id)
                && fixed.is_none_or(|id| id == a.id)
                && !a.credentials.access_token.is_empty()
                && (a.expires_at == 0
                    || a.expires_at > now()
                    || !a.credentials.refresh_token.is_empty()
                    || !a.credentials.personal_token.is_empty())
        })
        .collect();
    if picks.is_empty() {
        return Err(Failure::new(503, "No eligible account for this model; check the fixed account, enablement, quota and authorization"));
    }
    let limit = std::env::var(provider.env("MAX_CONCURRENCY"))
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(5)
        .clamp(1, MAX_ACCOUNT_CONCURRENCY);
    let key = format!(
        "{}:{}:{}:{session}",
        crate::modules::account::get_data_dir()?.display(),
        provider.key(),
        crate::proxy::sticky_config::target_affinity_key(targets)
    );
    let sticky = policy.sticky.mode != SchedulingMode::PerformanceFirst && !session.is_empty();
    let mut runtime = RUNTIME
        .lock()
        .map_err(|_| "Native account scheduler unavailable")?;
    let mut sessions = SESSIONS
        .lock()
        .map_err(|_| "Native account sessions unavailable")?;
    let time = now();
    sessions.retain(|_, binding| time.saturating_sub(binding.touched) < 3600);
    if !sticky {
        sessions.remove(&key);
    }
    let bound = sessions
        .get(&key)
        .map(|b| b.account_id.clone())
        .filter(|id| picks.iter().any(|(a, _)| &a.id == id));
    let mut candidates = Vec::new();
    for (account, pick) in picks {
        let entry = runtime.entry(account.id.clone()).or_default();
        let available =
            entry.in_flight < limit && (!policy.breaker.enabled || entry.cooldown_until <= time);
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
                entry.sequence,
                account,
                pick,
            ));
        }
    }
    candidates.sort_by_key(|(unbound, load, used, ..)| (*unbound, *load, *used));
    let Some((_, _, _, account, model)) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let sequence = runtime
        .values()
        .map(|r| r.sequence)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let entry = runtime
        .get_mut(&account.id)
        .expect("selected account exists");
    entry.in_flight += 1;
    entry.sequence = sequence;
    if sticky {
        if sessions.len() >= 10000 && !sessions.contains_key(&key) {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, b)| b.touched)
                .map(|(k, _)| k.clone())
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            key,
            Binding {
                account_id: account.id.clone(),
                touched: time,
            },
        );
    }
    Ok(Some(Lease {
        account,
        model,
        finished: false,
        breaker: policy.breaker.clone(),
    }))
}

/// All waiting is bounded by both the global Cache First limit and the request deadline.
pub async fn acquire(
    provider: Provider,
    targets: &[&str],
    session: &str,
    policy: &Policy,
    exclude: &HashSet<String>,
    request_deadline: tokio::time::Instant,
) -> Result<Lease, Failure> {
    let wait = if policy.sticky.mode == SchedulingMode::CacheFirst {
        policy.sticky.max_wait_seconds.min(300)
    } else {
        0
    };
    let deadline = request_deadline.min(tokio::time::Instant::now() + Duration::from_secs(wait));
    loop {
        if tokio::time::Instant::now() >= request_deadline {
            return Err(Failure::new(
                504,
                "Account scheduling exceeded the request timeout",
            ));
        }
        let changed = CHANGED.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if let Some(lease) = try_acquire(provider, targets, session, policy, exclude)? {
            return Ok(lease);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Failure::new(
                429,
                "Accounts are busy or cooling down; global scheduling wait limit reached",
            ));
        }
        let _ = tokio::time::timeout(remaining.min(Duration::from_secs(1)), changed).await;
    }
}
