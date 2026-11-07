// Upstream 模块 - 上游客户端
// 对应上游通讯接口

pub mod chat_codec;
pub(crate) mod chat_relay;
pub mod client;
pub mod native;
pub mod openai_compat;
pub mod responses_wire;
pub mod retry;
pub mod workbuddy;

use axum::{
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

/// OpenAI-style error body shared by the native (non-Antigravity) providers.
pub(crate) fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"type": code, "code": code, "message": message}})),
    )
        .into_response()
}

/// Keeps the monitor's captured upstream body naming the native ID actually sent. The
/// body is stamped and re-serialized only when `model` differs from the last capture
/// recorded in `captured` (start with an empty string to capture unconditionally).
pub(crate) fn recapture(
    capture: Option<&crate::proxy::monitor::UpstreamRequestBodyHolder>,
    captured: &mut String,
    model: &str,
    payload: &serde_json::Value,
) {
    if captured != model {
        if let Some(capture) = capture {
            let mut body = payload.clone();
            body["model"] = serde_json::json!(model);
            capture.set_value(&body);
        }
        *captured = model.to_owned();
    }
}

/// The registry binding admitted for this request. Normally inserted by the model
/// registry middleware; resolved here only when a provider layer runs without it.
pub(crate) async fn published_request(
    mut request: Request,
) -> Result<(Request, crate::modules::model_registry::PublishedModel), Response> {
    if let Some(published) = request
        .extensions()
        .get::<crate::modules::model_registry::PublishedModel>()
        .cloned()
    {
        return Ok((request, published));
    }
    let (resolved, model) =
        crate::proxy::middleware::user_token_models::requested_model(request).await?;
    request = resolved;
    let published = crate::proxy::middleware::model_registry::resolve_public_model(
        model.as_deref().unwrap_or(""),
    )
    .map_err(|(status, message)| error_response(status, "model_not_published", &message))?;
    request.extensions_mut().insert(published.clone());
    Ok((request, published))
}
