use super::super::accounts;
use super::*;
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

struct Mock(tokio::task::JoinHandle<()>);
impl Drop for Mock {
    fn drop(&mut self) {
        self.0.abort();
        std::env::remove_var("STELLAR_TEST_WORKBUDDY_BASE");
    }
}
async fn serve(app: Router) -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STELLAR_TEST_WORKBUDDY_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    Mock(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }))
}
fn account(uid: &str) -> accounts::Account {
    accounts::parse_import(
        json!({"uid":uid,"accessToken":"mock-secret","expiresAt":accounts::now()+7200}),
    )
    .unwrap()
}
#[tokio::test]
async fn workbuddy_verification_does_not_send_a_generated_prompt() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let _mock = serve(Router::new().route(
        "/v2/chat/completions",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                "should not be called"
            }
        }),
    ))
    .await;
    assert!(PROBE_DISABLED.contains("不再向上游发送生成的验证短句"));
    assert!(start().is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn workbuddy_verification_rechecks_credentials_after_waiting_for_capacity() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mut seeded = account("waiting");
    seeded.models.push(accounts::Model {
        id: "native".into(),
        name: "Native".into(),
    });
    let id = accounts::upsert(seeded).unwrap().id;
    let original = accounts::get(&id).unwrap();
    let first = accounts::acquire("native").unwrap();
    let second = accounts::acquire("native").unwrap();
    let reauthorize = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        accounts::upsert(account("waiting")).unwrap();
        drop(first);
        drop(second);
    };
    let (result, _) = tokio::join!(
        accounts::scheduling::acquire_probe(&original, "native"),
        reauthorize,
    );
    assert!(result.is_err());
    assert_eq!(accounts::list().unwrap()[0].in_flight, 0);
    assert!(accounts::acquire("native").is_ok());
}

