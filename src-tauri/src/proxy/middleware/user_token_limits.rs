//! Per-user-key admission limits for this single-process proxy.
//! Balance is settled by monitor before the last permit is released. In-flight
//! requests finish using the policy captured at admission.
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
    billing_policy: std::sync::OnceLock<crate::modules::billing::BillingPolicy>,
}

impl UserTokenPermit {
    pub(super) fn acquire(token_id: &str, limit: i32) -> Result<Arc<Self>, LimitError> {
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
            billing_policy: std::sync::OnceLock::new(),
        }))
    }

    pub fn model(&self) -> Option<&str> {
        self.model.get().map(String::as_str)
    }

    pub fn billing_policy(&self) -> Option<&crate::modules::billing::BillingPolicy> {
        self.billing_policy.get()
    }

    pub(super) fn set_billing_policy(
        &self,
        model: String,
        policy: crate::modules::billing::BillingPolicy,
    ) {
        let _ = self.model.set(model);
        let _ = self.billing_policy.set(policy);
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
pub(super) enum LimitError {
    Concurrency,
    UnpricedModel,
    InsufficientBalance,
    AccountingUnavailable,
}
impl IntoResponse for LimitError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Concurrency => (StatusCode::TOO_MANY_REQUESTS, "concurrency_limit_exceeded", "This API key has reached its concurrent request limit. Retry after an active request finishes."),
            Self::UnpricedModel => (StatusCode::FORBIDDEN, "model_not_priced", "This model has no configured price and is not free for this API key."),
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
    if request.headers().contains_key(axum::http::header::UPGRADE) {
        // The legacy WebSocket handler calls generation handlers directly, bypassing
        // per-frame admission and usage settlement. Restricted keys require HTTP/SSE.
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": {
                "code": "restricted_key_requires_http", "type": "restricted_key_requires_http",
                "message": "User API keys must use HTTP/SSE for balance accounting."
            }})),
        )
            .into_response();
    }
    let permit = match UserTokenPermit::acquire(&token.id, token.max_concurrency) {
        Ok(permit) => permit,
        Err(error) => return error.into_response(),
    };
    let filter_models = super::user_token_models::is_model_list(&request);
    let (resolved_request, model) = match super::user_token_models::requested_model(request).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    request = resolved_request;
    let rules = match crate::modules::billing::load_prices() {
        Ok(rules) => rules,
        Err(_) => return LimitError::AccountingUnavailable.into_response(),
    };
    if let Some(model) = model {
        let Some(policy) =
            crate::modules::billing::policy_for_model(&token.free_models, &rules, &model)
        else {
            return LimitError::UnpricedModel.into_response();
        };
        if let crate::modules::billing::BillingPolicy::Priced(price) = &policy {
            if !price.admits(token.balance_micros.unwrap_or(0)) {
                return LimitError::InsufficientBalance.into_response();
            }
        }
        permit.set_billing_policy(model, policy);
    }
    request.extensions_mut().insert(permit.clone());
    let response = next.run(request).await;
    let response = if filter_models {
        super::user_token_models::filter_model_list(response, &token.free_models, &rules).await
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
        let token = db::create_token(
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
        .unwrap();
        db::connect_db()
            .unwrap()
            .execute(
                "UPDATE user_tokens SET free_models='[\"test\"]' WHERE id=?1",
                [&token.id],
            )
            .unwrap();
        token
    }
    fn request(key: &db::UserToken) -> Request {
        let mut req = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::from(r#"{"model":"test"}"#))
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
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
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
    async fn user_token_legacy_quota_does_not_block_free_requests_and_usage_is_retained() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let a = key(2, 100);
        let router = app();
        let active = router.clone().oneshot(request(&a)).await.unwrap();
        db::record_token_usage_and_ip(&a.id, "127.0.0.1", "test", 60, 40, 200, None).unwrap();
        assert_eq!(
            router.oneshot(request(&a)).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(active.status(), StatusCode::OK);
        assert_eq!(
            db::get_token_by_id(&a.id)
                .unwrap()
                .unwrap()
                .total_tokens_used,
            100
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
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        for (concurrency, quota) in [(1, 0), (0, 100), (0, 0)] {
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
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
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
            // A known key must not bypass billing even with authentication off.

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
                    .body(Body::from(r#"{"model":"unpriced"}"#))
                    .unwrap();
                assert_eq!(
                    router.clone().oneshot(req).await.unwrap().status(),
                    StatusCode::FORBIDDEN
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
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        use crate::modules::billing::{self, ModelPrice, Price};
        db::init_db().unwrap();
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

        // Missing prices are denied unless explicitly free; legacy allowed_models
        // never grants a free entitlement or blocks a priced model.
        assert_eq!(status(&free).await, StatusCode::FORBIDDEN);
        db::connect_db()
            .unwrap()
            .execute(
                "UPDATE user_tokens SET free_models=?1, allowed_models='[\"other\"]' WHERE id=?2",
                rusqlite::params![serde_json::to_string(&vec![&free]).unwrap(), &k.id],
            )
            .unwrap();
        // A zero balance blocks priced models but not explicit freebies.
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
        record(&free, 9999, 9999, 200); // explicitly free
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

        // Every administrator balance edit is in the ledger; NULL cannot
        // switch off charging, and a free grant overrides even a priced model.
        let ledger: Vec<i64> = db::connect_db().unwrap()
            .prepare("SELECT delta_micros FROM balance_ledger WHERE token_id = ?1 ORDER BY created_at, rowid").unwrap()
            .query_map([&k.id], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(ledger, vec![600]);
        assert!(db::update_token_with_balance(
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
            Some(None)
        )
        .is_err());
        db::connect_db()
            .unwrap()
            .execute(
                "UPDATE user_tokens SET free_models=?1 WHERE id=?2",
                rusqlite::params![
                    serde_json::to_string(&vec![&free, &per_call]).unwrap(),
                    &k.id
                ],
            )
            .unwrap();
        assert_eq!(status(&per_call).await, StatusCode::OK);
        record(&per_call, 9999, 9999, 200);
        assert_eq!(
            db::get_token_by_id(&k.id).unwrap().unwrap().balance_micros,
            Some(-100)
        );

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
    #[tokio::test]
    async fn user_token_settlement_keeps_admission_price_and_free_grant() {
        use crate::modules::billing::{self, ModelPrice, Price};
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let key = key(0, 0);
        db::connect_db()
            .unwrap()
            .execute(
                "UPDATE user_tokens SET balance_micros=1000 WHERE id=?1",
                [&key.id],
            )
            .unwrap();
        billing::save_prices(vec![ModelPrice {
            pattern: "paid".into(),
            price: Price::PerCall {
                call_price_micros: 100,
            },
        }])
        .unwrap();
        let router = Router::new()
            .route(
                "/",
                post(
                    |axum::Extension(permit): axum::Extension<Arc<UserTokenPermit>>| async move {
                        // Administrative changes during the request apply to future requests.
                        billing::save_prices(vec![]).unwrap();
                        db::connect_db()
                            .unwrap()
                            .execute(
                                "UPDATE user_tokens SET free_models='[\"paid\"]' WHERE id=?1",
                                [&permit.token_id],
                            )
                            .unwrap();
                        db::record_token_usage_with_policy(
                            &permit.token_id,
                            "local",
                            permit.model().unwrap(),
                            10,
                            20,
                            None,
                            200,
                            None,
                            permit.billing_policy(),
                        )
                        .unwrap();
                        "ok"
                    },
                ),
            )
            .layer(axum::middleware::from_fn(user_token_limits_middleware));
        let mut req = request(&key);
        *req.body_mut() = Body::from(r#"{"model":"paid"}"#);
        let response = router.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            db::get_token_by_id(&key.id)
                .unwrap()
                .unwrap()
                .balance_micros,
            Some(900)
        );
        let free_policy = billing::policy_for_model(&["paid".into()], &[], "paid").unwrap();
        billing::save_prices(vec![ModelPrice {
            pattern: "paid".into(),
            price: Price::PerCall {
                call_price_micros: 800,
            },
        }])
        .unwrap();
        db::record_token_usage_with_policy(
            &key.id,
            "local",
            "paid",
            10,
            20,
            None,
            200,
            None,
            Some(&free_policy),
        )
        .unwrap();
        assert_eq!(
            db::get_token_by_id(&key.id)
                .unwrap()
                .unwrap()
                .balance_micros,
            Some(900)
        );
    }
}
