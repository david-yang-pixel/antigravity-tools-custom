use super::*;
use serde_json::json;
use std::collections::{HashMap, HashSet};

struct Sandbox {
    _guard: std::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
    overrides: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl Sandbox {
    fn new() -> Self {
        let guard = crate::modules::account::SHARED_TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ABV_DATA_DIR");
        let overrides = [
            "ABV_QODER_ENABLED",
            "ABV_TRAE_ENABLED",
            "ABV_QODER_MAX_CONCURRENCY",
            "ABV_TRAE_MAX_CONCURRENCY",
            "ABV_WORKBUDDY_ENABLED",
            "ABV_WORKBUDDY_MODELS",
            "ABV_CUSTOM_UPSTREAMS",
        ]
        .into_iter()
        .map(|key| {
            let value = std::env::var_os(key);
            std::env::remove_var(key);
            (key, value)
        })
        .collect();
        std::env::set_var("ABV_DATA_DIR", dir.path());
        Self {
            _guard: guard,
            _dir: dir,
            previous,
            overrides,
        }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        for (key, value) in &self.overrides {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        if let Some(value) = &self.previous {
            std::env::set_var("ABV_DATA_DIR", value);
        } else {
            std::env::remove_var("ABV_DATA_DIR");
        }
    }
}
fn account(provider: Provider, uid: &str) -> Account {
    let mut a = Account::new(provider, Realm::Cn);
    a.uid = uid.into();
    a.nickname = uid.into();
    a.credentials.access_token = "test-access-private".into();
    a.credentials.refresh_token = "test-refresh-private".into();
    a.expires_at = now() + 7200;
    a.models = vec![Model {
        id: "shared-model".into(),
        name: "Shared".into(),
        ..Default::default()
    }];
    a
}

#[test]
fn qoder_quota_includes_shared_pool_and_distinguishes_unlimited_from_unknown() {
    let q = qoder::parse_quota(&json!({"userQuota":{"total":100,"remaining":0,"used":100},"addOnQuota":{"total":10,"remaining":5,"used":5},"orgResourcePackage":{"cap":2500,"remaining":952,"used":1548,"available":true}})).unwrap();
    assert_eq!(q.remaining, Some(957.0));
    assert_eq!(q.total, Some(2610.0));
    assert!(!q.exhausted());
    let unlimited = qoder::parse_quota(&json!({"userQuota":{"total":0,"remaining":0,"used":0},"orgResourcePackage":{"cap":-1,"remaining":0,"used":0}})).unwrap();
    assert!(unlimited.unlimited);
    assert_eq!(unlimited.remaining, None);
    assert!(!unlimited.exhausted());
    assert!(qoder::parse_quota(&json!({"message":"temporarily unavailable"})).is_err());
    assert!(qoder::parse_quota(&json!({"isQuotaExceeded":true,"userQuota":{}})).is_err());
}
#[test]
fn qoder_standard_zero_credits_are_a_valid_quota_snapshot() {
    let quota = qoder::parse_quota(&json!({
        "userType":"personal_standard", "usageType":"credits", "isQuotaExceeded":true,
        "userQuota":{"total":0,"used":0,"remaining":0,"percentage":0,"unit":"credits"}
    }))
    .unwrap();
    assert_eq!(quota.total, Some(0.0));
    assert_eq!(quota.remaining, Some(0.0));
    assert_eq!(quota.used, Some(0.0));
    assert!(!quota.unlimited);
    assert!(quota.exhausted());
}
#[test]
fn qoder_catalog_uses_official_free_metadata_and_keeps_disabled_models_unavailable() {
    let models = qoder::parse_models(&json!({"chat":[
        {"key":"free-flag","enable":true,"is_free":true,"price_factor":0},
        {"key":"zero-price","enable":true,"price_factor":0},
        {"key":"paid","enable":true,"is_free":false,"price_factor":0},
        {"key":"unknown","enable":true},
        {"key":"disabled","enable":false,"is_free":true}
    ]}))
    .unwrap();
    assert_eq!(models.len(), 4);
    for id in ["free-flag", "zero-price"] {
        assert!(models.iter().find(|model| model.id == id).unwrap().is_free);
    }
    for id in ["paid", "unknown"] {
        assert!(!models.iter().find(|model| model.id == id).unwrap().is_free);
    }
    let legacy: Model = serde_json::from_value(json!({"id":"old-model","name":"Old"})).unwrap();
    assert!(!legacy.is_free);
}
#[test]
fn free_models_with_zero_credits_keep_concurrency_and_failure_cooldowns() {
    let _sandbox = Sandbox::new();
    for status in [401, 429] {
        let mut a = account(Provider::Qoder, &format!("free-account-{status}"));
        a.models = qoder::parse_models(&json!({"chat":[
            {"key":"free-model","enable":true,"is_free":true},
            {"key":"paid-model","enable":true,"is_free":false}
        ]}))
        .unwrap();
        a.quota = Some(
            qoder::parse_quota(&json!({"userQuota":{"total":0,"used":0,"remaining":0}})).unwrap(),
        );
        let a = store::upsert(a, None).unwrap();
        let mut lease = accounts::select(Provider::Qoder, "free-model", &HashSet::new()).unwrap();
        assert_eq!(lease.account.id, a.id);
        assert!(accounts::select(Provider::Qoder, "paid-model", &HashSet::new()).is_err());
        assert!(accounts::select(Provider::Qoder, "free-model", &HashSet::new()).is_err());
        lease.failure(status);
        drop(lease);
        assert!(accounts::select(Provider::Qoder, "free-model", &HashSet::new()).is_err());
    }
}
#[test]
fn quota_expiry_and_missing_usage_do_not_invent_zero_balance() {
    let q = trae::parse_quota(&json!({"data":{"user_entitlement_pack_list":[{"entitlement_base_info":{"quota":{"credits_limit":100}},"usage":{"credits_amount":25}},
        {"entitlement_base_info":{"end_time":now()-1,"quota":{"credits_limit":500}},"usage":{"credits_amount":10}}]}})).unwrap();
    assert_eq!(q.total, Some(100.0));
    assert_eq!(q.remaining, Some(75.0));
    let unknown = trae::parse_quota(&json!({"user_entitlement_pack_list":[{"entitlement_base_info":{"quota":{"credits_limit":100}}}]})).unwrap();
    assert_eq!(unknown.remaining, None);
    assert!(!unknown.unlimited);
    assert!(!unknown.exhausted());
    assert!(trae::parse_quota(
        &json!({"user_entitlement_pack_list":[{"quota":{"future_limit":100}}]})
    )
    .is_err());
}
#[test]
fn trae_legacy_quota_and_unlimited_are_recognized() {
    let q = trae::parse_quota(&json!({"Data":{"userEntitlementPackList":[{"entitlementBaseInfo":{"quota":{"basicUsageLimit":-1}},"usage":{"basicUsageAmount":90}}]}})).unwrap();
    assert!(q.unlimited);
    assert_eq!(q.remaining, None);
    assert_eq!(q.used, Some(90.0));
}
#[test]
fn token_rotation_preserves_refresh_token_when_omitted_and_converts_epoch_units() {
    let mut a = account(Provider::Qoder, "uid");
    let expiry = now() + 3600;
    qoder::apply_tokens(
        &mut a,
        &json!({"token":"new-access","expires_at":expiry * 1000}),
    )
    .unwrap();
    assert_eq!(a.expires_at, expiry);
    assert_eq!(a.credentials.refresh_token, "test-refresh-private");
    trae::apply_tokens(&mut a, &json!({"Result":{"Token":"trae-access","RefreshToken":"rotated","TokenExpireAt":expiry*1000}})).unwrap();
    assert_eq!(a.expires_at, expiry);
    assert_eq!(a.credentials.refresh_token, "rotated");
    assert!(trae::apply_tokens(&mut a, &json!({"Result":{}})).is_err());
}
#[test]
fn identity_and_callback_urls_cannot_choose_an_untrusted_credential_destination() {
    for bad in [
        "https://qoder.com.attacker.example/a",
        "https://qoder.sh@evil.example/a",
        "http://openapi.qoder.sh/a",
        "https://openapi.qoder.sh:444/a",
    ] {
        assert!(http::official_url(Provider::Qoder, bad).is_err());
    }
    assert!(
        http::official_url(Provider::Qoder, "https://openapi.qoder.sh/api/v1/userinfo").is_ok()
    );
    assert!(http::official_url(
        Provider::Trae,
        "https://coresg-normal.trae.ai/api/agent/v3/llm_utils_chat"
    )
    .is_ok());
    let mut query = HashMap::from([("state".into(), "other-session".into())]);
    assert!(login::validate_callback(&query, "session", "trace").is_err());
    query = HashMap::from([("login_trace_id".into(), "other-trace".into())]);
    assert!(login::validate_callback(&query, "session", "trace").is_err());
}
#[test]
fn account_vault_and_public_projection_never_expose_tokens() {
    let _sandbox = Sandbox::new();
    let a = store::upsert(account(Provider::Qoder, "private-user"), None).unwrap();
    let disk = std::fs::read_to_string(
        crate::modules::account::get_data_dir()
            .unwrap()
            .join("qoder_accounts.enc"),
    )
    .unwrap();
    assert!(!disk.contains("test-access-private"));
    assert!(!disk.contains("private-user"));
    let view = serde_json::to_string(&AccountView::from(&a)).unwrap();
    assert!(!view.contains("test-access-private"));
    assert!(!view.contains("test-refresh-private"));
    assert!(!view.contains("credentials"));
    assert_eq!(
        store::get(Provider::Qoder, &a.id)
            .unwrap()
            .credentials
            .refresh_token,
        "test-refresh-private"
    );
    assert!(store::all(Provider::Trae).unwrap().is_empty());
}
#[test]
fn reauthorization_and_deletion_reject_stale_refresh_writes() {
    let _sandbox = Sandbox::new();
    let a = store::upsert(account(Provider::Trae, "u"), None).unwrap();
    let mut replacement = account(Provider::Trae, "u");
    replacement.credentials.refresh_token = "new-refresh".into();
    let replacement = store::upsert(replacement, Some(&a.id)).unwrap();
    assert_eq!(replacement.id, a.id);
    assert!(
        store::update(Provider::Trae, &a.id, Some(&a.revision), |a| a
            .credentials
            .refresh_token =
            "stale".into())
        .is_err()
    );
    assert_eq!(
        store::get(Provider::Trae, &a.id)
            .unwrap()
            .credentials
            .refresh_token,
        "new-refresh"
    );
    assert!(store::upsert(account(Provider::Trae, "different-user"), Some(&a.id)).is_err());
    store::remove(Provider::Trae, &a.id).unwrap();
    assert!(store::update(Provider::Trae, &a.id, None, |_| {}).is_err());
}
#[test]
fn scheduler_rotates_limits_concurrency_and_skips_cooling_or_exhausted_accounts() {
    let _sandbox = Sandbox::new();
    let a = store::upsert(account(Provider::Qoder, "a"), None).unwrap();
    let b = store::upsert(account(Provider::Qoder, "b"), None).unwrap();
    let mut first = accounts::select(Provider::Qoder, "shared-model", &HashSet::new()).unwrap();
    assert_eq!(first.account.id, a.id);
    let second = accounts::select(Provider::Qoder, "shared-model", &HashSet::new()).unwrap();
    assert_eq!(second.account.id, b.id);
    assert!(accounts::select(Provider::Qoder, "shared-model", &HashSet::new()).is_err());
    first.failure(429);
    drop(first);
    drop(second);
    let next = accounts::select(Provider::Qoder, "shared-model", &HashSet::new()).unwrap();
    assert_eq!(next.account.id, b.id);
    drop(next);
    store::update(Provider::Qoder, &b.id, None, |a| {
        a.quota = Some(Quota::from_packs(
            "credits",
            vec![QuotaPack {
                name: "empty".into(),
                total: Some(10.0),
                remaining: Some(0.0),
                used: Some(10.0),
                unlimited: false,
                expires_at: None,
            }],
        ))
    })
    .unwrap();
    assert!(accounts::select(Provider::Qoder, "shared-model", &HashSet::new()).is_err());
}

#[tokio::test]
async fn native_pools_follow_live_modes_and_global_session_reset() {
    use crate::proxy::sticky_config::{
        AccountSchedulingPolicy as Policy, SchedulingMode, StickySessionConfig,
    };
    let _sandbox = Sandbox::new();
    for provider in Provider::ALL {
        store::upsert(account(provider, "global-a"), None).unwrap();
        store::upsert(account(provider, "global-b"), None).unwrap();
    }
    let manager = crate::proxy::TokenManager::new(crate::modules::account::get_data_dir().unwrap());
    let deadline = || tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    for provider in Provider::ALL {
        manager
            .update_sticky_config(StickySessionConfig {
                mode: SchedulingMode::Balance,
                max_wait_seconds: 0,
            })
            .await;
        let policy = Policy::live(&manager, None).await;
        let first = accounts::scheduling::acquire(
            provider,
            "shared-model",
            "user-a:conversation",
            &policy,
            &HashSet::new(),
            deadline(),
        )
        .await
        .unwrap();
        let id = first.account.id.clone();
        drop(first);
        let same = accounts::scheduling::acquire(
            provider,
            "shared-model",
            "user-a:conversation",
            &policy,
            &HashSet::new(),
            deadline(),
        )
        .await
        .unwrap();
        assert_eq!(same.account.id, id);
        drop(same);
        let other = accounts::scheduling::acquire(
            provider,
            "shared-model",
            "user-b:conversation",
            &policy,
            &HashSet::new(),
            deadline(),
        )
        .await
        .unwrap();
        assert_ne!(other.account.id, id);
        drop(other);
        manager.clear_all_sessions();
        let rebound = accounts::scheduling::acquire(
            provider,
            "shared-model",
            "user-a:conversation",
            &policy,
            &HashSet::new(),
            deadline(),
        )
        .await
        .unwrap();
        assert_eq!(rebound.account.id, id);
        drop(rebound);
        // Clearing a real binding must release affinity, not merely the AG pool's map.
        manager.clear_all_sessions();
        let reassigned = accounts::scheduling::acquire(
            provider,
            "shared-model",
            "user-a:conversation",
            &policy,
            &HashSet::new(),
            deadline(),
        )
        .await
        .unwrap();
        assert_ne!(reassigned.account.id, id);
        drop(reassigned);
        manager
            .update_sticky_config(StickySessionConfig {
                mode: SchedulingMode::PerformanceFirst,
                max_wait_seconds: 60,
            })
            .await;
        let performance = Policy::live(&manager, None).await;
        let first = accounts::scheduling::acquire(
            provider,
            "shared-model",
            "same-conversation",
            &performance,
            &HashSet::new(),
            deadline(),
        )
        .await
        .unwrap();
        let id = first.account.id.clone();
        drop(first);
        let next = accounts::scheduling::acquire(
            provider,
            "shared-model",
            "same-conversation",
            &performance,
            &HashSet::new(),
            deadline(),
        )
        .await
        .unwrap();
        assert_ne!(next.account.id, id);
    }
}

#[tokio::test]
async fn native_cache_first_waits_for_its_account_and_honors_both_deadlines() {
    use crate::proxy::sticky_config::{AccountSchedulingPolicy as Policy, SchedulingMode};
    use std::time::Duration;
    let _sandbox = Sandbox::new();
    store::upsert(account(Provider::Qoder, "wait-a"), None).unwrap();
    store::upsert(account(Provider::Qoder, "wait-b"), None).unwrap();
    let mut policy = Policy::default();
    policy.sticky.mode = SchedulingMode::CacheFirst;
    policy.sticky.max_wait_seconds = 1;
    let excluded = HashSet::new();
    let deadline = || tokio::time::Instant::now() + Duration::from_secs(3);
    let first = accounts::scheduling::acquire(
        Provider::Qoder,
        "shared-model",
        "waiting",
        &policy,
        &excluded,
        deadline(),
    )
    .await
    .unwrap();
    let id = first.account.id.clone();
    let release = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(first);
    };
    let (next, _) = tokio::join!(
        accounts::scheduling::acquire(
            Provider::Qoder,
            "shared-model",
            "waiting",
            &policy,
            &excluded,
            deadline()
        ),
        release
    );
    let mut next = next.unwrap();
    assert_eq!(next.account.id, id);
    policy.sticky.max_wait_seconds = 0;
    let error = tokio::time::timeout(
        Duration::from_millis(100),
        accounts::scheduling::acquire(
            Provider::Qoder,
            "shared-model",
            "waiting",
            &policy,
            &excluded,
            deadline(),
        ),
    )
    .await
    .unwrap()
    .err()
    .unwrap();
    assert_eq!(error.status, 429);
    next.failure(429);
    drop(next);
    // Balance may move to the free account, Cache First retains the cooling account.
    let mut balance = policy.clone();
    balance.sticky.mode = SchedulingMode::Balance;
    let fallback = accounts::scheduling::acquire(
        Provider::Qoder,
        "shared-model",
        "waiting",
        &balance,
        &excluded,
        deadline(),
    )
    .await
    .unwrap();
    assert_ne!(fallback.account.id, id);
    drop(fallback);
    accounts::scheduling::clear_sessions();
    accounts::scheduling::clear_cooldowns(None);
    let mut cooling = accounts::scheduling::acquire(
        Provider::Qoder,
        "shared-model",
        "deadline",
        &policy,
        &excluded,
        deadline(),
    )
    .await
    .unwrap();
    cooling.failure(429);
    drop(cooling);
    policy.sticky.max_wait_seconds = 1;
    let began = tokio::time::Instant::now();
    let error = accounts::scheduling::acquire(
        Provider::Qoder,
        "shared-model",
        "deadline",
        &policy,
        &excluded,
        began + Duration::from_millis(30),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.status, 504);
    assert!(began.elapsed() < Duration::from_millis(250));
    let began = tokio::time::Instant::now();
    let error = accounts::scheduling::acquire(
        Provider::Qoder,
        "shared-model",
        "deadline",
        &policy,
        &excluded,
        deadline(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.status, 429);
    assert!(began.elapsed() >= Duration::from_millis(900));
}

#[tokio::test]
async fn native_fixed_account_never_silently_falls_back() {
    use crate::proxy::sticky_config::AccountSchedulingPolicy as Policy;
    let _sandbox = Sandbox::new();
    let a = store::upsert(account(Provider::Qoder, "fixed-a"), None).unwrap();
    let b = store::upsert(account(Provider::Qoder, "fixed-b"), None).unwrap();
    store::save_settings(
        Provider::Qoder,
        Settings {
            enabled: true,
            preferred_account_id: Some(b.id.clone()),
        },
    )
    .unwrap();
    let policy = Policy::default();
    let fixed = accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "fixed",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(fixed.account.id, b.id);
    assert!(accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "fixed",
        &policy,
        &HashSet::new()
    )
    .unwrap()
    .is_none());
    drop(fixed);
    // An explicit model test may choose a different account without changing the
    // source's fixed account for ordinary inference.
    let probe = accounts::select_for_probe(
        Provider::Qoder,
        "shared-model",
        &HashSet::new(),
        Some(&a.id),
    )
    .unwrap();
    assert_eq!(probe.account.id, a.id);
    drop(probe);
    accounts::configure(Provider::Qoder, &b.id, Some(false), None).unwrap();
    assert!(accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "fixed",
        &policy,
        &HashSet::new()
    )
    .is_err());
    store::remove(Provider::Qoder, &b.id).unwrap();
    assert!(accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "fixed",
        &policy,
        &HashSet::new()
    )
    .is_err());
    store::save_settings(
        Provider::Qoder,
        Settings {
            enabled: true,
            preferred_account_id: None,
        },
    )
    .unwrap();
    assert_eq!(
        accounts::scheduling::try_acquire(
            Provider::Qoder,
            "shared-model",
            "fixed",
            &policy,
            &HashSet::new()
        )
        .unwrap()
        .unwrap()
        .account
        .id,
        a.id
    );
}