#[tokio::test]
async fn workbuddy_verification_scopes_grants_and_maintenance_cannot_restore_denied_models() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let _mock = serve(
        Router::new()
            .route(
                "/console/enterprises/personal/models",
                get(|| async {
                    Json(json!({"code":0,"data":{"models":[{"id":"shared"},{"id":"denied"}]}}))
                }),
            )
            .route(
                "/v3/config",
                get(|| async { Json(json!({"code":0,"data":{"models":[]}})) }),
            )
            .route(
                "/v2/chat/completions",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        StatusCode::BAD_REQUEST.into_response()
                    }
                }),
            ),
    )
    .await;
    let a = accounts::upsert(account("a")).unwrap().id;
    let b = accounts::upsert(account("b")).unwrap().id;
    // Initialize before new metadata/settings arrive: verification can never publish.
    crate::modules::model_registry::view(&[], &std::collections::HashMap::new()).unwrap();
    // Existing installations may already advertise a now-rejected native ID.
    super::super::save(
        serde_json::from_value(json!({
            "enabled":true,"models":{"shared":"shared","denied":"denied"}
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(start().unwrap_err().contains("Select 1–5"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    accounts::sync(&a, false).await.unwrap();
    accounts::sync(&b, false).await.unwrap();
    let select = |id: &str| {
        super::super::save(serde_json::from_value(json!({"preferred_account_id": id})).unwrap())
            .unwrap();
    };
    select(&a);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let current = accounts::get(&a).unwrap();
    accounts::record_model_verification(&current, "denied", false).unwrap();
    let current = accounts::get(&a).unwrap();
    accounts::record_model_verification(&current, "shared", true).unwrap();
    assert!(accounts::get(&a).unwrap().supports_model("shared"));
    assert!(!accounts::get(&a).unwrap().supports_model("denied"));
    for _ in 0..2 {
        accounts::sync(&a, false).await.unwrap();
        accounts::sync(&b, false).await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!accounts::get(&a).unwrap().supports_model("denied"));
    assert!(accounts::get(&b).unwrap().supports_model("shared"));
    let registry =
        crate::modules::model_registry::view(&[], &std::collections::HashMap::new()).unwrap();
    assert!(registry.published_models.is_empty());
    assert!(accounts::list()
        .unwrap()
        .iter()
        .all(|a| a.health.successes == 0 && a.health.failures == 0 && a.in_flight == 0));
}

#[test]
fn workbuddy_verification_preserves_legacy_calls_reauthorization_and_rejects_stale_saves() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mut a = account("a");
    a.models.push(accounts::Model {
        id: "native".into(),
        name: "Native".into(),
    });
    // Simulate an encrypted account written by the previous version.
    let mut legacy = serde_json::to_value(a).unwrap();
    legacy.as_object_mut().unwrap().remove("verified_models");
    legacy.as_object_mut().unwrap().remove("unavailable_models");
    legacy.as_object_mut().unwrap().remove("models_verified_at");
    let view = accounts::upsert(serde_json::from_value(legacy).unwrap()).unwrap();
    assert_eq!(view.candidate_model_count, 1);
    assert_eq!(view.models.len(), 1);
    assert!(accounts::acquire("native").is_ok());
    assert_eq!(view.models_verified_at, 0);
    let mut observed = accounts::get(&view.id).unwrap();
    accounts::save_verified_models(&observed, &observed.models, &[]).unwrap();
    // An already running sync cannot erase verification committed since it started.
    accounts::save_observation(&mut observed).unwrap();
    assert!(accounts::get(&view.id).unwrap().supports_model("native"));
    // Removal from the candidate catalog revokes that grant, even if the ID later returns.
    let models = observed.models.clone();
    observed.models.clear();
    accounts::save_observation(&mut observed).unwrap();
    observed.models = models;
    accounts::save_observation(&mut observed).unwrap();
    assert!(accounts::get(&view.id).unwrap().verified_models.is_empty());
    assert!(accounts::get(&view.id).unwrap().supports_model("native"));
    // An inconclusive probe can hide an unverified result without blocking calls.
    accounts::save_verified_models(&observed, &[], &[]).unwrap();
    assert!(accounts::acquire("native").is_ok());
    accounts::save_verified_models(&observed, &[], &["native".into()]).unwrap();
    assert!(accounts::acquire("native").is_err());
    let models = observed.models.clone();
    observed.models.clear();
    accounts::save_observation(&mut observed).unwrap();
    observed.models = models;
    accounts::save_observation(&mut observed).unwrap();
    assert!(accounts::acquire("native").is_err());
    accounts::save_verified_models(&observed, &observed.models, &[]).unwrap();
    accounts::upsert(account("a")).unwrap();
    assert_eq!(accounts::get(&view.id).unwrap().available_models().len(), 1);
    assert!(accounts::acquire("native").is_ok());
    assert!(accounts::save_verified_models(&observed, &observed.models, &[]).is_err());
    let current = accounts::get(&view.id).unwrap();
    accounts::set_enabled(&view.id, false).unwrap();
    assert!(accounts::save_verified_models(&current, &current.models, &[]).is_err());
    accounts::remove(&view.id).unwrap();
    assert!(accounts::save_verified_models(&current, &current.models, &[]).is_err());
}

#[test]
fn model_registry_withdrawn_wb_catalog_does_not_reactivate_historical_probe() {
    use crate::modules::model_registry as registry;
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mut seeded = account("catalog-withdrawal");
    seeded.models.push(accounts::Model {
        id: "native".into(),
        name: "Native".into(),
    });
    let id = accounts::upsert(seeded).unwrap().id;
    let candidate_id = registry::candidate_id(registry::WORKBUDDY_SOURCE, "native");
    registry::view(&[], &Default::default()).unwrap();
    let mut current = accounts::get(&id).unwrap();
    accounts::record_model_verification(&current, "native", true).unwrap();
    registry::record_verification(registry::ModelVerificationResult {
        candidate_id: candidate_id.clone(),
        status: "verified".into(),
        message: String::new(),
        checked_at: accounts::now(),
        account_id: Some(id.clone()),
    })
    .unwrap();
    let before = registry::view(&[], &Default::default()).unwrap();
    assert_eq!(
        before
            .candidates
            .iter()
            .find(|c| c.id == candidate_id)
            .unwrap()
            .verified_account_count,
        1
    );
    current.models.clear();
    accounts::save_observation(&mut current).unwrap();
    assert!(accounts::get(&id).unwrap().verified_models.is_empty());
    let after = registry::view(&[], &Default::default()).unwrap();
    let candidate = after
        .candidates
        .iter()
        .find(|c| c.id == candidate_id)
        .unwrap();
    assert_eq!(candidate.verified_account_count, 0);
    assert_eq!(candidate.account_count, 0);
    assert_eq!(candidate.verification_status, "unverified");
    assert!(after.published_models.is_empty());
}
