//! One server-side worker for native accounts, also active without an open browser.
use super::{
    accounts::{self, Account, AccountView, Realm},
    resources::Credits,
};
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Management {
    pub managed: bool,
    pub auto_checkin: bool,
    /// Background check-in runs only after an administrator saves the switch.
    /// Older files that inherited the previous default do not count as that choice.
    pub auto_checkin_chosen: bool,
    pub keepalive: bool,
    pub auto_rewards: bool,
    pub auto_lottery: bool,
    /// Send the Buddy on its daily trip; arrival rewards come back through auto_rewards.
    pub auto_travel: bool,
    pub activity: super::activity::State,
    pub credits: Option<Credits>,
    pub credits_error: Option<String>,
    pub last_sync_at: i64,
    pub next_sync_at: i64,
    pub last_keepalive_at: i64,
    pub keepalive_attempt_at: i64,
    pub keepalive_error: Option<String>,
    pub checkin_date: Option<String>,
    pub checkin_at: i64,
    pub checkin_attempt_at: i64,
    pub checkin_status: String,
    pub checkin_error: Option<String>,
    pub checkin_guard: super::checkin::State,
}
impl Default for Management {
    fn default() -> Self {
        Self {
            managed: true,
            auto_checkin: false,
            auto_checkin_chosen: false,
            keepalive: true,
            auto_rewards: false,
            auto_lottery: false,
            auto_travel: false,
            activity: super::activity::State::default(),
            credits: None,
            credits_error: None,
            last_sync_at: 0,
            next_sync_at: 0,
            last_keepalive_at: 0,
            keepalive_attempt_at: 0,
            keepalive_error: None,
            checkin_date: None,
            checkin_at: 0,
            checkin_attempt_at: 0,
            checkin_status: "pending".into(),
            checkin_error: None,
            checkin_guard: super::checkin::State::default(),
        }
    }
}
pub fn day(at: i64) -> String {
    beijing(at).format("%Y-%m-%d").to_string()
}
fn beijing(at: i64) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
}
fn beijing_minute(at: i64) -> u32 {
    let local = beijing(at);
    local.hour() * 60 + local.minute()
}
fn sync_delay(account_id: &str) -> i64 {
    900 + super::identity::sync_extra_secs(account_id)
}
/// Catalog and credit polls stop once a real generation is this old.
const QUIET_AFTER_SECS: i64 = 6 * 3600;
fn recently_active(account: &Account, now: i64) -> bool {
    let last = accounts::last_generation_at(&account.id);
    last > 0 && now.saturating_sub(last) < QUIET_AFTER_SECS
}
fn catalog_due(account: &Account, now: i64) -> bool {
    now >= account.management.next_sync_at && recently_active(account, now)
}
fn checkin_due(a: &Account, now: i64) -> bool {
    if a.realm != Realm::Cn
        || !a.management.auto_checkin
        || !a.management.auto_checkin_chosen
        || a.management.checkin_date.as_deref() == Some(day(now).as_str())
        || !recently_active(a, now)
        || a.management
            .checkin_guard
            .admission(&day(now), now)
            .is_err()
    {
        return false;
    }
    let retry = if a.management.checkin_attempt_at > 0 {
        super::identity::checkin_retry_secs(&a.id, a.management.checkin_attempt_at)
    } else {
        0
    };
    if now - a.management.checkin_attempt_at < retry {
        return false;
    }
    // Prefer a stable daytime habit slot; fall back late evening so the day is not missed.
    let mins = beijing_minute(now);
    mins >= super::identity::checkin_minute(&a.id) || mins >= 22 * 60 + 30
}
fn keepalive_due(a: &Account, now: i64) -> bool {
    // Renew only inside the proactive window. A daily refresh is not required
    // to keep a still-valid token usable.
    a.management.keepalive
        && now - a.management.keepalive_attempt_at >= 300
        && a.expires_at <= now + accounts::PROACTIVE_REFRESH_SECS
}
fn due(a: &Account, now: i64) -> bool {
    if a.management.keepalive_error.is_some() && now - a.management.keepalive_attempt_at < 300 {
        return false;
    }
    a.enabled
        && a.management.managed
        && (catalog_due(a, now) || keepalive_due(a, now) || checkin_due(a, now))
}
/// Manual sync / check-in and automatic maintenance share a per-account lock.
/// Refresh tokens have a separate single-flight lock; observations never overwrite credentials.
pub async fn run(
    id: &str,
    force_refresh: bool,
    checkin_now: bool,
    automatic: bool,
) -> Result<AccountView, String> {
    let lock = accounts::refresh_lock(&format!("maintenance:{id}"))?;
    let _guard = if checkin_now {
        lock.try_lock()
            .map_err(|_| "WorkBuddy account maintenance is already running")?
    } else {
        lock.lock().await
    };
    let initial = accounts::get(id)?;
    let now = accounts::now();
    if checkin_now {
        if initial.realm != Realm::Cn || !initial.enabled {
            return Err("Check-in requires an enabled domestic WorkBuddy account".into());
        }
        // An idempotent repeat must not refresh credentials or poll other endpoints.
        if initial.management.checkin_date.as_deref() == Some(day(now).as_str()) {
            return accounts::view(initial);
        }
        initial.management.checkin_guard.admission(&day(now), now)?;
    }
    if automatic && !due(&initial, now) {
        return accounts::view(initial);
    }
    // Manual sync still refreshes the catalog. Automatic polls only follow a
    // real generation, so an idle pool does not heartbeat the upstream. A run
    // that only renews the token or checks in leaves the sync schedule alone,
    // so the first poll after the account becomes active is not pushed back.
    let polls_catalog = if automatic {
        catalog_due(&initial, now)
    } else {
        !checkin_now
    };
    let wants_checkin = checkin_now || (automatic && checkin_due(&initial, now));
    if wants_checkin {
        accounts::edit_management(id, |m| {
            m.checkin_guard
                .reserve(day(now), now, super::identity::checkin_retry_secs(id, now));
            m.checkin_attempt_at = now;
            // Remains visible if credential preparation, cancellation or a crash
            // prevents recording a final response. Never retry immediately.
            m.checkin_status = "failed".into();
            m.checkin_error =
                Some("Check-in attempt has no confirmed result; waiting for cooldown".into());
        })?;
    }
    if polls_catalog {
        // Persist the attempt before network I/O so a restart cannot cause a tight retry loop.
        // Per-account jitter keeps the pool from syncing in lockstep.
        accounts::edit_management(id, |m| m.next_sync_at = now + sync_delay(id))?;
    }
    let refresh = force_refresh || (automatic && keepalive_due(&initial, now));
    let mut account = match accounts::fresh(id, refresh).await {
        Ok(account) => account,
        Err(_) => return accounts::view(accounts::get(id)?),
    };
    // Recheck disable/management toggles before starting optional side effects.
    let current = accounts::get(id)?;
    if !current.enabled || (automatic && !current.management.managed) {
        return accounts::view(current);
    }
    account.management = current.management;
    let should_checkin = wants_checkin
        && (checkin_now
            || (account.management.auto_checkin && account.management.auto_checkin_chosen));
    if should_checkin && account.realm == Realm::Cn {
        match super::resources::checkin(&account).await {
            Ok(status) => {
                account.management.checkin_status = status.into();
                account.management.checkin_date = Some(day(now));
                account.management.checkin_at = accounts::now();
                account.management.checkin_error = None;
                account.management.checkin_guard.pause = None;
            }
            Err(error) => {
                account.management.checkin_status = if error.pause.is_some() {
                    "paused"
                } else {
                    "failed"
                }
                .into();
                account.management.checkin_guard.fail(&error);
                account.management.checkin_error = Some(error.message);
                // Stop this run here. A rejected check-in must not be followed
                // by additional model/credit probes as part of the same action.
                accounts::save_observation(&mut account)?;
                return accounts::view(account);
            }
        }
    }
    if account.realm == Realm::Global {
        account.management.checkin_status = "unsupported".into();
    }
    if polls_catalog {
        let (models, credits) = tokio::join!(
            super::client::models(&account),
            super::resources::credits(&account)
        );
        match models {
            Ok(models) => {
                account.models = models;
                account.last_error = None;
            }
            Err(e) => account.last_error = Some(e),
        }
        match credits {
            Ok(credits) => {
                account.management.credits = Some(credits);
                account.management.credits_error = None;
            }
            Err(e) => account.management.credits_error = Some(e),
        }
        account.management.last_sync_at = accounts::now();
        account.management.next_sync_at = accounts::now() + sync_delay(id);
    }
    accounts::save_observation(&mut account)?;
    super::register_models(&account.available_models())?;
    accounts::view(account)
}