#[tokio::test]
async fn native_breaker_and_resets_follow_the_global_manager() {
    use crate::proxy::sticky_config::AccountSchedulingPolicy as Policy;
    let _sandbox = Sandbox::new();
    let a = store::upsert(account(Provider::Qoder, "breaker-qoder"), None).unwrap();
    let b = store::upsert(account(Provider::Trae, "breaker-trae"), None).unwrap();
    let manager = crate::proxy::TokenManager::new(crate::modules::account::get_data_dir().unwrap());
    manager
        .update_circuit_breaker_config(crate::models::CircuitBreakerConfig {
            backoff_steps: vec![2, 10],
            ..Default::default()
        })
        .await;
    let policy = Policy::live(&manager, None).await;
    let mut lease = accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    lease.failure(429);
    drop(lease);
    assert!((1..=2).contains(&(accounts::runtime(&a.id).cooldown_until - now())));
    assert!(manager.clear_rate_limit(&a.id));
    let mut lease = accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    lease.failure_after(429, Some(120));
    drop(lease);
    assert!(accounts::runtime(&a.id).cooldown_until >= now() + 119);
    let mut lease = accounts::scheduling::try_acquire(
        Provider::Trae,
        "shared-model",
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    lease.failure(503);
    drop(lease);
    manager.clear_all_rate_limits();
    assert_eq!(accounts::runtime(&a.id).cooldown_until, 0);
    assert_eq!(accounts::runtime(&b.id).cooldown_until, 0);
    let mut lease = accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    lease.failure(503);
    drop(lease);
    manager
        .update_circuit_breaker_config(crate::models::CircuitBreakerConfig {
            enabled: false,
            ..Default::default()
        })
        .await;
    let off = Policy::live(&manager, None).await;
    assert_eq!(accounts::runtime(&a.id).cooldown_until, 0);
    let mut lease = accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "",
        &off,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    assert!(accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "",
        &off,
        &HashSet::new()
    )
    .unwrap()
    .is_none());
    lease.failure(503);
    drop(lease);
    assert_eq!(accounts::runtime(&a.id).cooldown_until, 0);
    assert!(accounts::scheduling::try_acquire(
        Provider::Qoder,
        "shared-model",
        "",
        &off,
        &HashSet::new()
    )
    .unwrap()
    .is_some());
}
#[tokio::test]
async fn model_tests_respect_source_disablement_and_in_flight_permits() {
    let _sandbox = Sandbox::new();
    let account = store::upsert(account(Provider::Qoder, "probe-user"), None).unwrap();
    let lease = accounts::select(Provider::Qoder, "shared-model", &HashSet::new()).unwrap();
    assert!(crate::proxy::upstream::native::probe(
        Provider::Qoder,
        "shared-model",
        Some(&account.id),
        "system",
        "user"
    )
    .await
    .is_err());
    drop(lease);
    store::save_settings(
        Provider::Qoder,
        Settings {
            enabled: false,
            preferred_account_id: None,
        },
    )
    .unwrap();
    assert!(crate::proxy::upstream::native::probe(
        Provider::Qoder,
        "shared-model",
        Some(&account.id),
        "system",
        "user"
    )
    .await
    .is_err());
}

