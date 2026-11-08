//! Cross-source round-robin for publications that bind targets on several pooled sources.
//!
//! Runs after admission/billing/monitoring and before the per-source dispatchers. Each
//! dispatcher only ever sees a single-source binding ([`PublishedModel::for_source`]), so
//! the provider layers stay unaware of other sources. Successive requests of a publication
//! start at successive sources (its own source first), spreading load across them; a
//! request moves on to the following source only on a refusal that another source may not
//! share (rate limit, unavailable accounts, upstream failure). A response that already
//! started streaming, or a refusal about the request itself, is returned as is.
use crate::modules::model_registry::PublishedModel;
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::Request,
    http::{request::Parts, Method, StatusCode},
    middleware::Next,
    response::Response,
};

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use super::error_response;

/// Next starting source per publication record. Only multi-source publications get an
/// entry, so the map stays bounded by the registry's publication limit.
static TURNS: LazyLock<Mutex<HashMap<String, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// This request's source order: the sources rotated by one step per request.
fn rotation<'a>(published: &'a PublishedModel) -> Vec<&'a str> {
    let mut sources = published.sources();
    let start = TURNS.lock().map_or(0, |mut turns| {
        let turn = turns.entry(published.id.clone()).or_insert(0);
        let start = *turn % sources.len();
        *turn = turn.wrapping_add(1);
        start
    });
    sources.rotate_left(start);
    sources
}

pub async fn route(request: Request, next: Next) -> Response {
    if request.method() != Method::POST
        || !crate::proxy::middleware::model_registry::is_inference_path(request.uri().path())
    {
        return next.run(request).await;
    }
    let Some(published) = request
        .extensions()
        .get::<PublishedModel>()
        .filter(|published| !published.extra_targets.is_empty())
        .cloned()
    else {
        return next.run(request).await;
    };
    let (parts, body) = request.into_parts();
    let limit = std::env::var("ABV_MAX_BODY_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50 * 1024 * 1024);
    let bytes = match to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "Request exceeds the configured body limit.",
            )
        }
    };
    let sources = rotation(&published);
    for (index, source) in sources.iter().enumerate() {
        let Some(binding) = published.for_source(source) else {
            continue;
        };
        let attempt = next.clone().run(rebuild(&parts, &bytes, binding)).await;
        let last = index + 1 == sources.len();
        if last || !fails_over(attempt.status()) {
            return attempt;
        }
        tracing::warn!(
            public_id = %published.public_id,
            source = %source,
            status = attempt.status().as_u16(),
            "Source could not serve the publication; trying its next source"
        );
    }
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "model_source_unavailable",
        "No source of this model could serve the request.",
    )
}

