//! Per-user-key admission limits for this single-process proxy.
//! Quota is settled by monitor before the last permit is released. In-flight
//! requests are allowed to finish even if another request exhausts the quota.
use super::auth::UserTokenIdentity;
use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Default)]
struct KeyState {
    active: usize,
    accounting_failed: bool,
}
static ACTIVE: Lazy<Mutex<HashMap<String, KeyState>>> = Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
pub struct UserTokenPermit {
    token_id: String,
    /// The model admission resolved (decoded route or body); settlement bills this
    /// one so the charge always matches what was admitted.
    model: std::sync::OnceLock<String>,
}

impl UserTokenPermit {
    fn acquire(token_id: &str, limit: i32) -> Result<Arc<Self>, LimitError> {
        let mut keys = ACTIVE.lock();
        let key = keys.entry(token_id.to_owned()).or_default();
        if key.accounting_failed {
            return Err(LimitError::AccountingUnavailable);
        }
        if limit > 0 && key.active >= limit as usize {
            return Err(LimitError::Concurrency);
        }
        key.active += 1;
        Ok(Arc::new(Self {
            token_id: token_id.to_owned(),
            model: std::sync::OnceLock::new(),
        }))
    }

    pub fn model(&self) -> Option<&str> {
        self.model.get().map(String::as_str)
    }

    /// Fail closed if accounting cannot be persisted; never silently grant free usage.
    pub fn accounting_failed(&self) {
        if let Some(key) = ACTIVE.lock().get_mut(&self.token_id) {
            key.accounting_failed = true;
        }
    }
}

impl Drop for UserTokenPermit {
    fn drop(&mut self) {
        let mut keys = ACTIVE.lock();
        if let Some(key) = keys.get_mut(&self.token_id) {
            key.active -= 1;
            if key.active == 0 && !key.accounting_failed {
                keys.remove(&self.token_id);
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum LimitError {
    Concurrency,
    Quota,
    InsufficientBalance,
    AccountingUnavailable,
}
impl IntoResponse for LimitError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Concurrency => (StatusCode::TOO_MANY_REQUESTS, "concurrency_limit_exceeded", "This API key has reached its concurrent request limit. Retry after an active request finishes."),
            Self::Quota => (StatusCode::TOO_MANY_REQUESTS, "token_quota_exhausted", "This API key has exhausted its lifetime token quota. Contact the administrator to increase it."),
            Self::InsufficientBalance => (StatusCode::PAYMENT_REQUIRED, "insufficient_balance", "This API key's balance is too low for the requested model. Top up your balance to continue."),
            Self::AccountingUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "token_accounting_unavailable", "Token usage accounting is unavailable. Contact the administrator."),
        };
        (
            status,
            Json(serde_json::json!({"error": {"type": code, "code": code, "message": message}})),
        )
            .into_response()
    }
}

pub async fn user_token_limits_middleware(mut request: Request, next: Next) -> Response {
    let Some(identity) = request.extensions().get::<UserTokenIdentity>().cloned() else {
        return next.run(request).await;
    };
    let ip = super::ip_filter::extract_client_ip(&request).unwrap_or_else(|| "127.0.0.1".into());
    // Also enforce user-key restrictions when proxy auth is Off but a known key is supplied.
    match crate::modules::user_token_db::validate_token(&identity.token, &ip) {
        Ok((true, _)) => {},
        Ok((false, reason)) => return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": {
            "type": "token_rejected", "code": "token_rejected", "message": reason.unwrap_or_else(|| "Token rejected".into())
        }}))).into_response(),
        Err(_) => return LimitError::AccountingUnavailable.into_response(),
    }
    let token = match crate::modules::user_token_db::get_token_by_id(&identity.token_id) {
        Ok(Some(token)) => token,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return LimitError::AccountingUnavailable.into_response(),
    };
    if token.quota_exhausted() {
        return LimitError::Quota.into_response();
    }
    if request.headers().contains_key(axum::http::header::UPGRADE)
        && (token.max_concurrency > 0 || token.has_quota_limit() || token.is_billed())
    {
        // The legacy WebSocket handler calls generation handlers directly, bypassing
        // per-frame admission and usage settlement. Restricted keys require HTTP/SSE.
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": {
                "code": "restricted_key_requires_http", "type": "restricted_key_requires_http",
                "message": "Keys with concurrency or quota limits must use HTTP/SSE."
            }})),
        )
            .into_response();
    }
    let permit = match UserTokenPermit::acquire(&token.id, token.max_concurrency) {
        Ok(permit) => permit,
        Err(error) => return error.into_response(),
    };
    let filter_models = super::user_token_models::is_model_list(&request);
    let model = if !token.allowed_models.is_empty() || token.is_billed() {
        match super::user_token_models::requested_model(request).await {
            Ok((next, model)) => {
                request = next;
                model
            }
            Err(response) => return response,
        }
    } else {
        None
    };
    if let Err(response) =
        super::user_token_models::ensure_allowed(&token.allowed_models, model.as_deref())
    {
        return response;
    }
    if let Some(model) = &model {
        let _ = permit.model.set(model.clone());
    }
    if let Some(balance) = token.balance_micros {
        // Unpriced models are free; priced ones need enough balance to start.
        let rules = match crate::modules::billing::load_prices() {
            Ok(rules) => rules,
            Err(_) => return LimitError::AccountingUnavailable.into_response(),
        };
        let price = model
            .as_deref()
            .and_then(|model| crate::modules::billing::match_price(&rules, model));
        if price.is_some_and(|rule| !rule.price.admits(balance)) {
            return LimitError::InsufficientBalance.into_response();
        }
    }
    request.extensions_mut().insert(permit.clone());
    let response = next.run(request).await;
    let response = if filter_models {
        super::user_token_models::filter_model_list(response, &token.allowed_models).await
    } else {
        response
    };
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(PermittedBody {
            body,
            permit: Some(permit),
        }),
    )
}