#[test]
fn provider_streams_preserve_text_tools_usage_and_surface_envelope_errors() {
    let wrapped = json!({"statusCodeValue":200,"body":json!({"choices":[{"index":0,"delta":{"content":"hello"}}]}).to_string()});
    let decoded: Value = serde_json::from_str(
        &wire::event(Provider::Qoder, "message", &wrapped.to_string())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(decoded["choices"][0]["delta"]["content"], "hello");
    assert_eq!(
        wire::event(
            Provider::Qoder,
            "message",
            &json!({"statusCodeValue":429,"body":"rate limited"}).to_string()
        )
        .unwrap_err()
        .status,
        429
    );
    let chunk = wire::event(Provider::Trae, "output", &json!({"response":"text","tool_calls":[{"id":"call-1","function_call":{"name":"read_file","arguments":"{}"}}]}).to_string()).unwrap().unwrap();
    let chunk: Value = serde_json::from_str(&chunk).unwrap();
    assert_eq!(
        chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );
    assert!(
        wire::event(Provider::Trae, "progress_notice", "\"working\"")
            .unwrap()
            .is_none()
    );
    assert!(wire::event(Provider::Trae, "error", "{\"code\":429}").is_err());
}
#[test]
fn payload_translation_keeps_tool_history_and_provider_model_identity() {
    let a = account(Provider::Trae, "u");
    let payload = wire::trae_request(&a, "shared-model", json!({"messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"call","function":{"name":"read","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call","content":"result"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]})).unwrap();
    assert_eq!(payload["function"], "chat_v3");
    assert_eq!(payload["config_name"], "shared-model");
    assert!(payload["tools"][0]["function"]["parameters"].is_string());
    assert_eq!(
        payload["messages"][0]["tool_calls"][0]["function_call"]["name"],
        "read"
    );
    assert_eq!(payload["messages"][1]["tool_call_id"], "call");
    let payload = wire::qoder_request(
        &account(Provider::Qoder, "u"),
        "shared-model",
        json!({"messages":[{"role":"user","content":"hi"}],"tools":[]}),
    )
    .unwrap();
    assert_eq!(payload["model_config"]["key"], "shared-model");
    assert_eq!(payload["business"]["model_key"], "shared-model");
    assert_eq!(payload["tools"], json!([]));
}

#[test]
fn cosy_signing_uses_the_encoded_wire_bytes_and_rotates_cached_identity() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use sha2::Digest;
    let mut a = account(Provider::Qoder, "signing-user");
    let body = qoder::sign::encode(b"{}");
    assert_eq!(body, "$kwm");
    let url = "https://api3.qoder.sh/algo/api/v2/model/list?Encode=1";
    let headers = qoder::sign::headers(&a, &body, url, "", false).unwrap();
    let auth = headers["authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Bearer COSY.")
        .unwrap();
    let (payload, signature) = auth.rsplit_once('.').unwrap();
    let expected = format!(
        "{:x}",
        md5::Md5::digest(format!(
            "{payload}\n{}\n{}\n{body}\n/api/v2/model/list",
            headers["cosy-key"].to_str().unwrap(),
            headers["cosy-date"].to_str().unwrap()
        ))
    );
    assert_eq!(signature, expected);
    assert_eq!(
        STANDARD
            .decode(headers["cosy-key"].to_str().unwrap())
            .unwrap()
            .len(),
        128
    );
    let repeated = qoder::sign::headers(&a, &body, url, "", false).unwrap();
    assert_eq!(headers["cosy-key"], repeated["cosy-key"]);
    a.credentials.access_token = "new-token".into();
    let rotated = qoder::sign::headers(&a, &body, url, "", false).unwrap();
    assert_ne!(headers["cosy-key"], rotated["cosy-key"]);
}

#[tokio::test]
async fn native_model_discovery_requires_explicit_publication_and_keeps_source_bindings() {
    use crate::modules::model_registry as registry;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        Router,
    };
    use std::sync::Arc;
    use tower::ServiceExt;
    let _sandbox = Sandbox::new();
    let mapping = HashMap::new();
    for provider in Provider::ALL {
        store::upsert(account(provider, provider.key()), None).unwrap();
    }
    let view = registry::view(&[], &mapping).unwrap();
    assert!(view.published_models.is_empty());
    for provider in Provider::ALL {
        assert!(view
            .candidates
            .iter()
            .any(|c| c.source_id == provider.source() && c.native_model == "shared-model"));
    }
    let request = registry::SaveModelRegistryRequest {
        revision: view.revision,
        ignored_candidate_ids: vec![],
        allow_unverified: true,
        published_models: Provider::ALL
            .into_iter()
            .map(|p| registry::PublishedModel {
                id: format!("new:{}", p.key()),
                public_id: format!("{}-chat", p.key()),
                display_name: p.name().into(),
                source_id: p.source().into(),
                native_model: "shared-model".into(),
                enabled: true,
                legacy: false,
            })
            .collect(),
    };
    registry::save(request, &[], &mapping).unwrap();
    let monitor = Arc::new(crate::proxy::monitor::ProxyMonitor::disabled_for_test());
    let app = Router::new()
        .fallback(|| async { StatusCode::IM_A_TEAPOT })
        .layer(axum::middleware::from_fn_with_state(
            crate::proxy::server::test_app_state(monitor),
            crate::proxy::upstream::native::route,
        ))
        .layer(axum::middleware::from_fn(
            crate::proxy::middleware::model_registry::model_registry_middleware,
        ));
    for provider in Provider::ALL {
        let binding = registry::resolve_existing(&format!("{}-chat", provider.key()))
            .unwrap()
            .unwrap();
        assert_eq!(binding.source_id, provider.source());
        assert_eq!(binding.native_model, "shared-model");
        let admitted = crate::proxy::middleware::model_registry::resolve_public_model(&format!(
            "{}-chat",
            provider.key()
        ))
        .unwrap();
        assert_eq!(admitted.source_id, provider.source());
        assert_eq!(admitted.native_model, "shared-model");
        for account in store::all(provider).unwrap() {
            accounts::configure(provider, &account.id, Some(false), None).unwrap();
        }
        for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
            let mut payload = if path == "/v1/responses" {
                json!({"input":"hello","max_output_tokens":16})
            } else {
                json!({"messages":[{"role":"user","content":"hello"}],"max_tokens":16})
            };
            payload["model"] = json!(format!("{}-chat", provider.key()));
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            // Disabled accounts are configuration unavailability, not a busy pool.
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["code"], "native_upstream_failed");
        }
    }
}

