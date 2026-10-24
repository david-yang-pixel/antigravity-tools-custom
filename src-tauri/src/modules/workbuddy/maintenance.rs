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
    pub keepalive: bool,
    pub auto_rewards: bool,
    pub auto_lottery: bool,
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
}
impl Default for Management {
    fn default() -> Self {
        Self {
            managed: true,
            auto_checkin: true,
            keepalive: true,
            auto_rewards: false,
            auto_lottery: false,
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
fn checkin_due(a: &Account, now: i64) -> bool {
    if a.realm != Realm::Cn
        || !a.management.auto_checkin
        || a.management.checkin_date.as_deref() == Some(day(now).as_str())
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
    // Renew inside the proactive window so inference rarely hits a blocking refresh.
    a.management.keepalive
        && now - a.management.keepalive_attempt_at >= 300
        && (a.expires_at <= now + accounts::PROACTIVE_REFRESH_SECS
            || now - a.management.last_keepalive_at >= 86400)
}
fn due(a: &Account, now: i64) -> bool {
    if a.management.keepalive_error.is_some() && now - a.management.keepalive_attempt_at < 300 {
        return false;
    }
    a.enabled
        && a.management.managed
        && (now >= a.management.next_sync_at || keepalive_due(a, now) || checkin_due(a, now))
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
    let _guard = lock.lock().await;
    let initial = accounts::get(id)?;
    let now = accounts::now();
    if automatic && !due(&initial, now) {
        return accounts::view(initial);
    }
    // Persist the attempt before network I/O so a restart cannot cause a tight retry loop.
    // Per-account jitter keeps the pool from syncing in lockstep.
    accounts::edit_management(id, |m| m.next_sync_at = now + sync_delay(id))?;
    let refresh = force_refresh || (automatic && keepalive_due(&initial, now));
    let mut account = match accounts::fresh(id, refresh).await {
        Ok(account) => account,
        Err(_) => return accounts::view(accounts::get(id)?),
    };
    // Recheck disable/management toggles before starting optional side effects.
    let current = accounts::get(id)?;
    if automatic && (!current.enabled || !current.management.managed) {
        return accounts::view(current);
    }
    account.management.managed = current.management.managed;
    account.management.auto_checkin = current.management.auto_checkin;
    account.management.keepalive = current.management.keepalive;
    let should_checkin = checkin_now || (automatic && checkin_due(&account, now));
    if should_checkin && account.realm == Realm::Cn {
        accounts::edit_management(id, |m| m.checkin_attempt_at = now)?;
        account.management.checkin_attempt_at = now;
        match super::resources::checkin(&account).await {
            Ok(status) => {
                account.management.checkin_status = status.into();
                account.management.checkin_date = Some(day(now));
                account.management.checkin_at = accounts::now();
                account.management.checkin_error = None;
            }
            Err(error) => {
                account.management.checkin_status = "failed".into();
                account.management.checkin_error = Some(error);
            }
        }
    }
    if account.realm == Realm::Global {
        account.management.checkin_status = "unsupported".into();
    }
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
    accounts::save_observation(&mut account)?;
    super::register_models(&account.available_models())?;
    accounts::view(account)
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
        // Beijing 2026-10-20 00:00 — sync is due, but check-in waits for the habit slot.
        let midnight = 1792425600;
        let evening = midnight + 23 * 3600; // past the 22:30 same-day fallback
        assert!(due(&a, midnight));
        assert!(!checkin_due(&a, midnight));
        assert!(checkin_due(&a, evening));
        a.management.checkin_date = Some(day(evening));
        assert!(!checkin_due(&a, evening));
        assert!(checkin_due(&a, evening + 86400));
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
        // Force refresh+check-in: habit slots are daytime-only and would skip around midnight.
        let a = run(&id, true, true, false).await.unwrap();
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
    }
}
