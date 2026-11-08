use super::*;
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub(crate) struct Sandbox {
    _guard: std::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
    overrides: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl Sandbox {
    pub(crate) fn new() -> Self {
        let guard = crate::modules::account::SHARED_TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ABV_DATA_DIR");
        let overrides = [
            "ABV_CHATGPT_ENABLED",
            "ABV_CHATGPT_MAX_CONCURRENCY",
            "ABV_CHATGPT_EGRESS_ENABLED",
            "ABV_CHATGPT_CLIENT_VERSION",
            "ABV_CHATGPT_REAUTH_URL",
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
/// Points ChatGPT at a closed local port so a test that only expects failures can never
/// reach the real service; restores the production endpoints on drop.
pub(super) struct OfflineChatGpt;
impl OfflineChatGpt {
    pub(super) fn new() -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .unwrap();
        chatgpt::endpoints::set_test(Some(chatgpt::endpoints::Endpoints {
            auth: format!("http://127.0.0.1:{port}"),
            backend: format!("http://127.0.0.1:{port}/backend-api"),
        }));
        Self
    }
}
impl Drop for OfflineChatGpt {
    fn drop(&mut self) {
        chatgpt::endpoints::set_test(None);
    }
}
fn account(provider: Provider, uid: &str) -> Account {
    let mut a = Account::new(provider, Realm::Global);
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
fn account_vault_and_public_projection_never_expose_tokens() {
    let _sandbox = Sandbox::new();
    let mut a = account(Provider::ChatGpt, "private-user");
    a.login_credentials = Some(reauthorize::LoginCredentials {
        email: "private@example.com".into(),
        password: "test-password-private".into(),
        two_factor: "PRIVATEBASE32".into(),
    });
    let a = store::upsert(a, None).unwrap();
    let disk = std::fs::read_to_string(
        crate::modules::account::get_data_dir()
            .unwrap()
            .join("chatgpt_accounts.enc"),
    )
    .unwrap();
    assert!(!disk.contains("test-access-private"));
    assert!(!disk.contains("private-user"));
    let view = serde_json::to_string(&AccountView::from(&a)).unwrap();
    assert!(!view.contains("test-access-private"));
    assert!(!view.contains("test-refresh-private"));
    assert!(serde_json::from_str::<serde_json::Value>(&view)
        .unwrap()
        .get("credentials")
        .is_none());
    assert!(!view.contains("test-password-private"));
    assert!(!view.contains("PRIVATEBASE32"));
    assert!(!disk.contains("test-password-private"));
    assert_eq!(
        store::get(Provider::ChatGpt, &a.id)
            .unwrap()
            .credentials
            .refresh_token,
        "test-refresh-private"
    );
}
#[test]
fn reauthorization_and_deletion_reject_stale_refresh_writes() {
    let _sandbox = Sandbox::new();
    let a = store::upsert(account(Provider::ChatGpt, "u"), None).unwrap();
    let mut replacement = account(Provider::ChatGpt, "u");
    replacement.credentials.refresh_token = "new-refresh".into();
    let replacement = store::upsert(replacement, Some(&a.id)).unwrap();
    assert_eq!(replacement.id, a.id);
    assert!(
        store::update(Provider::ChatGpt, &a.id, Some(&a.revision), |a| a
            .credentials
            .refresh_token =
            "stale".into())
        .is_err()
    );
    assert_eq!(
        store::get(Provider::ChatGpt, &a.id)
            .unwrap()
            .credentials
            .refresh_token,
        "new-refresh"
    );
    assert!(store::upsert(account(Provider::ChatGpt, "different-user"), Some(&a.id)).is_err());
    store::remove(Provider::ChatGpt, &a.id).unwrap();
    assert!(store::update(Provider::ChatGpt, &a.id, None, |_| {}).is_err());
}
#[test]
fn scheduler_rotates_limits_concurrency_and_skips_cooling_or_exhausted_accounts() {
    let _sandbox = Sandbox::new();
    let a = store::upsert(account(Provider::ChatGpt, "a"), None).unwrap();
    let b = store::upsert(account(Provider::ChatGpt, "b"), None).unwrap();
    let mut leases: Vec<_> = (0..10)
        .map(|_| accounts::select(Provider::ChatGpt, "shared-model", &HashSet::new()).unwrap())
        .collect();
    assert_eq!(leases[0].account.id, a.id);
    assert_eq!(leases[1].account.id, b.id);
    assert_eq!(
        leases
            .iter()
            .filter(|lease| lease.account.id == a.id)
            .count(),
        5
    );
    assert_eq!(
        leases
            .iter()
            .filter(|lease| lease.account.id == b.id)
            .count(),
        5
    );
    assert!(accounts::select(Provider::ChatGpt, "shared-model", &HashSet::new()).is_err());
    drop(leases.pop());
    let replacement = accounts::select(Provider::ChatGpt, "shared-model", &HashSet::new()).unwrap();
    leases.push(replacement);
    let mut first = leases.remove(0);
    first.failure(429);
    drop(first);
    drop(leases);
    let next = accounts::select(Provider::ChatGpt, "shared-model", &HashSet::new()).unwrap();
    assert_eq!(next.account.id, b.id);
    drop(next);
    store::update(Provider::ChatGpt, &b.id, None, |a| {
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
    assert!(accounts::select(Provider::ChatGpt, "shared-model", &HashSet::new()).is_err());
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
            &["shared-model"],
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
            &["shared-model"],
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
            &["shared-model"],
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
            &["shared-model"],
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
            &["shared-model"],
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
            &["shared-model"],
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
            &["shared-model"],
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
    std::env::set_var("ABV_CHATGPT_MAX_CONCURRENCY", "1");
    store::upsert(account(Provider::ChatGpt, "wait-a"), None).unwrap();
    store::upsert(account(Provider::ChatGpt, "wait-b"), None).unwrap();
    let mut policy = Policy::default();
    policy.sticky.mode = SchedulingMode::CacheFirst;
    policy.sticky.max_wait_seconds = 1;
    let excluded = HashSet::new();
    let deadline = || tokio::time::Instant::now() + Duration::from_secs(3);
    let first = accounts::scheduling::acquire(
        Provider::ChatGpt,
        &["shared-model"],
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
            Provider::ChatGpt,
            &["shared-model"],
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
            Provider::ChatGpt,
            &["shared-model"],
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
        Provider::ChatGpt,
        &["shared-model"],
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
        Provider::ChatGpt,
        &["shared-model"],
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
        Provider::ChatGpt,
        &["shared-model"],
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
        Provider::ChatGpt,
        &["shared-model"],
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
    std::env::set_var("ABV_CHATGPT_MAX_CONCURRENCY", "1");
    let a = store::upsert(account(Provider::ChatGpt, "fixed-a"), None).unwrap();
    let b = store::upsert(account(Provider::ChatGpt, "fixed-b"), None).unwrap();
    store::save_settings(
        Provider::ChatGpt,
        Settings {
            enabled: true,
            preferred_account_id: Some(b.id.clone()),
        },
    )
    .unwrap();
    let policy = Policy::default();
    let fixed = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
        "fixed",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(fixed.account.id, b.id);
    assert!(accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
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
        Provider::ChatGpt,
        "shared-model",
        &HashSet::new(),
        Some(&a.id),
    )
    .unwrap();
    assert_eq!(probe.account.id, a.id);
    drop(probe);
    accounts::configure(Provider::ChatGpt, &b.id, Some(false), None).unwrap();
    assert!(accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
        "fixed",
        &policy,
        &HashSet::new()
    )
    .is_err());
    store::remove(Provider::ChatGpt, &b.id).unwrap();
    assert!(accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
        "fixed",
        &policy,
        &HashSet::new()
    )
    .is_err());
    store::save_settings(
        Provider::ChatGpt,
        Settings {
            enabled: true,
            preferred_account_id: None,
        },
    )
    .unwrap();
    assert_eq!(
        accounts::scheduling::try_acquire(
            Provider::ChatGpt,
            &["shared-model"],
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
    std::env::set_var("ABV_CHATGPT_MAX_CONCURRENCY", "1");
    let a = store::upsert(account(Provider::ChatGpt, "breaker-a"), None).unwrap();
    let manager = crate::proxy::TokenManager::new(crate::modules::account::get_data_dir().unwrap());
    manager
        .update_circuit_breaker_config(crate::models::CircuitBreakerConfig {
            backoff_steps: vec![2, 10],
            ..Default::default()
        })
        .await;
    let policy = Policy::live(&manager, None).await;
    let mut lease = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
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
        Provider::ChatGpt,
        &["shared-model"],
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    lease.failure_after(429, Some(120));
    drop(lease);
    assert!(accounts::runtime(&a.id).cooldown_until >= now() + 119);
    // A second account is selected while `a` cools down; both cooldowns are then cleared.
    let b = store::upsert(account(Provider::ChatGpt, "breaker-b"), None).unwrap();
    let mut lease = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
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
    store::remove(Provider::ChatGpt, &b.id).unwrap();
    let mut lease = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
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
        Provider::ChatGpt,
        &["shared-model"],
        "",
        &off,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    assert!(accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
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
        Provider::ChatGpt,
        &["shared-model"],
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
    std::env::set_var("ABV_CHATGPT_MAX_CONCURRENCY", "1");
    let account = store::upsert(account(Provider::ChatGpt, "probe-user"), None).unwrap();
    let lease = accounts::select(Provider::ChatGpt, "shared-model", &HashSet::new()).unwrap();
    assert!(crate::proxy::upstream::native::probe(
        Provider::ChatGpt,
        "shared-model",
        Some(&account.id),
        "system",
        "user"
    )
    .await
    .is_err());
    drop(lease);
    store::save_settings(
        Provider::ChatGpt,
        Settings {
            enabled: false,
            preferred_account_id: None,
        },
    )
    .unwrap();
    assert!(crate::proxy::upstream::native::probe(
        Provider::ChatGpt,
        "shared-model",
        Some(&account.id),
        "system",
        "user"
    )
    .await
    .is_err());
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
    let _offline = OfflineChatGpt::new();
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
                extra_native_models: Vec::new(),
                extra_targets: Vec::new(),
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
    let mut a = account(Provider::ChatGpt, "native-user");
    a.verified_models.insert("shared-model".into(), now());
    let a = store::upsert(a, None).unwrap();
    let view = registry::view(&[], &mapping).unwrap();
    assert_eq!(
        view.candidates
            .iter()
            .find(|c| c.source_id == Provider::ChatGpt.source())
            .unwrap()
            .verified_account_count,
        1
    );
    store::update(Provider::ChatGpt, &a.id, None, |a| a.models.clear()).unwrap();
    let view = registry::view(&[], &mapping).unwrap();
    assert_eq!(
        view.candidates
            .iter()
            .find(|c| c.source_id == Provider::ChatGpt.source())
            .unwrap()
            .verified_account_count,
        0
    );
    std::fs::write(
        crate::modules::account::get_data_dir()
            .unwrap()
            .join("chatgpt_accounts.enc"),
        "damaged vault",
    )
    .unwrap();
    let view = registry::view(&[], &mapping).unwrap();
    assert!(
        !view
            .sources
            .iter()
            .find(|s| s.id == Provider::ChatGpt.source())
            .unwrap()
            .enabled
    );
    assert!(view
        .notices
        .iter()
        .any(|n| n.contains("ChatGPT") && n.contains("unavailable")));
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

#[test]
fn maintenance_batches_take_the_oldest_due_accounts_first() {
    let at = 1_000_000;
    // 25 accounts that just failed are due again at the next tick, ahead of five
    // accounts later in the vault that have never been maintained.
    let mut vault: Vec<Account> = (0..25)
        .map(|_| {
            let mut a = Account::new(Provider::ChatGpt, Realm::Global);
            a.next_sync_at = at - 1;
            a
        })
        .collect();
    let starved: Vec<Account> = (0..5)
        .map(|_| Account::new(Provider::ChatGpt, Realm::Global))
        .collect();
    vault.extend(starved.iter().cloned());
    let mut later = Account::new(Provider::ChatGpt, Realm::Global);
    later.next_sync_at = at + 1;
    let mut paused = Account::new(Provider::ChatGpt, Realm::Global);
    paused.auto_refresh = false;
    vault.extend([later.clone(), paused.clone()]);
    let batch = accounts::due_for_maintenance(vault, at);
    assert_eq!(batch.len(), 20);
    for a in &starved {
        assert!(batch.iter().any(|b| b.id == a.id));
    }
    assert!(batch.iter().all(|b| b.id != later.id && b.id != paused.id));
}

#[tokio::test]
async fn registry_refresh_skips_an_unreadable_native_vault_and_still_reports() {
    let _sandbox = Sandbox::new();
    std::fs::write(
        crate::modules::account::get_data_dir()
            .unwrap()
            .join("chatgpt_accounts.enc"),
        "damaged vault",
    )
    .unwrap();
    let manager = crate::proxy::TokenManager::new(crate::modules::account::get_data_dir().unwrap());
    let view = crate::modules::model_registry_service::refresh(
        &manager,
        crate::modules::model_registry::RefreshModelCandidatesRequest { source_id: None },
    )
    .await
    .unwrap();
    assert!(view.notices.iter().any(|n| n.contains("目录同步完成")));
    assert!(view
        .notices
        .iter()
        .any(|n| n.contains("ChatGPT") && n.contains("已跳过该来源")));
}

#[test]
fn idle_accounts_only_renew_near_expiry_and_new_catalogs_are_always_fetched() {
    let at = now();
    let mut a = account(Provider::ChatGpt, "idle");
    a.next_sync_at = at - 1;
    // Never fetched: due so the account can become schedulable at all.
    assert_eq!(accounts::work(&a, at), Some(accounts::Work::Sync));
    a.models_updated_at = at - 3600;
    // Fetched before and unused since: no catalog/quota heartbeat.
    assert_eq!(accounts::work(&a, at), None);
    assert!(accounts::sync_paused(&a, at));
    a.expires_at = at + 120;
    assert_eq!(accounts::work(&a, at), Some(accounts::Work::Renew));
    a.credentials.refresh_token.clear();
    assert_eq!(accounts::work(&a, at), None);
    a.auto_refresh = false;
    a.models_updated_at = 0;
    assert_eq!(accounts::work(&a, at), None);
    assert!((900..1080).contains(&accounts::sync_interval(&a.id)));
}

#[test]
fn idle_zero_balance_accounts_keep_recovery_until_credits_return() {
    let at = now();
    let mut a = account(Provider::ChatGpt, "idle-quota-recovery");
    a.models_updated_at = at - 86400;
    a.next_sync_at = at - 1;
    a.quota = Some(exhausted_quota());
    // No in-memory activity, as after a restart. Recovery also survives expiry
    // of the quota's admission TTL; it never becomes a catalog poll.
    for age in [0, 901, 86400] {
        a.quota.as_mut().unwrap().updated_at = at - age;
        assert_eq!(accounts::work(&a, at), Some(accounts::Work::Quota));
        assert!(!accounts::sync_paused(&a, at));
        assert_eq!(AccountView::from(&a).next_sync_at, a.next_sync_at);
    }
    assert!(!a.quota.as_ref().unwrap().exhausted());
    // Existing due time and operator switches still bound recovery work.
    a.next_sync_at = at + accounts::sync_interval(&a.id);
    assert_eq!(accounts::work(&a, at), None);
    assert_eq!(
        accounts::work(&a, a.next_sync_at),
        Some(accounts::Work::Quota)
    );
    a.next_sync_at = at - 1;
    a.enabled = false;
    assert_eq!(accounts::work(&a, at), None);
    a.enabled = true;
    a.auto_refresh = false;
    assert_eq!(accounts::work(&a, at), None);
    a.auto_refresh = true;
    // A successful recovery stops the idle heartbeat again. Unknown and
    // unlimited balances also do not start a zero-balance recovery loop.
    for remaining in [Some(20.0), None] {
        a.quota.as_mut().unwrap().remaining = remaining;
        assert_eq!(accounts::work(&a, at), None);
        assert!(accounts::sync_paused(&a, at));
    }
    a.quota.as_mut().unwrap().remaining = Some(0.0);
    a.quota.as_mut().unwrap().unlimited = true;
    assert_eq!(accounts::work(&a, at), None);
    assert!(accounts::sync_paused(&a, at));
}

#[tokio::test]
async fn duplicate_import_is_rejected_before_egress_and_preserves_saved_tokens() {
    let _sandbox = Sandbox::new();
    let mut a = account(Provider::ChatGpt, "duplicate-import");
    a.expires_at = now() - 3600;
    let a = store::upsert(a, None).unwrap();
    let before = serde_json::to_value(&a).unwrap();
    // Any attempt to obtain a login route fails locally, before networking. The
    // duplicate-credential guard must run even before that route validation.
    std::fs::write(
        crate::modules::account::get_data_dir()
            .unwrap()
            .join("workbuddy_egress.json"),
        "invalid egress settings",
    )
    .unwrap();
    let error = login::import(
        Provider::ChatGpt,
        json!({
            "access_token":"expired-import-access",
            "refresh_token":a.credentials.refresh_token,
            "expires_at":a.expires_at
        }),
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(error.contains("already saved"), "{error}");
    assert_eq!(store::all(Provider::ChatGpt).unwrap().len(), 1);
    assert_eq!(
        serde_json::to_value(store::get(Provider::ChatGpt, &a.id).unwrap()).unwrap(),
        before
    );
}

#[tokio::test]
async fn duplicate_import_rechecks_credentials_saved_while_waiting_for_egress() {
    let _sandbox = Sandbox::new();
    // If the duplicate guard regresses, client creation fails locally rather
    // than letting this test contact a real ChatGPT endpoint.
    let mut config = crate::models::AppConfig::default();
    config.proxy.upstream_proxy.enabled = true;
    config.proxy.upstream_proxy.url = "http://127.0.0.1:invalid".into();
    crate::modules::config::save_app_config(&config).unwrap();
    let route = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        crate::modules::egress::login_route(
            crate::modules::egress::Pool::Native(Provider::ChatGpt),
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let a = account(Provider::ChatGpt, "concurrent-import");
    let import = login::import(
        Provider::ChatGpt,
        json!({"refresh_token":a.credentials.refresh_token}),
        None,
    );
    tokio::pin!(import);
    // The vault is empty at the first guard; suspend at the held login lock.
    assert!(futures::poll!(&mut import).is_pending());
    let a = store::upsert(a, None).unwrap();
    let before = serde_json::to_value(&a).unwrap();
    drop(route);
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), &mut import)
        .await
        .unwrap()
        .err()
        .unwrap();
    assert!(error.contains("already saved"), "{error}");
    assert_eq!(
        serde_json::to_value(store::get(Provider::ChatGpt, &a.id).unwrap()).unwrap(),
        before
    );
}

fn model(id: &str, is_free: bool) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        is_free,
        ..Default::default()
    }
}
fn with_models(uid: &str, models: Vec<Model>) -> Account {
    let mut a = account(Provider::ChatGpt, uid);
    a.models = models;
    a
}
fn exhausted_quota() -> Quota {
    Quota::from_packs(
        "credits",
        vec![QuotaPack {
            name: "credits".into(),
            total: Some(100.0),
            remaining: Some(0.0),
            used: Some(100.0),
            unlimited: false,
            expires_at: None,
        }],
    )
}
#[test]
fn native_group_pools_accounts_with_different_native_ids() {
    use crate::proxy::sticky_config::AccountSchedulingPolicy as Policy;
    let _sandbox = Sandbox::new();
    let cn = store::upsert(
        with_models("group-cn", vec![model("shared-model", false)]),
        None,
    )
    .unwrap();
    let global = store::upsert(
        with_models("group-global", vec![model("shared-model-sg", false)]),
        None,
    )
    .unwrap();
    let policy = Policy::default();
    let targets = ["shared-model", "shared-model-sg"];
    let first = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &targets,
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    let second = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &targets,
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    let mut picked: Vec<_> = [&first, &second]
        .iter()
        .map(|l| (l.account.id.clone(), l.model.clone()))
        .collect();
    picked.sort();
    let mut expected = vec![
        (cn.id.clone(), "shared-model".to_string()),
        (global.id.clone(), "shared-model-sg".to_string()),
    ];
    expected.sort();
    assert_eq!(picked, expected);
}
#[test]
fn native_group_quota_filter_applies_per_target() {
    use crate::proxy::sticky_config::AccountSchedulingPolicy as Policy;
    let _sandbox = Sandbox::new();
    let policy = Policy::default();
    let targets = ["paid-model", "free-model-sg"];
    let mut paid_only = with_models("quota-paid", vec![model("paid-model", false)]);
    paid_only.quota = Some(exhausted_quota());
    let stored = store::upsert(paid_only, None).unwrap();
    assert!(accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &targets,
        "",
        &policy,
        &HashSet::new()
    )
    .is_err());
    store::update(Provider::ChatGpt, &stored.id, None, |a| {
        a.models = vec![model("paid-model", false), model("free-model-sg", true)];
    })
    .unwrap();
    let lease = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &targets,
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(lease.model, "free-model-sg");
    store::update(Provider::ChatGpt, &stored.id, None, |a| {
        a.models = vec![model("paid-model", true)];
    })
    .unwrap();
    drop(lease);
    let lease = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &targets,
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(lease.model, "paid-model");
}
#[test]
fn native_group_prefers_first_target_on_accounts_listing_both() {
    use crate::proxy::sticky_config::AccountSchedulingPolicy as Policy;
    let _sandbox = Sandbox::new();
    store::upsert(
        with_models(
            "both",
            vec![
                model("shared-model-sg", false),
                model("shared-model", false),
            ],
        ),
        None,
    )
    .unwrap();
    let lease = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model", "shared-model-sg"],
        "",
        &Policy::default(),
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(lease.model, "shared-model");
}
#[test]
fn publication_targets_lease_the_only_enabled_accounts_native_id() {
    use crate::modules::model_registry::PublishedModel;
    use crate::proxy::sticky_config::AccountSchedulingPolicy as Policy;
    let _sandbox = Sandbox::new();
    let mut cn = with_models("only-cn", vec![model("shared-model", false)]);
    cn.enabled = false;
    store::upsert(cn, None).unwrap();
    let global = store::upsert(
        with_models("only-global", vec![model("shared-model-sg", false)]),
        None,
    )
    .unwrap();
    let published = PublishedModel {
        id: "p".into(),
        public_id: "public-model".into(),
        display_name: "Public".into(),
        source_id: Provider::ChatGpt.source().into(),
        native_model: "shared-model".into(),
        extra_native_models: vec!["shared-model-sg".into()],
        extra_targets: Vec::new(),
        enabled: true,
        legacy: false,
    };
    let lease = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &published.native_models(),
        "",
        &Policy::default(),
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(lease.account.id, global.id);
    assert_eq!(lease.model, "shared-model-sg");
}

async fn settle_lease(uid: &str, breaker_enabled: bool) -> (Account, accounts::Lease) {
    use crate::proxy::sticky_config::AccountSchedulingPolicy as Policy;
    let a = store::upsert(account(Provider::ChatGpt, uid), None).unwrap();
    let manager = crate::proxy::TokenManager::new(crate::modules::account::get_data_dir().unwrap());
    manager
        .update_circuit_breaker_config(crate::models::CircuitBreakerConfig {
            enabled: breaker_enabled,
            ..Default::default()
        })
        .await;
    let policy = Policy::live(&manager, None).await;
    let lease = accounts::scheduling::try_acquire(
        Provider::ChatGpt,
        &["shared-model"],
        "",
        &policy,
        &HashSet::new(),
    )
    .unwrap()
    .unwrap();
    (a, lease)
}

#[tokio::test]
async fn lease_settle_until_blocks_even_with_breaker_disabled() {
    let _sandbox = Sandbox::new();
    let (a, mut lease) = settle_lease("settle-until", false).await;
    lease.settle(&http::Failure::classified(
        429,
        "q",
        http::Penalty::Until {
            at: now() + 3600,
            message: "Account quota exhausted",
        },
        true,
    ));
    drop(lease);
    let r = accounts::runtime(&a.id);
    assert!(r.cooldown_until >= now() + 3599);
    assert_eq!(r.last_error.as_deref(), Some("Account quota exhausted"));
}

#[tokio::test]
async fn lease_settle_neutral_leaves_counters_untouched() {
    let _sandbox = Sandbox::new();
    let (a, mut lease) = settle_lease("settle-neutral", true).await;
    lease.settle(&http::Failure::classified(
        400,
        "q",
        http::Penalty::Neutral,
        false,
    ));
    drop(lease);
    let r = accounts::runtime(&a.id);
    assert_eq!(r.failures, 0);
    assert_eq!(r.cooldown_until, 0);
}

#[tokio::test]
async fn lease_settle_disable_turns_account_off() {
    let _sandbox = Sandbox::new();
    let (a, mut lease) = settle_lease("settle-disable", true).await;
    lease.settle(&http::Failure::classified(
        401,
        "q",
        http::Penalty::Disable {
            message: "dead".into(),
        },
        true,
    ));
    drop(lease);
    let stored = store::get(Provider::ChatGpt, &a.id).unwrap();
    assert!(!stored.enabled);
    assert_eq!(stored.auth_error.as_deref(), Some("dead"));
    assert_eq!(accounts::runtime(&a.id).failures, 1);
}

#[tokio::test]
async fn lease_settle_disable_skips_a_reimported_account() {
    let _sandbox = Sandbox::new();
    let (a, mut lease) = settle_lease("settle-disable-stale", true).await;
    // The account is re-imported while the request is in flight.
    let reimported =
        store::upsert(account(Provider::ChatGpt, "settle-disable-stale"), None).unwrap();
    assert_eq!(reimported.id, a.id);
    assert_ne!(reimported.revision, lease.account.revision);
    lease.settle(&http::Failure::classified(
        401,
        "q",
        http::Penalty::Disable {
            message: "dead".into(),
        },
        true,
    ));
    drop(lease);
    let stored = store::get(Provider::ChatGpt, &a.id).unwrap();
    assert!(stored.enabled);
    assert!(!stored.credential_dead);
    assert_eq!(stored.auth_error, None);
}

#[tokio::test]
async fn lease_settle_without_penalty_matches_failure_after() {
    let _sandbox = Sandbox::new();
    let (a, mut lease) = settle_lease("settle-plain-a", true).await;
    lease.settle(&http::Failure::new(429, "x"));
    drop(lease);
    store::remove(Provider::ChatGpt, &a.id).unwrap();
    let (b, mut lease) = settle_lease("settle-plain-b", true).await;
    lease.failure_after(429, None);
    drop(lease);
    let (ra, rb) = (accounts::runtime(&a.id), accounts::runtime(&b.id));
    assert_eq!(ra.failures, rb.failures);
    assert_eq!(ra.last_error, rb.last_error);
    assert!((ra.cooldown_until - rb.cooldown_until).abs() <= 1);
    assert_eq!(ra.failures, 1);
}

#[test]
fn clients_are_cached_per_account_and_route() {
    use crate::modules::egress::Exit;
    let a = account(Provider::ChatGpt, "plain-cache");
    assert!(!http::cached_for(&a.id));
    http::client_on(&a, &Exit::Direct).unwrap();
    assert!(http::cached_for(&a.id));
    http::client_on(&a, &Exit::Direct).unwrap();
    http::forget(&a.id);
    assert!(!http::cached_for(&a.id));
}

#[path = "chatgpt_dispatch_tests.rs"]
mod chatgpt_dispatch;