#[test]
fn withdrawn_model_evidence_is_not_reused_and_broken_native_vault_is_isolated() {
    use crate::modules::model_registry as registry;
    let _sandbox = Sandbox::new();
    let mapping = HashMap::new();
    let mut a = account(Provider::Qoder, "qoder-user");
    a.verified_models.insert("shared-model".into(), now());
    let a = store::upsert(a, None).unwrap();
    let view = registry::view(&[], &mapping).unwrap();
    assert_eq!(
        view.candidates
            .iter()
            .find(|c| c.source_id == Provider::Qoder.source())
            .unwrap()
            .verified_account_count,
        1
    );
    store::update(Provider::Qoder, &a.id, None, |a| a.models.clear()).unwrap();
    let view = registry::view(&[], &mapping).unwrap();
    assert_eq!(
        view.candidates
            .iter()
            .find(|c| c.source_id == Provider::Qoder.source())
            .unwrap()
            .verified_account_count,
        0
    );
    std::fs::write(
        crate::modules::account::get_data_dir()
            .unwrap()
            .join("qoder_accounts.enc"),
        "damaged vault",
    )
    .unwrap();
    let view = registry::view(&[], &mapping).unwrap();
    assert!(
        !view
            .sources
            .iter()
            .find(|s| s.id == Provider::Qoder.source())
            .unwrap()
            .enabled
    );
    assert!(view
        .notices
        .iter()
        .any(|n| n.contains("Qoder") && n.contains("unavailable")));
    assert!(view.sources.iter().any(|s| s.id == Provider::Trae.source()));
}

