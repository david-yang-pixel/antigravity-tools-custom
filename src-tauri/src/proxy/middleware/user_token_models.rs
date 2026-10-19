//! Protocol-independent model extraction and billable/free catalog filtering.
use axum::{
    body::{to_bytes, Body},
    extract::{FromRequest, FromRequestParts, Multipart, Path, Request},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use std::collections::HashMap;

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"type": code, "code": code, "message": message}})),
    )
        .into_response()
}
fn invalid_model() -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "model_required",
        "Provide one explicit model ID for this API key.",
    )
}
pub fn is_model_list(request: &Request) -> bool {
    request.method() == Method::GET
        && matches!(
            request.uri().path(),
            "/v1/models"
                | "/v1/models/claude"
                | "/v1/responses/models"
                | "/responses/models"
                | "/v1beta/models"
        )
}

/// The one model a request targets, read without consuming it. `None` for routes
/// that use no model (model lists, telemetry); a request whose model cannot be
/// determined is rejected so per-model policies cannot be bypassed.
pub async fn requested_model(mut request: Request) -> Result<(Request, Option<String>), Response> {
    if is_model_list(&request) {
        return Ok((request, None));
    }
    if request.headers().get(header::UPGRADE).is_some() {
        // WebSocket frames can switch models after the HTTP handshake. Do not
        // let a restricted key bypass policy; HTTP Responses/SSE remains supported.
        return Err(error(
            StatusCode::FORBIDDEN,
            "restricted_key_requires_http",
            "Model-restricted keys must use HTTP/SSE, not WebSocket transport.",
        ));
    }
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    if matches!(
        path.as_str(),
        "/v1/api/event_logging" | "/v1/api/event_logging/batch"
    ) {
        return Ok((request, None));
    }
    let model = if path.starts_with("/v1beta/models/") || request.method() == Method::GET {
        let (mut parts, body) = request.into_parts();
        let params = Path::<HashMap<String, String>>::from_request_parts(&mut parts, &())
            .await
            .map_err(|_| invalid_model())?;
        let model = params.get("model").ok_or_else(invalid_model)?.clone();
        request = Request::from_parts(parts, body);
        if method == Method::POST && path.starts_with("/v1beta/") && !path.ends_with("/countTokens")
        {
            model
                .rsplit_once(':')
                .map(|(id, _)| id.to_owned())
                .unwrap_or(model)
        } else {
            model
        }
    } else {
        let limit = std::env::var("ABV_MAX_BODY_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100 * 1024 * 1024);
        let (parts, body) = request.into_parts();
        let bytes = to_bytes(body, limit).await.map_err(|_| {
            error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_body_unavailable",
                "Request body could not be read within the configured size limit.",
            )
        })?;
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let model = if content_type.starts_with("multipart/form-data") {
            let copy = Request::from_parts(parts.clone(), Body::from(bytes.clone()));
            let mut multipart = Multipart::from_request(copy, &())
                .await
                .map_err(|_| invalid_model())?;
            let mut model = None;
            while let Some(field) = multipart.next_field().await.map_err(|_| invalid_model())? {
                if field.name() == Some("model") {
                    if model.is_some() {
                        return Err(invalid_model());
                    }
                    model = Some(field.text().await.map_err(|_| invalid_model())?);
                }
            }
            model.ok_or_else(invalid_model)?
        } else {
            let body: Value = serde_json::from_slice(&bytes).map_err(|_| invalid_model())?;
            body.get("model")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or_else(invalid_model)?
                .to_owned()
        };
        request = Request::from_parts(parts, Body::from(bytes));
        model
    };
    Ok((request, Some(model)))
}

