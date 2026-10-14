//! Read-only user portal. Authentication derives ownership from the presented key;
//! no caller-supplied token ID is ever used and no administrator DTO is returned.
use crate::modules::user_token_db as db;
use crate::proxy::server::AppState;
use axum::extract::{Query, State};
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct UsageOverview {
    username: String,
    token_hint: String,
    expires_at: Option<i64>,
    expired: bool,
    max_concurrency: i32,
    token_quota: i64,
    total_tokens_used: i64,
    remaining_tokens: Option<i64>,
    total_requests: i64,
    allowed_models: Vec<String>,
    available_models: Vec<String>,
    logs: db::UsageLogPage,
    daily: Vec<db::UsagePeriod>,
    weekly: Vec<db::UsagePeriod>,
    models: Vec<db::ModelUsage>,
}

#[derive(Default, Deserialize)]
struct PortalQuery {
    page: Option<u32>,
    start_time: Option<i64>,
    end_time: Option<i64>,
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/token-portal/me", get(overview))
}

async fn overview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PortalQuery>,
) -> Response {
    let mut result = read_overview(&headers, &query);
    if let Ok(data) = &mut result {
        data.available_models = if data.allowed_models.is_empty() {
            let only_raw = *state.only_raw_quota_models.read().await;
            crate::proxy::common::model_mapping::get_all_dynamic_models(
                &state.custom_mapping,
                Some(&state.token_manager),
                only_raw,
            )
            .await
        } else {
            data.allowed_models.clone()
        };
    }
    render(result)
}
fn render(result: Result<UsageOverview, StatusCode>) -> Response {
    let mut response = match result {
        Ok(data) => Json(data).into_response(),
        Err(status) => (status, Json(serde_json::json!({"error": {
            "code": if status == StatusCode::UNAUTHORIZED { "invalid_user_token" } else if status == StatusCode::BAD_REQUEST { "invalid_query" } else { "usage_unavailable" }
        }}))).into_response(),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Authorization".parse().unwrap());
    response
}

fn read_overview(headers: &HeaderMap, query: &PortalQuery) -> Result<UsageOverview, StatusCode> {
    let key = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let token = db::get_token_by_value(key)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .filter(|t| t.enabled)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let page = query.page.unwrap_or(1);
    if page == 0 || page > 1_000_000 {
        return Err(StatusCode::BAD_REQUEST);
    }
    if query
        .start_time
        .into_iter()
        .chain(query.end_time)
        .any(|ts| !(0..=253402300799).contains(&ts))
        || matches!((query.start_time, query.end_time), (Some(start), Some(end)) if start > end)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let logs = db::filtered_usage_logs_for_token(&token.id, page, query.start_time, query.end_time)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let now = chrono::Utc::now().timestamp();
    let daily = db::period_usage_for_token(&token.id, false, now)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let weekly = db::period_usage_for_token(&token.id, true, now)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // Expired / exhausted keys can still view their own historical usage.
    // Disabled or deleted keys are revoked for the portal too.
    let models =
        db::model_usage_for_token(&token.id).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let suffix: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Ok(UsageOverview {
        username: token.username,
        token_hint: format!("••••{suffix}"),
        expires_at: token.expires_at,
        expired: token
            .expires_at
            .is_some_and(|ts| ts <= chrono::Utc::now().timestamp()),
        max_concurrency: token.max_concurrency,
        token_quota: token.token_quota,
        total_tokens_used: token.total_tokens_used,
        remaining_tokens: (token.token_quota > 0)
            .then(|| (token.token_quota - token.total_tokens_used).max(0)),
        total_requests: token.total_requests,
        available_models: token.allowed_models.clone(),
        allowed_models: token.allowed_models,
        logs,
        daily,
        weekly,
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    fn test_routes() -> Router {
        Router::new().route(
            "/api/token-portal/me",
            get(
                |headers: HeaderMap, Query(query): Query<PortalQuery>| async move {
                    render(read_overview(&headers, &query))
                },
            ),
        )
    }
    fn key(name: &str) -> db::UserToken {
        db::init_db().unwrap();
        db::create_token(
            name.into(),
            "never".into(),
            None,
            0,
            None,
            None,
            None,
            2,
            100,
            vec!["model-a".into()],
        )
        .unwrap()
    }
    #[tokio::test]
    async fn user_token_portal_only_returns_authenticated_keys_aggregates() {
        let a = key("portal-a");
        let b = key("portal-b");
        db::record_token_usage_and_ip(&a.id, "secret-ip", "model-a", 30, 20, 200, None).unwrap();
        db::record_token_usage_and_ip(&a.id, "secret-ip", "model-b", 10, 0, 429, None).unwrap();
        db::record_token_usage_and_ip(&b.id, "other-ip", "other-model", 999, 999, 200, None)
            .unwrap();
        let response = test_routes()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/token-portal/me?token_id={}", b.id))
                    .header("Authorization", format!("Bearer {}", a.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = to_bytes(response.into_body(), 10000).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        for forbidden in [
            &a.token,
            &b.token,
            &a.id,
            &b.id,
            "secret-ip",
            "other-model",
            "portal-b",
        ] {
            assert!(!text.contains(forbidden));
        }
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["total_tokens_used"], 60);
        assert_eq!(v["remaining_tokens"], 40);
        assert_eq!(v["models"].as_array().unwrap().len(), 2);
        assert_eq!(v["models"][0]["input_tokens"], 30);
        assert_eq!(v["models"][1]["failed_requests"], 1);
    }
    #[tokio::test]
    async fn user_token_portal_validates_time_filters() {
        let a = key("time-filter");
        for query in [
            "start_time=2&end_time=1",
            "start_time=-1",
            "end_time=253402300800",
            "start_time=not-a-time",
        ] {
            let response = test_routes()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/token-portal/me?{query}"))
                        .header("Authorization", format!("Bearer {}", a.token))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }
    #[tokio::test]
    async fn user_token_portal_revocation_and_exhaustion() {
        let a = key("revoked");
        db::record_token_usage_and_ip(&a.id, "local", "model-a", 100, 10, 200, None).unwrap();
        let request = || {
            Request::builder()
                .uri("/api/token-portal/me")
                .header("Authorization", format!("Bearer {}", a.token))
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            test_routes().oneshot(request()).await.unwrap().status(),
            StatusCode::OK
        );
        db::update_token(
            &a.id,
            None,
            None,
            Some(false),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            test_routes().oneshot(request()).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            test_routes()
                .oneshot(
                    Request::builder()
                        .uri("/api/token-portal/me?token=anything")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            test_routes()
                .oneshot(
                    Request::builder()
                        .uri("/api/token-portal/me")
                        .header("Authorization", "Bearer global-admin-key")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
