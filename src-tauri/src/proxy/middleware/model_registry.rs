//! Public inference admission. Runs after authentication, before any billing
//! permit/monitor/provider routing. No client-controlled header bypass exists.
use crate::modules::model_registry::{self, PublishedModel, ANTIGRAVITY_SOURCE, WORKBUDDY_SOURCE};
use axum::{
    extract::Request,
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

pub(crate) fn is_inference_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/chat/completions"
            | "/v1/completions"
            | "/v1/responses"
            | "/responses"
            | "/responses/compact"
            | "/v1/messages"
            | "/v1/messages/count_tokens"
            | "/v1/images/generations"
            | "/v1/images/edits"
            | "/v1/audio/transcriptions"
    ) || path.starts_with("/v1beta/models/")
}

/// Shared by HTTP admission and every normalized WebSocket frame. Errors are
/// intentionally generic: registry contents and filesystem paths are admin data.
pub(crate) fn resolve_public_model(
    public_id: &str,
) -> Result<PublishedModel, (StatusCode, String)> {
    if public_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Provide one published model ID.".into(),
        ));
    }
    let published = model_registry::resolve_existing(public_id)
        .map_err(|error| {
            tracing::error!(%error, "Model registry lookup failed; inference denied");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Model registry unavailable.".into(),
            )
        })?
        .filter(|model| model.enabled)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "Model is not published or is disabled.".into(),
            )
        })?;
    if !matches!(
        published.source_id.as_str(),
        ANTIGRAVITY_SOURCE | WORKBUDDY_SOURCE
    ) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Model source is not integrated.".into(),
        ));
    }
    Ok(published)
}

/// Handler-side boundary for paths that only implement the AG protocol. This is
/// also used by direct (non-Router) WebSocket dispatch; never infer source by name.
pub(crate) fn antigravity_model(public_id: &str) -> Result<PublishedModel, (StatusCode, String)> {
    let published = resolve_public_model(public_id)?;
    if published.source_id != ANTIGRAVITY_SOURCE {
        return Err((
            StatusCode::BAD_REQUEST,
            "This model requires a supported WorkBuddy HTTP endpoint.".into(),
        ));
    }
    Ok(published)
}

fn rejection(status: StatusCode, message: String) -> Response {
    let code = if status == StatusCode::SERVICE_UNAVAILABLE {
        "model_registry_unavailable"
    } else if status == StatusCode::BAD_REQUEST {
        "model_required"
    } else {
        "model_not_published"
    };
    (
        status,
        Json(serde_json::json!({"error": {"type": code, "code": code, "message": message}})),
    )
        .into_response()
}

