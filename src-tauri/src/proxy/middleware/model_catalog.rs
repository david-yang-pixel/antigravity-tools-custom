//! Admission against the same catalog exposed by the model APIs.
use crate::modules::model_catalog::{self, Provider};
use crate::proxy::common::model_mapping::ModelRoutingMode;
use crate::proxy::server::AppState;
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

pub fn unsupported_model_error(model: &str) -> serde_json::Value {
    serde_json::json!({"error": {
        "type": "invalid_request_error",
        "code": "model_not_supported",
        "param": "model",
        "message": format!("该模型不支持：{}。请前往「可用模型」查看支持的模型。 / This model is not supported. See Available Models for supported model IDs.", model)
    }})
}

pub async fn is_supported(state: &AppState, model: &str) -> bool {
    supported_provider(state, model).await.is_some()
}

async fn supported_provider(state: &AppState, model: &str) -> Option<Provider> {
    let provider = model_catalog::selected_provider(model)?;
    if provider == Provider::Antigravity {
        state.token_manager.refresh_pending_accounts().await;
        return state
            .token_manager
            .get_raw_collected_models()
            .contains(model)
            .then_some(provider);
    }
    // Explicit provider routes retain their own disabled/configuration errors.
    let mapping = state.custom_mapping.read().await;
    let workbuddy_route = match crate::modules::workbuddy::load() {
        Ok(settings) => settings.target_model(model, &mapping).is_some(),
        Err(_) => crate::modules::workbuddy::known_model_on_configuration_error(model, &mapping),
    };
    if !workbuddy_route {
        return None;
    }
    // Preserve useful errors for an already selected provider when disabled or broken.
    if crate::modules::workbuddy::load().is_err()
        || crate::modules::workbuddy::load().is_ok_and(|settings| !settings.enabled)
    {
        return Some(provider);
    }
    model_catalog::workbuddy_verified(model, &mapping).then_some(provider)
}

fn is_model_request(request: &Request) -> bool {
    request.method() == Method::POST
        && (matches!(
            request.uri().path(),
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
                | "/v1/models/detect"
        ) || request.uri().path().starts_with("/v1beta/models/"))
}

pub async fn validate_model(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let routing = ModelRoutingMode::from_only_raw(*state.only_raw_quota_models.read().await);
    let model_list = super::user_token_models::is_model_list(&request);
    match admit_request(request, routing, |model| async move {
        supported_provider(&state, &model).await
    })
    .await
    {
        Ok(request) => {
            let response = next.run(request).await;
            if model_list {
                match crate::modules::billing::load_prices() {
                    Ok(prices) => super::user_token_models::filter_model_list(response, &[], &prices).await,
                    Err(_) => (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
                        "error":{"code":"model_prices_unavailable","message":"Model pricing is unavailable."}
                    }))).into_response(),
                }
            } else {
                response
            }
        }
        Err(response) => response,
    }
}

async fn admit_request<F, Fut>(
    mut request: Request,
    routing: ModelRoutingMode,
    supports: F,
) -> Result<Request, Response>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Option<Provider>>,
{
    request.extensions_mut().insert(routing);
    if !routing.is_exact() || !is_model_request(&request) {
        return Ok(request);
    }
    let (mut request, model) = super::user_token_models::requested_model(request).await?;
    if let Some(model) = model {
        if let Some(provider) = supports(model.clone()).await {
            request.extensions_mut().insert(provider);
        } else {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(unsupported_model_error(&model)),
            )
                .into_response());
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        middleware::from_fn,
        routing::post,
        Router,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn exact_model_admission_carries_a_stable_policy_to_handlers() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let configured = Arc::new(AtomicBool::new(true));
        let snapshot = ModelRoutingMode::from_only_raw(configured.load(Ordering::Relaxed));
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from(r#"{"model":"gemini-2.5-flash"}"#))
            .unwrap();
        let request = admit_request(request, snapshot, |_| async {
            configured.store(false, Ordering::Relaxed);
            Some(Provider::Antigravity)
        })
        .await
        .unwrap();
        assert_eq!(
            request.extensions().get::<ModelRoutingMode>(),
            Some(&ModelRoutingMode::Exact)
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from(r#"{"model":"legacy-alias"}"#))
            .unwrap();
        let request = admit_request(request, ModelRoutingMode::Compatible, |_| async {
            panic!("Compatibility mode must keep legacy alias admission");
        })
        .await
        .unwrap();
        assert_eq!(
            request.extensions().get::<ModelRoutingMode>(),
            Some(&ModelRoutingMode::Compatible)
        );
    }

    fn app() -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: String| async move { body }),
            )
            .route("/v1/responses", post(|body: String| async move { body }))
            .route("/v1/messages", post(|body: String| async move { body }))
            .route("/v1/images/edits", post(|body: String| async move { body }))
            .route(
                "/v1/audio/transcriptions",
                post(|body: String| async move { body }),
            )
            .route(
                "/v1beta/models/:model",
                post(|body: String| async move { body }),
            )
            .route(
                "/v1beta/models/:model/countTokens",
                post(|body: String| async move { body }),
            )
            .layer(from_fn(|request: Request, next: Next| async {
                match admit_request(request, ModelRoutingMode::Exact, |model| {
                    std::future::ready(
                        (model == "gemini-2.5-flash").then_some(Provider::Antigravity),
                    )
                })
                .await
                {
                    Ok(request) => next.run(request).await,
                    Err(response) => response,
                }
            }))
    }

    #[tokio::test]
    async fn exact_model_admission_rejects_unknown_models_across_protocols() {
        for path in [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
            "/v1beta/models/unsupported:generateContent",
            "/v1beta/models/unsupported/countTokens",
        ] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"model":"unsupported","messages":[]}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["code"], "model_not_supported");
            assert!(body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("可用模型"));
        }
    }

    #[tokio::test]
    async fn exact_model_admission_preserves_supported_requests_and_multipart() {
        for path in [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent",
        ] {
            let body = r#"{"model":"gemini-2.5-flash","messages":[],"stream":true}"#;
            let response = app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), body);
        }
        for path in ["/v1/images/edits", "/v1/audio/transcriptions"] {
            for (model, status) in [
                ("gemini-2.5-flash", StatusCode::OK),
                ("unsupported", StatusCode::BAD_REQUEST),
            ] {
                let body = format!("--test-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--test-boundary--\r\n");
                let response = app()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(path)
                            .header(
                                "content-type",
                                "multipart/form-data; boundary=test-boundary",
                            )
                            .body(Body::from(body.clone()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), status);
                if status == StatusCode::OK {
                    assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), body);
                }
            }
        }
    }
}
