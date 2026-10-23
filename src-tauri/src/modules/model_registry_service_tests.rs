use super::*;
use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

struct Mock {
    task: tokio::task::JoinHandle<()>,
    previous: Option<std::ffi::OsString>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(previous) = &self.previous {
            std::env::set_var("STELLAR_TEST_REGISTRY_AG_BASE", previous);
        } else {
            std::env::remove_var("STELLAR_TEST_REGISTRY_AG_BASE");
        }
    }
}

#[tokio::test]
async fn model_registry_selected_probe_is_single_attempt_scoped_and_never_publishes() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let count = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(AtomicBool::new(false));
    let calls = count.clone();
    let mode = fail.clone();
    let app = Router::new().route("/v1internal:generateContent", post(move |Json(body): Json<Value>| {
        let calls = calls.clone();
        let mode = mode.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(body["model"], "NativeCase");
            assert!(body["request"]["generationConfig"].get("thinkingConfig").is_none());
            if mode.load(Ordering::SeqCst) {
                (StatusCode::NOT_FOUND, Json(json!({"error":{"message":"endpoint unavailable"}}))).into_response()
            } else {
                Json(json!({"response":{"candidates":[{"content":{"parts":[{"text":"OK"}]},"finishReason":"STOP"}]}})).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let previous = std::env::var_os("STELLAR_TEST_REGISTRY_AG_BASE");
    std::env::set_var(
        "STELLAR_TEST_REGISTRY_AG_BASE",
        format!("http://{}/v1internal", listener.local_addr().unwrap()),
    );
    let _mock = Mock {
        task: tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
        previous,
    };
    let token = crate::models::TokenData::new(
        "mock-access".into(),
        "mock-refresh".into(),
        7200,
        Some("mock@example.test".into()),
        Some("mock-project".into()),
        None,
        false,
        None,
    );
    let mut account =
        crate::modules::account::add_account("mock@example.test".into(), None, token).unwrap();
    let mut quota = crate::models::QuotaData::new();
    quota.models.push(
        serde_json::from_value(json!({"name":"NativeCase", "percentage":100, "reset_time":""}))
            .unwrap(),
    );
    account.quota = Some(quota);
    crate::modules::account::save_account(&account).unwrap();
    // Initialize before discovery is read through the legacy directory service.
    model_registry::view(&[], &Default::default()).unwrap();
    let manager = TokenManager::new(crate::modules::account::get_data_dir().unwrap());
    load_local_accounts(&manager).await.unwrap();
    let upstream = UpstreamClient::new(None, None);
    let id = model_registry::candidate_id(model_registry::ANTIGRAVITY_SOURCE, "NativeCase");
    let request = |confirm| VerifyModelCandidatesRequest {
        candidate_ids: vec![id.clone()],
        confirm_upstream_usage: confirm,
        account_id: None,
    };
    assert!(verify(&manager, &upstream, request(false)).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let report = verify(&manager, &upstream, request(true)).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(report.results[0].status, "verified");
    assert_eq!(
        report.results[0].account_id.as_deref(),
        Some(account.id.as_str())
    );
    assert!(report.registry.published_models.is_empty());
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("mock-access"));
    let candidate = report
        .registry
        .candidates
        .iter()
        .find(|c| c.id == id)
        .unwrap();
    assert_eq!(candidate.verified_account_count, 1);
    assert!(candidate
        .capabilities
        .iter()
        .filter(|c| ["reasoning", "vision", "tools"].contains(&c.key.as_str()))
        .all(|c| c.status == "unverified"));
    fail.store(true, Ordering::SeqCst);
    let report = verify(&manager, &upstream, request(true)).await.unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "no fallback or retry for an admin probe"
    );
    assert_eq!(
        report.results[0].status, "failed",
        "404 alone does not prove a model unavailable"
    );
    assert!(report.registry.published_models.is_empty());
}

#[tokio::test]
async fn model_registry_first_run_without_ag_accounts_is_supported() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let manager = TokenManager::new(crate::modules::account::get_data_dir().unwrap());
    assert_eq!(load_local_accounts(&manager).await.unwrap(), 0);
    assert!(desktop_view().await.is_ok());
}
