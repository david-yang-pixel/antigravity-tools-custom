use super::*;

fn snapshot() -> Value {
    json!({
        "id":"one", "email":"one@example.test", "created_at":1,
        "token":{"refresh_token":"credential", "project_id":"project"},
        "quota":{"models":[{"name":"working"},{"name":"candidate"},{"name":"denied"}]}
    })
}

fn verify(snapshot: &mut Value, model: &str, outcome: Outcome, success: Option<i64>) {
    let mut evidence = snapshot
        .get("model_verification")
        .and_then(|value| serde_json::from_value::<Evidence>(value.clone()).ok())
        .unwrap_or_default();
    evidence.identity = identity(snapshot);
    evidence.models.insert(
        model.into(),
        Observation {
            outcome,
            checked_at: 10,
            last_success_at: success,
        },
    );
    snapshot["model_verification"] = serde_json::to_value(evidence).unwrap();
}

#[test]
fn model_verification_catalog_requires_account_specific_success() {
    let mut account = snapshot();
    assert!(verified_model_ids(&account).is_empty());
    verify(&mut account, "working", Outcome::Verified, Some(10));
    verify(&mut account, "candidate", Outcome::Inconclusive, None);
    verify(&mut account, "denied", Outcome::Unsupported, None);
    assert_eq!(
        verified_model_ids(&account),
        HashSet::from(["working".into()])
    );
    verify(&mut account, "working", Outcome::Inconclusive, Some(10));
    assert!(verified_model_ids(&account).contains("working"));
    account["token"]["access_token"] = json!("refreshed");
    assert!(verified_model_ids(&account).contains("working"));
    let mut other = account.clone();
    other["id"] = json!("another-account");
    assert!(verified_model_ids(&other).is_empty());
    account["token"]["refresh_token"] = json!("reimported");
    assert!(verified_model_ids(&account).is_empty());
}

#[test]
fn model_verification_does_not_confuse_temporary_or_request_failures_with_unsupported() {
    for (status, message) in [
        (429, "Quota exhausted"),
        (500, "Model not found"),
        (400, "Penalty is not enabled for this model"),
        (400, "User location is not supported for the API use."),
        (401, "Authentication failed"),
        (403, "Permission denied"),
    ] {
        assert_eq!(
            classify_error(status, &json!({"error":{"message":message}})),
            Outcome::Inconclusive
        );
    }
    assert_eq!(
        classify_error(404, &json!({"error":{"message":"Model not found"}})),
        Outcome::Unsupported
    );
    assert_eq!(
        classify_error(
            403,
            &json!({"error":{"details":[{"reason":"MODEL_ACCESS_DENIED"}]}})
        ),
        Outcome::Unsupported
    );
}

#[test]
fn model_verification_requires_content_and_preserves_exact_probe_id() {
    assert!(!has_output(&json!({"usageMetadata": {}})));
    assert!(!has_output(
        &json!({"candidates":[{"content":{"parts":[{"thought":true,"text":"thinking"}]}}]})
    ));
    assert!(has_output(
        &json!({"response":{"candidates":[{"content":{"parts":[{"text":"OK"}]}}]}})
    ));
    for model in [
        "gemini-2.5-flash",
        "gemini-2.5-pro",
        "claude-sonnet-4-6",
        "claude-opus-4-6-thinking",
        "gemini-3.8-flash-high",
        "Future-Model",
    ] {
        let mut body = request_body(model, "project", "account");
        crate::proxy::pipeline::InboundThinkingPipeline::align_official_envelope(&mut body);
        assert_eq!(body["model"], model);
        let config = &body["request"]["generationConfig"];
        let max = config["maxOutputTokens"].as_u64().unwrap();
        assert!(max <= 2048);
        assert!(
            max > config["thinkingConfig"]["thinkingBudget"]
                .as_u64()
                .unwrap_or(0)
        );
    }
}

#[test]
fn model_verification_removes_models_missing_from_catalog() {
    let mut account = snapshot();
    verify(&mut account, "working", Outcome::Verified, Some(10));
    account["quota"]["models"] = json!([]);
    assert!(verified_model_ids(&account).is_empty());
}

#[test]
fn model_verification_persists_without_stale_refresh_restoring_revoked_access() {
    let _data = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mut account = Account::new(
        "verification".into(),
        "test@example.test".into(),
        crate::models::TokenData::new(
            "access".into(),
            "refresh".into(),
            3600,
            None,
            Some("project".into()),
            None,
            false,
            None,
        ),
    );
    let mut quota = crate::models::QuotaData::new();
    quota.models.push(
        serde_json::from_value(json!({"name":"working","percentage":100,"reset_time":""})).unwrap(),
    );
    account.quota = Some(quota.clone());
    super::super::account::save_account(&account).unwrap();
    record(&account, "working", Outcome::Verified).unwrap();
    let stale = super::super::account::load_account(&account.id).unwrap();
    record(&account, "working", Outcome::Inconclusive).unwrap();
    let load =
        || serde_json::to_value(super::super::account::load_account(&account.id).unwrap()).unwrap();
    assert!(verified_model_ids(&load()).contains("working"));
    record(&account, "working", Outcome::Unsupported).unwrap();
    super::super::account::save_account(&stale).unwrap();
    super::super::account::update_account_quota(&account.id, quota).unwrap();
    assert!(verified_model_ids(&load()).is_empty());
    record(&account, "working", Outcome::Verified).unwrap();
    assert!(verified_model_ids(&load()).contains("working"));
    let mut changed = super::super::account::load_account(&account.id).unwrap();
    changed.token.project_id = Some("different-project".into());
    super::super::account::save_account(&changed).unwrap();
    assert!(verified_model_ids(&load()).is_empty());
    assert!(record(&account, "working", Outcome::Verified).is_err());
    super::super::account::save_account(&stale).unwrap();
    assert!(
        verified_model_ids(&load()).is_empty(),
        "A stale identity must not restore its old verification"
    );
    super::super::account::save_account(&stale).unwrap();
    assert!(
        verified_model_ids(&load()).is_empty(),
        "Repeated stale saves cannot restore old proof"
    );
}

#[tokio::test]
async fn model_verification_checks_real_http_bodies_and_stream_completion() {
    use axum::{http::StatusCode, routing::get, Router};
    for (status, content_type, body, expected) in [
        (200, "application/json", r#"{"response":{"candidates":[{"content":{"parts":[{"text":"OK"}]}}]}}"#, Outcome::Verified),
        (200, "application/json", r#"{"usageMetadata":{"totalTokenCount":5}}"#, Outcome::Inconclusive),
        (400, "application/json", r#"{"error":{"message":"Penalty is not enabled for this model"}}"#, Outcome::Inconclusive),
        (404, "application/json", r#"{"error":{"message":"Model not found"}}"#, Outcome::Unsupported),
        (200, "text/event-stream", "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"OK\"}]},\"finishReason\":\"STOP\"}]}}\n\n", Outcome::Verified),
        (200, "text/event-stream", "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}}\n\n", Outcome::Inconclusive),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/", get(move || async move {
            (StatusCode::from_u16(status).unwrap(), [("content-type", content_type)], body)
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let response = rquest::Client::builder().no_proxy().build().unwrap()
            .get(format!("http://{address}/")).send().await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), read_response(response)).await.unwrap();
        server.abort();
        assert_eq!(result, expected, "{body}");
    }
}