pub async fn filter_model_list(
    response: Response,
    free_models: &[String],
    prices: &[crate::modules::billing::ModelPrice],
) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, 8 * 1024 * 1024).await else {
        return error(
            StatusCode::BAD_GATEWAY,
            "model_list_unavailable",
            "Model list unavailable.",
        );
    };
    let Ok(mut json) = serde_json::from_slice::<Value>(&bytes) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "model_list_unavailable",
            "Model list unavailable.",
        );
    };
    for field in ["data", "models"] {
        if let Some(models) = json.get_mut(field).and_then(Value::as_array_mut) {
            models.retain(|model| {
                model
                    .get("id")
                    .or_else(|| model.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|id| {
                        crate::modules::billing::policy_for_model(free_models, prices, id).is_some()
                    })
            });
        }
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    parts
        .headers
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Response::from_parts(parts, Body::from(json.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        middleware::{from_fn, Next},
        routing::{get, post},
        Router,
    };
    use tower::ServiceExt;
    async fn policy(request: Request, next: Next) -> Response {
        let (request, model) = match requested_model(request).await {
            Ok(resolved) => resolved,
            Err(r) => return r,
        };
        if model.as_deref().is_some_and(|id| {
            crate::modules::billing::policy_for_model(&["allowed-model".into()], &[], id).is_none()
        }) {
            return error(
                StatusCode::FORBIDDEN,
                "model_not_priced",
                "Model unavailable",
            );
        }
        next.run(request).await
    }
    fn app() -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: String| async move { body }),
            )
            .route("/v1/images/edits", post(|body: String| async move { body }))
            .route("/v1beta/models/:model", post(|| async { "allowed" }))
            .route("/v1/models/:model", get(|| async { "allowed" }))
            .route(
                "/v1beta/models/:model/countTokens",
                post(|| async { "allowed" }),
            )
            .route("/v1/responses", get(|| async { "websocket" }))
            .layer(from_fn(policy))
    }
    #[tokio::test]
    async fn user_token_models_enforce_json_and_preserve_request() {
        let body = r#"{"model":"allowed-model","messages":[],"stream":true}"#;
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let r = app().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(to_bytes(r.into_body(), 1000).await.unwrap(), body);
        for (body, status) in [
            (r#"{"model":"denied-model"}"#, StatusCode::FORBIDDEN),
            (r#"{}"#, StatusCode::BAD_REQUEST),
        ] {
            let req = Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .body(Body::from(body))
                .unwrap();
            assert_eq!(app().oneshot(req).await.unwrap().status(), status);
        }
    }
    #[tokio::test]
    async fn user_token_models_enforce_decoded_gemini_path_and_websocket() {
        for (uri, status) in [
            (
                "/v1beta/models/allowed-model%3AgenerateContent",
                StatusCode::OK,
            ),
            (
                "/v1beta/models/denied-model:streamGenerateContent?model=allowed-model",
                StatusCode::FORBIDDEN,
            ),
        ] {
            let r = app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .body(Body::from(r#"{"model":"allowed-model"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), status);
        }
        let r = app()
            .oneshot(
                Request::builder()
                    .uri("/v1/responses")
                    .header("Upgrade", "websocket")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn user_token_models_enforce_multipart() {
        for (model, status) in [
            ("allowed-model", StatusCode::OK),
            ("denied-model", StatusCode::FORBIDDEN),
        ] {
            let body=format!("--boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--boundary--\r\n");
            let r = app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/images/edits")
                        .header("Content-Type", "multipart/form-data; boundary=boundary")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), status);
        }
    }
    #[tokio::test]
    async fn user_token_models_filter_both_catalog_formats() {
        let response=Json(serde_json::json!({"data":[{"id":"allowed-model"},{"id":"denied"}],"models":[{"name":"models/allowed-model"},{"name":"models/denied"}]})).into_response();
        let r = filter_model_list(response, &["allowed-model".into()], &[]).await;
        let v: Value =
            serde_json::from_slice(&to_bytes(r.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(v["data"].as_array().unwrap().len(), 1);
        assert_eq!(v["models"].as_array().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn user_token_models_reject_ambiguous_names_and_duplicate_form_models() {
        let request = Request::builder()
            .method("POST")
            .uri("/v1beta/models/allowed-model:denied:generateContent")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from(r#"{"model":"models/allowed-model"}"#))
            .unwrap();
        assert_eq!(
            app().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
        let body = "--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nallowed-model\r\n--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ndenied\r\n--b--\r\n";
        let request = Request::builder()
            .method("POST")
            .uri("/v1/images/edits")
            .header("Content-Type", "multipart/form-data; boundary=b")
            .body(Body::from(body))
            .unwrap();
        assert_eq!(
            app().oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1beta/models/allowed-model/countTokens")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    }
}