pub async fn model_registry_middleware(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    // The Responses handshake has no billable model. User keys remain blocked
    // by their accounting middleware; master-key frames are checked individually.
    if request.method() == Method::GET
        && path == "/v1/responses"
        && request.headers().contains_key(header::UPGRADE)
    {
        return next.run(request).await;
    }
    if request.method() != Method::POST || !is_inference_path(path) {
        return next.run(request).await;
    }
    let (mut request, model) = match super::user_token_models::requested_model(request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let published = match resolve_public_model(model.as_deref().unwrap_or("")) {
        Ok(model) => model,
        Err((status, message)) => return rejection(status, message),
    };
    // An in-process extension, not a wire header. Billing retains the original
    // memoized public ID; the provider receives this explicit source/native pair.
    request.extensions_mut().insert(published);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        routing::post,
        Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tower::ServiceExt;

    async fn billing_boundary(request: Request, next: Next) -> Response {
        assert!(request.extensions().get::<PublishedModel>().is_some());
        request
            .extensions()
            .get::<Arc<AtomicUsize>>()
            .unwrap()
            .fetch_add(1, Ordering::SeqCst);
        next.run(request).await
    }

    #[tokio::test]
    async fn model_registry_admission_denies_before_inner_billing_for_all_keys() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        model_registry::view(&["published".into()], &Default::default()).unwrap();
        let app = Router::new()
            .route("/v1/chat/completions", post(|| async { "admitted" }))
            .layer(axum::middleware::from_fn(billing_boundary))
            .layer(axum::middleware::from_fn(model_registry_middleware));
        for user in [false, true] {
            for (model, status) in [
                ("published", StatusCode::OK),
                ("not-published", StatusCode::NOT_FOUND),
            ] {
                let mut request = Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .body(Body::from(serde_json::json!({"model":model}).to_string()))
                    .unwrap();
                if user {
                    request
                        .extensions_mut()
                        .insert(super::super::auth::UserTokenIdentity {
                            token_id: "test-user".into(),
                            token: "secret".into(),
                            username: "test".into(),
                        });
                }
                let billed = Arc::new(AtomicUsize::new(0));
                request.extensions_mut().insert(billed.clone());
                // Client-supplied internal-looking headers confer no privilege.
                request
                    .headers_mut()
                    .insert("x-internal-warmup", "true".parse().unwrap());
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), status);
                let body = to_bytes(response.into_body(), 4096).await.unwrap();
                assert_eq!(body == "admitted", status == StatusCode::OK);
                assert_eq!(
                    billed.load(Ordering::SeqCst),
                    usize::from(status == StatusCode::OK)
                );
            }
        }
    }

    #[tokio::test]
    async fn model_registry_admission_checks_implicit_defaults_and_fails_closed() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        model_registry::view(&["published".into()], &Default::default()).unwrap();
        let app = Router::new()
            .route("/v1/images/generations", post(|| async { "must not run" }))
            .layer(axum::middleware::from_fn(model_registry_middleware));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/images/generations")
                    .body(Body::from(r#"{"prompt":"test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        std::fs::write(
            crate::modules::account::get_data_dir()
                .unwrap()
                .join("model_registry.json"),
            b"{",
        )
        .unwrap();
        assert_eq!(
            resolve_public_model("published").unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn model_registry_admission_covers_gemini_multipart_and_missing_models() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        model_registry::view(&["published".into()], &Default::default()).unwrap();
        let mut app = Router::new()
            .route("/v1beta/models/:model", post(|| async { "admitted" }))
            .route(
                "/v1beta/models/:model/countTokens",
                post(|| async { "admitted" }),
            );
        for path in [
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/responses",
            "/responses",
            "/responses/compact",
            "/v1/messages",
            "/v1/messages/count_tokens",
            "/v1/images/generations",
            "/v1/images/edits",
            "/v1/audio/transcriptions",
        ] {
            app = app.route(path, post(|| async { "admitted" }));
        }
        let app = app.layer(axum::middleware::from_fn(model_registry_middleware));
        for (path, body, content_type, status) in [
            ("/v1beta/models/published%3AgenerateContent", r#"{"model":"unpublished"}"#, "application/json", StatusCode::OK),
            ("/v1beta/models/unpublished:streamGenerateContent", r#"{"model":"published"}"#, "application/json", StatusCode::NOT_FOUND),
            ("/v1beta/models/unpublished/countTokens", "{}", "application/json", StatusCode::NOT_FOUND),
            ("/v1/chat/completions", "{}", "application/json", StatusCode::BAD_REQUEST),
            ("/v1/completions", "{}", "application/json", StatusCode::BAD_REQUEST),
            ("/v1/responses", "{}", "application/json", StatusCode::BAD_REQUEST),
            ("/responses", "{}", "application/json", StatusCode::BAD_REQUEST),
            ("/responses/compact", "{}", "application/json", StatusCode::BAD_REQUEST),
            ("/v1/messages", "{}", "application/json", StatusCode::BAD_REQUEST),
            ("/v1/messages/count_tokens", "{}", "application/json", StatusCode::BAD_REQUEST),
            ("/v1/images/generations", "{}", "application/json", StatusCode::NOT_FOUND),
            ("/v1/images/generations", r#"{"model":null}"#, "application/json", StatusCode::BAD_REQUEST),
            ("/v1/images/edits", "--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\ntest\r\n--b--\r\n", "multipart/form-data; boundary=b", StatusCode::NOT_FOUND),
            ("/v1/audio/transcriptions", "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\nbytes\r\n--b--\r\n", "multipart/form-data; boundary=b", StatusCode::NOT_FOUND),
        ] {
            let response = app.clone().oneshot(Request::builder().method("POST").uri(path)
                .header(header::CONTENT_TYPE, content_type).body(Body::from(body)).unwrap()).await.unwrap();
            assert_eq!(response.status(), status, "{path}");
        }
    }

    #[tokio::test]
    async fn model_registry_responses_followup_restores_public_model_before_billing() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let mapping =
            std::collections::HashMap::from([("public-pay".into(), "NativeModel".into())]);
        let legacy = vec!["public-pay".into()];
        let mut view = model_registry::view(&legacy, &mapping).unwrap();
        let previous = format!("resp-{}", uuid::Uuid::new_v4());
        crate::proxy::http_session_store::save_session(
            previous.clone(),
            crate::proxy::http_session_store::HttpSessionEntry {
                input_items: vec![],
                instructions: String::new(),
                model: "public-pay".into(),
                last_accessed: std::time::Instant::now(),
            },
        )
        .await;
        async fn public_billing(request: Request, next: Next) -> Response {
            assert_eq!(
                super::super::user_token_models::memoized_model(request.extensions()),
                Some(Some("public-pay".into()))
            );
            assert_eq!(
                request
                    .extensions()
                    .get::<PublishedModel>()
                    .unwrap()
                    .native_model,
                "NativeModel"
            );
            assert!(crate::modules::billing::policy_for_model(
                &["public-pay".into()],
                &[],
                "public-pay"
            )
            .is_some());
            assert!(crate::modules::billing::policy_for_model(
                &["public-pay".into()],
                &[],
                "NativeModel"
            )
            .is_none());
            billing_boundary(request, next).await
        }
        let app = Router::new()
            .route(
                "/v1/responses",
                post(|Json(body): Json<serde_json::Value>| async move { Json(body) }),
            )
            .layer(axum::middleware::from_fn(public_billing))
            .layer(axum::middleware::from_fn(model_registry_middleware));
        for disabled in [false, true] {
            if disabled {
                view.published_models[0].enabled = false;
                model_registry::save(
                    model_registry::SaveModelRegistryRequest {
                        revision: view.revision,
                        published_models: view.published_models.clone(),
                        ignored_candidate_ids: vec![],
                        allow_unverified: false,
                    },
                    &legacy,
                    &mapping,
                )
                .unwrap();
            }
            let billed = Arc::new(AtomicUsize::new(0));
            let mut request = Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"previous_response_id":previous,"input":"next"}).to_string(),
                ))
                .unwrap();
            request.extensions_mut().insert(billed.clone());
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                if disabled {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::OK
                }
            );
            assert_eq!(billed.load(Ordering::SeqCst), usize::from(!disabled));
            if !disabled {
                let value: serde_json::Value =
                    serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                        .unwrap();
                assert_eq!(value["model"], "public-pay");
            }
        }
    }
}