#[tokio::test]
async fn device_authorization_is_pkce_bound_cancellable_and_provider_scoped() {
    let started = login::start(Provider::Qoder, Realm::Global, None, false)
        .await
        .unwrap();
    let url = url::Url::parse(&started.auth_url).unwrap();
    assert_eq!(url.host_str(), Some("qoder.com"));
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query["challenge_method"], "S256");
    assert_eq!(query["challenge"].len(), 43);
    assert!(!query.contains_key("verifier"));
    assert!(!query.contains_key("redirect_uri"));
    assert!(login::cancel(Provider::Trae, &started.id).await.is_err());
    login::cancel(Provider::Qoder, &started.id).await.unwrap();
    assert!(login::poll(Provider::Qoder, &started.id).await.is_err());
}

#[tokio::test]
async fn normalized_provider_chunks_keep_tools_and_usage_on_all_client_protocols() {
    use crate::proxy::upstream::{
        chat_codec::{Namespaced, Protocol},
        chat_relay::Relay,
    };
    use futures::StreamExt;
    for protocol in [Protocol::Chat, Protocol::Responses, Protocol::Messages] {
        for streaming in [false, true] {
            let frames = vec![
                json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}).to_string(),
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":null}]}).to_string(),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4}}}).to_string(),
                "[DONE]".into(),
            ];
            let response = Relay {
                protocol,
                alias: "published-alias".into(),
                namespaced: Namespaced::default(),
                headers: Default::default(),
                error_code: "native_upstream_failed",
            }
            .run(
                futures::stream::iter(frames.into_iter().map(Ok)).boxed(),
                streaming,
            )
            .await;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 128 * 1024)
                .await
                .unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains("hello"));
            assert!(body.contains("read_file"));
            assert!(body.contains("published-alias"));
            if !streaming {
                let v: Value = serde_json::from_str(&body).unwrap();
                match protocol {
                    Protocol::Chat => {
                        assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["id"], "call-1");
                        assert_eq!(v["usage"]["prompt_tokens"], 12);
                    }
                    Protocol::Responses => {
                        assert!(v["output"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|item| item["type"] == "function_call"));
                        assert_eq!(v["usage"]["output_tokens"], 3);
                    }
                    Protocol::Messages => {
                        assert!(v["content"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|item| item["type"] == "tool_use"));
                        assert_eq!(v["usage"]["output_tokens"], 3);
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn incomplete_generation_does_not_become_a_successful_completion() {
    use crate::proxy::upstream::{
        chat_codec::{Namespaced, Protocol},
        chat_relay::Relay,
    };
    use futures::StreamExt;
    let frames = vec![Ok(
        json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]})
            .to_string(),
    )];
    let response = Relay {
        protocol: Protocol::Chat,
        alias: "alias".into(),
        namespaced: Namespaced::default(),
        headers: Default::default(),
        error_code: "native_upstream_failed",
    }
    .run(futures::stream::iter(frames).boxed(), false)
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
}
