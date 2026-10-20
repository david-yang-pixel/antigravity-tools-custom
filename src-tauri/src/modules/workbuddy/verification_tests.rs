use super::*;
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

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
fn stream(content: &str) -> Response {
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(content.to_owned()))
        .unwrap()
}
const OK: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

#[tokio::test]
async fn workbuddy_verification_requires_completed_text_not_just_http_200() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let _mock = serve(Router::new().route("/v2/chat/completions", post(|Json(body): Json<Value>| async move {
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["max_tokens"], 512);
        match body["model"].as_str().unwrap() {
            "ok" => stream(OK),
            "denied" => (StatusCode::BAD_REQUEST, Json(json!({"code":11158}))).into_response(),
            "missing" => (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"11-102"}}))).into_response(),
            "denied-stream" => stream("data: {\"error\":{\"code\":\"11-158\"}}\n\n"),
            "error" => stream("data: {\"error\":{\"message\":\"not allowed\"}}\n\n"),
            "reasoning" => stream("data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":\"length\"}]}\n\n"),
            "truncated" => stream("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"),
            "busy" => StatusCode::TOO_MANY_REQUESTS.into_response(),
            _ => Json(json!({"message":"not an inference result"})).into_response(),
        }
    }))).await;
    for (model, expected) in [
        ("ok", Outcome::Available),
        ("denied", Outcome::Unavailable),
        ("missing", Outcome::Unavailable),
        ("denied-stream", Outcome::Unavailable),
        ("error", Outcome::Failed),
        ("reasoning", Outcome::Failed),
        ("truncated", Outcome::Failed),
        ("busy", Outcome::Failed),
        ("json", Outcome::Failed),
    ] {
        assert_eq!(probe(&account("test"), model).await, expected, "{model}");
    }
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
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if headers["x-user-id"] == "a" && body["model"] == "shared" {
                            stream(OK)
                        } else {
                            (StatusCode::BAD_REQUEST, Json(json!({"code":11158}))).into_response()
                        }
                    }
                }),
            ),
    )
    .await;
    let a = accounts::upsert(account("a")).unwrap().id;
    let b = accounts::upsert(account("b")).unwrap().id;
    // Existing installations may already advertise a now-rejected native ID.
    super::super::save(
        serde_json::from_value(json!({
            "enabled":true,"models":{"shared":"shared","denied":"denied"}
        }))
        .unwrap(),
    )
    .unwrap();
    let first = start().unwrap();
    assert!(first.running);
    assert_eq!(start().unwrap().started_at, first.started_at);
    tokio::time::timeout(Duration::from_secs(5), async {
        while status().unwrap().running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let result = status().unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(
        (
            result.total,
            result.completed,
            result.available,
            result.unavailable
        ),
        (4, 4, 1, 3)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        accounts::get(&a).unwrap().available_models()[0].id,
        "shared"
    );
    assert!(accounts::get(&b).unwrap().available_models().is_empty());
    for _ in 0..4 {
        let lease = accounts::acquire("shared").unwrap();
        assert_eq!(lease.account.id, a);
    }
    assert!(accounts::acquire("denied").is_err());
    let observed = accounts::list().unwrap();
    assert!(observed
        .iter()
        .all(|a| a.health.successes == 0 && a.health.failures == 0 && a.in_flight == 0));
    let mapping = HashMap::from([
        ("blocked-alias".into(), "denied".into()),
        ("blocked-*".into(), "denied".into()),
        ("allowed-alias".into(), "shared".into()),
        ("other-provider".into(), "unrelated".into()),
    ]);
    let mut catalog = vec![
        "denied".into(),
        "blocked-alias".into(),
        "blocked-wildcard".into(),
    ];
    super::super::extend_catalog(&mut catalog, &mapping);
    assert_eq!(catalog, ["shared"]);
    let catalog = crate::proxy::common::model_mapping::get_all_dynamic_models(
        &tokio::sync::RwLock::new(mapping.clone()),
        None,
        false,
    )
    .await;
    assert!(catalog.contains(&"allowed-alias".into()));
    assert!(catalog.contains(&"other-provider".into()));
    assert!(!catalog
        .iter()
        .any(|id| id == "denied" || id.starts_with("blocked-")));
    // A fresh catalog is not a new entitlement, and maintenance never generates tokens.
    accounts::sync(&a, false).await.unwrap();
    accounts::sync(&b, false).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(!accounts::get(&a).unwrap().supports_model("denied"));
    assert!(accounts::get(&b).unwrap().available_models().is_empty());
    let settings =
        super::super::save(serde_json::from_value(json!({"preferred_account_id":b})).unwrap())
            .unwrap();
    assert!(settings.available_models.is_empty());
    let mut catalog = vec!["shared".into(), "other-provider".into()];
    super::super::extend_catalog(&mut catalog, &mapping);
    assert_eq!(catalog, ["other-provider"]);
    assert!(start().unwrap().running);
    tokio::time::timeout(Duration::from_secs(5), async {
        while status().unwrap().running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status().unwrap().total, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert!(accounts::get(&a).unwrap().supports_model("shared"));
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