/// Statuses that describe the source (its accounts, quota or upstream), not the request.
fn fails_over(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

fn rebuild(parts: &Parts, bytes: &Bytes, binding: PublishedModel) -> Request {
    let mut request = Request::new(Body::from(bytes.clone()));
    *request.method_mut() = parts.method.clone();
    *request.uri_mut() = parts.uri.clone();
    *request.version_mut() = parts.version;
    *request.headers_mut() = parts.headers.clone();
    *request.extensions_mut() = parts.extensions.clone();
    request.extensions_mut().insert(binding);
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::model_registry::ModelTarget;
    use axum::{routing::post, Router};
    use tower::ServiceExt;

    /// `record` keys the rotation; tests use distinct records so each starts at its own source.
    fn publication(record: &str, extra_targets: Vec<ModelTarget>) -> PublishedModel {
        PublishedModel {
            id: record.into(),
            public_id: "shared".into(),
            display_name: "shared".into(),
            source_id: "workbuddy:default".into(),
            native_model: "wb-model".into(),
            extra_native_models: vec!["wb-model-sg".into()],
            extra_targets,
            enabled: true,
            legacy: false,
        }
    }
    fn target(source: &str, native: &str) -> ModelTarget {
        ModelTarget {
            source_id: source.into(),
            native_model: native.into(),
        }
    }

    /// The handler answers with the status scripted for the source it was handed.
    async fn run(published: PublishedModel, statuses: &[(&'static str, StatusCode)]) -> Response {
        let statuses = statuses.to_vec();
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(move |request: Request| {
                    let statuses = statuses.clone();
                    async move {
                        let binding = request
                            .extensions()
                            .get::<PublishedModel>()
                            .unwrap()
                            .clone();
                        assert!(binding.extra_targets.is_empty());
                        let status = statuses
                            .iter()
                            .find(|(source, _)| *source == binding.source_id)
                            .map(|(_, status)| *status)
                            .unwrap();
                        let body = to_bytes(request.into_body(), usize::MAX).await.unwrap();
                        assert_eq!(&body[..], b"{}");
                        let mut response =
                            Response::new(Body::from(binding.native_models().join(",")));
                        *response.status_mut() = status;
                        response
                    }
                }),
            )
            .layer(axum::middleware::from_fn(route));
        let mut request = Request::post("/v1/chat/completions")
            .body(Body::from("{}"))
            .unwrap();
        request.extensions_mut().insert(published);
        app.oneshot(request).await.unwrap()
    }
    async fn text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn narrows_each_source_and_orders_own_source_first() {
        let published = publication(
            "narrow",
            vec![
                target("chatgpt:default", "gpt-a"),
                target("chatgpt:default", "gpt-b"),
            ],
        );
        assert_eq!(
            published.sources(),
            vec!["workbuddy:default", "chatgpt:default"]
        );
        let own = published.for_source("workbuddy:default").unwrap();
        assert_eq!(own.native_models(), vec!["wb-model", "wb-model-sg"]);
        assert!(own.extra_targets.is_empty());
        let other = published.for_source("chatgpt:default").unwrap();
        assert_eq!(other.source_id, "chatgpt:default");
        assert_eq!(other.native_models(), vec!["gpt-a", "gpt-b"]);
        assert_eq!(other.public_id, "shared");
        assert!(published.for_source("antigravity:default").is_none());
    }

    #[tokio::test]
    async fn successive_requests_rotate_across_sources() {
        let published = publication("rotate", vec![target("chatgpt:default", "gpt-a")]);
        let ok = [
            ("workbuddy:default", StatusCode::OK),
            ("chatgpt:default", StatusCode::OK),
        ];
        let mut served = Vec::new();
        for _ in 0..3 {
            served.push(text(run(published.clone(), &ok).await).await);
        }
        assert_eq!(
            served,
            ["wb-model,wb-model-sg", "gpt-a", "wb-model,wb-model-sg"]
        );
    }

    #[tokio::test]
    async fn source_refusals_fail_over_to_the_next_source() {
        let published = publication("failover", vec![target("chatgpt:default", "gpt-a")]);
        let statuses = [
            ("workbuddy:default", StatusCode::TOO_MANY_REQUESTS),
            ("chatgpt:default", StatusCode::OK),
        ];
        // Whichever source the rotation starts at, the refusing source is skipped.
        for _ in 0..2 {
            let response = run(published.clone(), &statuses).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(text(response).await, "gpt-a");
        }
    }

    #[tokio::test]
    async fn request_refusals_and_the_last_source_answer_as_is() {
        let published = publication("refusal", vec![target("chatgpt:default", "gpt-a")]);
        let response = run(
            published,
            &[
                ("workbuddy:default", StatusCode::BAD_REQUEST),
                ("chatgpt:default", StatusCode::OK),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let published = publication("exhausted", vec![target("chatgpt:default", "gpt-a")]);
        let response = run(
            published,
            &[
                ("workbuddy:default", StatusCode::SERVICE_UNAVAILABLE),
                ("chatgpt:default", StatusCode::BAD_GATEWAY),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(text(response).await, "gpt-a");
    }
}