// A body-owned permit survives response headers, preserves trailers and releases
// on EOF, error, or client disconnect. Monitor holds another Arc until settlement.
struct PermittedBody {
    body: Body,
    permit: Option<Arc<UserTokenPermit>>,
}
impl HttpBody for PermittedBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        if matches!(&result, Poll::Ready(None | Some(Err(_)))) {
            self.permit.take();
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::user_token_db as db;
    use axum::{routing::post, Router};
    use tower::ServiceExt;

    fn key(limit: i32, quota: i64) -> db::UserToken {
        db::init_db().unwrap();
        db::create_token(
            "limit-test".into(),
            "never".into(),
            None,
            0,
            None,
            None,
            None,
            limit,
            quota,
            Vec::new(),
        )
        .unwrap()
    }
    fn request(key: &db::UserToken) -> Request {
        let mut req = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(UserTokenIdentity {
            token_id: key.id.clone(),
            token: key.token.clone(),
            username: key.username.clone(),
        });
        req
    }
    fn app() -> Router {
        Router::new()
            .route(
                "/",
                post(|| async {
                    Body::from_stream(futures::stream::pending::<
                        Result<bytes::Bytes, std::io::Error>,
                    >())
                }),
            )
            .layer(axum::middleware::from_fn(user_token_limits_middleware))
    }
    #[tokio::test]
    async fn user_token_concurrency_holds_until_disconnect_and_isolates_keys() {
        let a = key(1, 0);
        let b = key(1, 0);
        let router = app();
        let first = router.clone().oneshot(request(&a)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let denied = router.clone().oneshot(request(&a)).await.unwrap();
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = axum::body::to_bytes(denied.into_body(), 4096)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("concurrency_limit_exceeded"));
        let other = router.clone().oneshot(request(&b)).await.unwrap();
        assert_eq!(other.status(), StatusCode::OK);
        drop(first);
        assert_eq!(
            router.oneshot(request(&a)).await.unwrap().status(),
            StatusCode::OK
        );
    }
    #[tokio::test]
    async fn user_token_quota_blocks_new_requests_and_edit_restores_access() {
        let a = key(2, 100);
        let router = app();
        let active = router.clone().oneshot(request(&a)).await.unwrap();
        db::record_token_usage_and_ip(&a.id, "127.0.0.1", "test", 60, 40, 200, None).unwrap();
        let denied = router.clone().oneshot(request(&a)).await.unwrap();
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = axum::body::to_bytes(denied.into_body(), 4096)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("token_quota_exhausted"));
        assert_eq!(active.status(), StatusCode::OK);
        db::update_token(
            &a.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(200),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            db::get_token_by_id(&a.id)
                .unwrap()
                .unwrap()
                .total_tokens_used,
            100
        );
        assert_eq!(
            router.oneshot(request(&a)).await.unwrap().status(),
            StatusCode::OK
        );
    }
    #[tokio::test]
    async fn user_token_permit_survives_body_drop_until_accounting_finishes() {
        let id = uuid::Uuid::new_v4().to_string();
        let permit = UserTokenPermit::acquire(&id, 1).unwrap();
        let settlement = permit.clone();
        let body = Body::new(PermittedBody {
            body: Body::empty(),
            permit: Some(permit),
        });
        axum::body::to_bytes(body, 100).await.unwrap();
        assert!(matches!(
            UserTokenPermit::acquire(&id, 1),
            Err(LimitError::Concurrency)
        ));
        drop(settlement);
        assert!(UserTokenPermit::acquire(&id, 1).is_ok());
    }
    #[tokio::test]
    async fn user_token_limits_reject_websocket_bypass() {
        for (concurrency, quota) in [(1, 0), (0, 100)] {
            let a = key(concurrency, quota);
            let mut req = request(&a);
            req.headers_mut()
                .insert("upgrade", "websocket".parse().unwrap());
            assert_eq!(
                app().oneshot(req).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
    }
    #[test]
    fn user_token_parallel_admission_is_atomic_and_dynamic() {
        let id = uuid::Uuid::new_v4().to_string();
        let held: Vec<_> = (0..32)
            .map(|_| {
                let id = id.clone();
                std::thread::spawn(move || UserTokenPermit::acquire(&id, 3))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|t| t.join().unwrap().ok())
            .collect();
        assert_eq!(held.len(), 3);
        assert!(matches!(
            UserTokenPermit::acquire(&id, 1),
            Err(LimitError::Concurrency)
        ));
        assert!(UserTokenPermit::acquire(&id, 4).is_ok());
        drop(held);
        assert!(!ACTIVE.lock().contains_key(&id));
    }
    #[test]
    fn user_token_accounting_failure_fails_closed() {
        let id = uuid::Uuid::new_v4().to_string();
        let p = UserTokenPermit::acquire(&id, 0).unwrap();
        p.accounting_failed();
        drop(p);
        assert!(matches!(
            UserTokenPermit::acquire(&id, 0),
            Err(LimitError::AccountingUnavailable)
        ));
        ACTIVE.lock().remove(&id);
    }
    #[tokio::test]
    async fn user_token_limits_apply_to_all_header_formats_and_auth_off() {
        use crate::proxy::{ProxyAuthMode, ProxySecurityConfig};
        use tokio::sync::RwLock;
        for mode in [ProxyAuthMode::Strict, ProxyAuthMode::Off] {
            let security = Arc::new(RwLock::new(ProxySecurityConfig {
                auth_mode: mode,
                api_key: "test-master-key".into(),
                admin_password: None,
                allow_lan_access: false,
                port: 8045,
                security_monitor: crate::proxy::config::SecurityMonitorConfig::default(),
            }));
            let router = app().layer(axum::middleware::from_fn_with_state(
                security,
                super::super::auth::auth_middleware,
            ));
            let a = key(1, 1);
            db::record_token_usage_and_ip(&a.id, "127.0.0.1", "test", 1, 0, 200, None).unwrap();
            for header in ["authorization", "x-api-key", "x-goog-api-key"] {
                let value = if header == "authorization" {
                    format!("Bearer {}", a.token)
                } else {
                    a.token.clone()
                };
                let req = Request::builder()
                    .method("POST")
                    .uri("/")
                    .header(header, value)
                    .body(Body::empty())
                    .unwrap();
                assert_eq!(
                    router.clone().oneshot(req).await.unwrap().status(),
                    StatusCode::TOO_MANY_REQUESTS
                );
            }
        }
    }
    #[tokio::test]
    async fn user_token_body_eof_and_error_release_permit() {
        for error in [false, true] {
            let id = uuid::Uuid::new_v4().to_string();
            let permit = UserTokenPermit::acquire(&id, 1).unwrap();
            let body = if error {
                Body::from_stream(futures::stream::once(async {
                    Err::<bytes::Bytes, _>(std::io::Error::other("upstream failed"))
                }))
            } else {
                Body::from("done")
            };
            let result = axum::body::to_bytes(
                Body::new(PermittedBody {
                    body,
                    permit: Some(permit),
                }),
                100,
            )
            .await;
            assert_eq!(result.is_err(), error);
            assert!(UserTokenPermit::acquire(&id, 1).is_ok());
        }
    }
    #[tokio::test]
    async fn user_token_billed_key_admission_settlement_and_admin_edit() {
        use crate::modules::billing::{self, ModelPrice, Price};
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let per_call = format!("call-{tag}");
        let per_token = format!("tok-{tag}-pro");
        let free = format!("free-{tag}");
        billing::save_prices(vec![
            ModelPrice {
                pattern: per_call.clone(),
                price: Price::PerCall {
                    call_price_micros: 500,
                },
            },
            ModelPrice {
                pattern: format!("tok-{tag}-*"),
                price: Price::PerToken {
                    input_price_micros: 1_000_000,
                    output_price_micros: 2_000_000,
                    cached_price_micros: None,
                },
            },
        ])
        .unwrap();
        let k = key(0, 0);
        let set_balance = |balance: Option<i64>| {
            db::update_token_with_balance(
                &k.id,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(balance),
            )
            .unwrap()
        };
        let router = app();
        let status = |model: &str| {
            let mut req = request(&k);
            *req.body_mut() = Body::from(serde_json::json!({ "model": model }).to_string());
            let router = router.clone();
            async move { router.oneshot(req).await.unwrap().status() }
        };

        // A zero balance blocks priced models but not unpriced (free) ones.
        set_balance(Some(0));
        assert_eq!(status(&per_call).await, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(status(&per_token).await, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(status(&free).await, StatusCode::OK);

        set_balance(Some(600));
        assert_eq!(status(&per_call).await, StatusCode::OK);
        assert_eq!(status(&per_token).await, StatusCode::OK);

        let record = |model: &str, input: i32, output: i32, code: u16| {
            db::record_token_usage_and_ip(&k.id, "127.0.0.1", model, input, output, code, None)
                .unwrap()
        };
        record(&per_call, 1000, 1000, 200); // -500
        record(&per_call, 0, 0, 502); // failed call: free
        record(&per_token, 100, 50, 200); // 100 * ¥1/M + 50 * ¥2/M = 200 micros
        record(&free, 9999, 9999, 200); // unpriced: free
        let balance = db::get_token_by_id(&k.id).unwrap().unwrap().balance_micros;
        assert_eq!(balance, Some(-100), "per-token settlement may overdraw");
        assert_eq!(status(&per_token).await, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(status(&free).await, StatusCode::OK);

        let mut charges: Vec<(String, Option<i64>, Option<String>)> =
            db::usage_logs_for_token(&k.id, 1)
                .unwrap()
                .items
                .into_iter()
                .map(|log| (log.model, log.cost_micros, log.billing_mode))
                .collect();
        charges.sort();
        let entry = |model: &str, cost: i64, mode: &str| {
            (model.to_string(), Some(cost), Some(mode.to_string()))
        };
        let mut expected = vec![
            entry(&per_call, 0, "per_call"),
            entry(&per_call, 500, "per_call"),
            entry(&per_token, 200, "per_token"),
            entry(&free, 0, "free"),
        ];
        expected.sort();
        assert_eq!(charges, expected);

        // Every administrator edit is in the ledger; switching billing off stops charging.
        let ledger: Vec<i64> = db::connect_db()
            .unwrap()
            .prepare("SELECT delta_micros FROM balance_ledger WHERE token_id = ?1 ORDER BY created_at, rowid")
            .unwrap()
            .query_map([&k.id], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ledger, vec![0, 600]);
        set_balance(None);
        record(&per_call, 0, 0, 200);
        let token = db::get_token_by_id(&k.id).unwrap().unwrap();
        assert_eq!(token.balance_micros, None);
        assert!(db::usage_logs_for_token(&k.id, 1)
            .unwrap()
            .items
            .iter()
            .any(|log| log.cost_micros.is_none()));
        assert_eq!(status(&per_call).await, StatusCode::OK);

        // Settlement bills the model admission resolved, even when the client
        // percent-encodes the Gemini action separator in the URL.
        set_balance(Some(1_000));
        let gemini = Router::new()
            .route(
                "/v1beta/models/:model",
                post(
                    |axum::Extension(permit): axum::Extension<Arc<UserTokenPermit>>| async move {
                        permit.model().unwrap_or_default().to_string()
                    },
                ),
            )
            .layer(axum::middleware::from_fn(user_token_limits_middleware));
        let mut req = request(&k);
        *req.uri_mut() = format!("/v1beta/models/{per_call}%3AgenerateContent")
            .parse()
            .unwrap();
        let response = gemini.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body, per_call.as_bytes());
    }
}