/// Recovery does not send a request or reset today's attempts / server cooldown.
pub async fn resume_checkin(id: &str) -> Result<AccountView, String> {
    let lock = accounts::refresh_lock(&format!("maintenance:{id}"))?;
    let _guard = lock
        .try_lock()
        .map_err(|_| "WorkBuddy account maintenance is already running")?;
    if accounts::get(id)?.management.checkin_guard.pause
        == Some(super::checkin::Pause::Authorization)
    {
        return Err("Reauthorize this WorkBuddy account before resuming check-in".into());
    }
    accounts::edit_management(id, |m| {
        m.checkin_guard.pause = None;
        m.checkin_error = None;
        if m.checkin_date.as_deref() != Some(day(accounts::now()).as_str()) {
            m.checkin_status = "pending".into();
        }
    })
}
pub fn start() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    tauri::async_runtime::spawn(async {
        if let Err(error) = accounts::ensure_identities() {
            tracing::warn!("WorkBuddy identity backfill: {error}");
        }
        loop {
            match accounts::all() {
                Ok(accounts) => {
                    for account in accounts {
                        if due(&account, accounts::now()) {
                            if let Err(error) = run(&account.id, false, false, true).await {
                                tracing::warn!("WorkBuddy maintenance: {error}");
                            }
                        }
                        if super::activity::due(&account, accounts::now()) {
                            // Activity jobs have their own bounded worker slots and share
                            // the maintenance lock; a slow campaign cannot stall keepalive.
                            let _ = super::activity::start(
                                &account.id,
                                super::activity::Action::Automatic,
                            );
                        }
                    }
                }
                Err(error) => tracing::warn!("WorkBuddy maintenance store: {error}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn workbuddy_maintenance_schedule_and_legacy_defaults() {
        let mut a = accounts::parse_import(
            json!({"uid":"test","accessToken":"secret","expiresAt":2000000000}),
        )
        .unwrap();
        assert!(!a.management.auto_checkin);
        assert!(!a.management.auto_checkin_chosen);
        assert!(!checkin_due(&a, 1792425600 + 23 * 3600));
        a.management.auto_checkin = true;
        a.management.auto_checkin_chosen = true;
        // Beijing 2026-10-20 00:00 — an unused account does not poll. A recent
        // generation makes the catalog due; check-in still waits for the habit slot.
        let midnight = 1792425600;
        assert!(!due(&a, midnight));
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        accounts::note_generation_at(&a.id, midnight);
        assert!(due(&a, midnight));
        accounts::note_generation_at(&a.id, midnight - QUIET_AFTER_SECS - 1);
        assert!(!due(&a, midnight));
        accounts::note_generation_at(&a.id, midnight);
        let evening = midnight + 23 * 3600; // past the 22:30 same-day fallback
        assert!(due(&a, midnight));
        assert!(!checkin_due(&a, midnight));
        // Even an enabled check-in switch must not wake an idle account.
        assert!(!checkin_due(&a, evening));
        accounts::note_generation_at(&a.id, evening);
        assert!(checkin_due(&a, evening));
        a.management.checkin_date = Some(day(evening));
        assert!(!checkin_due(&a, evening));
        assert!(!checkin_due(&a, evening + 86400));
        accounts::note_generation_at(&a.id, evening + 86400);
        assert!(checkin_due(&a, evening + 86400));
        accounts::note_generation_at(&a.id, evening);
        a.management.checkin_date = None;
        a.management.checkin_attempt_at = evening;
        assert!(!checkin_due(&a, evening + 60));
        a.management.last_keepalive_at = evening;
        a.management.next_sync_at = evening + sync_delay(&a.id);
        assert!(!due(&a, evening + 60));
        // Inside the proactive window (30m) maintenance should renew before hot-path blocking.
        a.expires_at = evening + 60 + 20 * 60;
        a.management.keepalive_attempt_at = evening - 301;
        assert!(keepalive_due(&a, evening + 60));
        assert!(due(&a, evening + 60));
        a.expires_at = evening + 60 + accounts::PROACTIVE_REFRESH_SECS + 60;
        a.management.last_keepalive_at = 0;
        assert!(!keepalive_due(&a, evening + 60));
        a.expires_at = evening + 100;
        assert!(due(&a, evening + 60));
        a.management.managed = false;
        assert!(!due(&a, evening + 60));
        a.management.managed = true;
        a.enabled = false;
        assert!(!due(&a, evening + 60));
        a.realm = Realm::Global;
        assert!(!checkin_due(&a, evening + 86400));
        let mut old = serde_json::to_value(&a).unwrap();
        old.as_object_mut().unwrap().remove("management");
        let restored: Account = serde_json::from_value(old).unwrap();
        assert!(restored.management.managed);
        assert!(!restored.management.auto_checkin);
        assert!(!restored.management.auto_checkin_chosen);
        assert!(restored.management.keepalive);
        assert!(restored.management.credits.is_none());
    }
    #[tokio::test]
    async fn workbuddy_maintenance_persists_checkin_keepalive_and_stale_credits() {
        use axum::{
            http::HeaderMap,
            routing::{get, post},
            Json, Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let checkins = Arc::new(AtomicUsize::new(0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        let resource_calls = Arc::new(AtomicUsize::new(0));
        let c = checkins.clone();
        let r = refreshes.clone();
        let resources = resource_calls.clone();
        let app = Router::new()
            .route(
                "/v2/plugin/auth/token/refresh",
                post(move || {
                    let r = r.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"code":0,"data":{"accessToken":"fresh","expiresIn":3600}}))
                    }
                }),
            )
            .route(
                "/v2/billing/meter/daily-checkin",
                post(move |h: HeaderMap| {
                    let c = c.clone();
                    async move {
                        assert_eq!(h["authorization"], "Bearer fresh");
                        assert_eq!(h["x-user-id"], "managed");
                        c.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({"code":10001,"msg":"今天已签到，请明天再来"})),
                        )
                    }
                }),
            )
            .route(
                "/v2/billing/meter/get-user-resource",
                post(move || {
                    let resources = resources.clone();
                    async move {
                        if resources.fetch_add(1, Ordering::SeqCst) == 0 {
                            Json(json!({"code":0,"data":{"Accounts":[{"CapacityRemain":42}]}}))
                        } else {
                            Json(json!({"code":1234,"data":null}))
                        }
                    }
                }),
            )
            .route(
                "/console/enterprises/personal/models",
                get(|| async { Json(json!({"code":0,"data":{"models":[{"id":"test-model"}]}})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        struct Mock(tokio::task::JoinHandle<()>);
        impl Drop for Mock {
            fn drop(&mut self) {
                self.0.abort();
                std::env::remove_var("STELLAR_TEST_WORKBUDDY_BASE");
            }
        }
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let id=accounts::upsert(accounts::parse_import(json!({"uid":"managed","accessToken":"old","refreshToken":"r","expiresAt":accounts::now()+3600})).unwrap()).unwrap().id;
        run(&id, true, false, false).await.unwrap();
        // Check-in alone must not poll models and credits, even when done manually.
        let a = run(&id, false, true, false).await.unwrap();
        assert_eq!(a.management.checkin_status, "already");
        assert!(a.management.checkin_error.is_none());
        assert!(a.management.checkin_at > 0);
        assert_eq!(a.management.credits.unwrap().available, 42.0);
        assert!(a.management.last_keepalive_at > 0);
        // Reloading persisted state must suppress same-day check-in and daily refresh.
        run(&id, false, false, true).await.unwrap();
        assert_eq!(checkins.load(Ordering::SeqCst), 1);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        let a = run(&id, false, false, false).await.unwrap();
        assert_eq!(a.management.credits.unwrap().available, 42.0);
        assert!(a.management.credits_error.is_some());
        accounts::set_enabled(&id, false).unwrap();
        accounts::edit_management(&id, |m| m.next_sync_at = 0).unwrap();
        run(&id, false, false, true).await.unwrap();
        assert_eq!(resource_calls.load(Ordering::SeqCst), 2);
        let mut global = accounts::get(&id).unwrap();
        global.realm = Realm::Global;
        assert_eq!(
            super::super::resources::checkin(&global).await.unwrap(),
            "unsupported"
        );
        assert_eq!(checkins.load(Ordering::SeqCst), 1);
        // An idle account whose token is near expiry renews it without polling
        // the catalog, and its sync schedule stays where it was.
        accounts::set_enabled(&id, true).unwrap();
        let mut expiring = accounts::get(&id).unwrap();
        expiring.realm = Realm::Cn;
        expiring.expires_at = accounts::now() + 60;
        accounts::upsert(expiring).unwrap();
        accounts::edit_management(&id, |m| m.keepalive_attempt_at = 0).unwrap();
        let before = accounts::get(&id).unwrap().management;
        let (refreshed, polled) = (
            refreshes.load(Ordering::SeqCst),
            resource_calls.load(Ordering::SeqCst),
        );
        run(&id, false, false, true).await.unwrap();
        let after = accounts::get(&id).unwrap().management;
        assert_eq!(refreshes.load(Ordering::SeqCst), refreshed + 1);
        assert_eq!(resource_calls.load(Ordering::SeqCst), polled);
        assert_eq!(after.last_sync_at, before.last_sync_at);
        assert_eq!(after.next_sync_at, before.next_sync_at);
    }

    #[tokio::test]
    async fn workbuddy_checkin_manual_and_automatic_share_persisted_safety_controls() {
        use axum::{http::StatusCode, routing::post, Json, Router};
        use std::sync::{
            atomic::{AtomicU16, AtomicUsize, Ordering},
            Arc,
        };
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let status = Arc::new(AtomicU16::new(403));
        let calls = Arc::new(AtomicUsize::new(0));
        let unexpected = Arc::new(AtomicUsize::new(0));
        let unhandled = unexpected.clone();
        let (s, c) = (status.clone(), calls.clone());
        let app = Router::new()
            .route(
                "/v2/billing/meter/daily-checkin",
                post(move || {
                    let (s, c) = (s.clone(), c.clone());
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        let status = s.load(Ordering::SeqCst);
                        (
                            StatusCode::from_u16(status).unwrap(),
                            [("retry-after", "172800")],
                            Json(json!({"code": if status == 200 {0} else {12345}})),
                        )
                    }
                }),
            )
            .fallback(move || {
                let unhandled = unhandled.clone();
                async move {
                    unhandled.fetch_add(1, Ordering::SeqCst);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        struct Mock(tokio::task::JoinHandle<()>);
        impl Drop for Mock {
            fn drop(&mut self) {
                self.0.abort();
                std::env::remove_var("STELLAR_TEST_WORKBUDDY_BASE");
            }
        }
        let _mock = Mock(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        for http in [403, 401, 429, 503, 200] {
            status.store(http, Ordering::SeqCst);
            let auth = json!({"uid":format!("checkin-{http}"),"accessToken":"valid","expiresAt":accounts::now()+86400});
            let id = accounts::upsert(accounts::parse_import(auth.clone()).unwrap())
                .unwrap()
                .id;
            accounts::configure(
                &id,
                accounts::Preferences {
                    managed: Some(true),
                    auto_checkin: Some(true),
                    keepalive: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
            // Idle automatic maintenance performs no check-in.
            let before = calls.load(Ordering::SeqCst);
            run(&id, false, false, true).await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), before);
            let result = run(&id, false, true, false).await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), before + 1);
            assert_eq!(result.management.checkin_guard.attempts, 1);
            if http == 200 {
                assert_eq!(result.management.checkin_status, "success");
                run(&id, false, true, false).await.unwrap();
            } else {
                assert!(
                    result.management.checkin_guard.next_attempt_at >= accounts::now() + 172799
                );
                assert!(run(&id, false, true, false).await.is_err());
                accounts::configure(
                    &id,
                    accounts::Preferences {
                        auto_checkin: Some(false),
                        ..Default::default()
                    },
                )
                .unwrap();
                accounts::configure(
                    &id,
                    accounts::Preferences {
                        auto_checkin: Some(true),
                        ..Default::default()
                    },
                )
                .unwrap();
                let saved = accounts::get(&id).unwrap();
                assert_eq!(saved.management.checkin_guard.attempts, 1);
                assert_eq!(
                    saved.management.checkin_guard.next_attempt_at,
                    result.management.checkin_guard.next_attempt_at
                );
                if http == 401 {
                    assert!(resume_checkin(&id).await.is_err());
                    accounts::upsert(accounts::parse_import(auth).unwrap()).unwrap();
                    assert!(accounts::get(&id)
                        .unwrap()
                        .management
                        .checkin_guard
                        .pause
                        .is_none());
                } else if http == 403 {
                    resume_checkin(&id).await.unwrap();
                }
                // Recovery/reauthorization must not bypass a server's waiting time.
                assert!(run(&id, false, true, false).await.is_err());
            }
            assert_eq!(calls.load(Ordering::SeqCst), before + 1);
            assert_eq!(
                unexpected.load(Ordering::SeqCst),
                0,
                "Check-in must not poll other endpoints"
            );
            accounts::set_enabled(&id, false).unwrap();
            assert!(run(&id, false, true, false).await.is_err());
        }
    }
}
