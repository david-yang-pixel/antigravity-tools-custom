//! User usage portal. Visitors sign in either with an API key (Bearer) or with an
//! email account (session cookie). Ownership always derives from the credential;
//! no caller-supplied token ID is ever used and no administrator DTO is returned.
use crate::modules::portal_account::{self as accounts, CodeDelivery, CodePurpose, PortalError};
use crate::modules::{billing, portal_mail, portal_settings, user_token_db as db};
use crate::proxy::server::AppState;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

const SESSION_COOKIE: &str = "abv_portal_session";
const COOKIE_PATH: &str = "/api/token-portal";

#[derive(Serialize)]
struct UsageOverview {
    username: String,
    token_hint: String,
    /// Email of the signed-in account; absent when signed in with a key.
    account_email: Option<String>,
    /// Registered key that the administrator has not enabled yet.
    pending_activation: bool,
    expires_at: Option<i64>,
    expired: bool,
    max_concurrency: i32,
    token_quota: i64,
    total_tokens_used: i64,
    remaining_tokens: Option<i64>,
    total_requests: i64,
    /// Prepaid balance in micro-yuan; absent when the key is not billed.
    balance_micros: Option<i64>,
    allowed_models: Vec<String>,
    available_models: Vec<String>,
    /// Price of each available model that has one; the others are free.
    model_prices: Vec<PortalModelPrice>,
    logs: db::UsageLogPage,
    daily: Vec<db::UsagePeriod>,
    weekly: Vec<db::UsagePeriod>,
    models: Vec<db::ModelUsage>,
}

#[derive(Serialize)]
struct PortalModelPrice {
    model: String,
    #[serde(flatten)]
    price: billing::Price,
}

#[derive(Default, Deserialize)]
struct PortalQuery {
    page: Option<u32>,
    start_time: Option<i64>,
    end_time: Option<i64>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/token-portal/me", get(overview))
        .route("/api/token-portal/config", get(portal_config))
        .route("/api/token-portal/auth/send-code", post(send_code))
        .route("/api/token-portal/auth/register", post(register))
        .route("/api/token-portal/auth/login", post(login))
        .route(
            "/api/token-portal/auth/reset-password",
            post(reset_password),
        )
        .route("/api/token-portal/auth/logout", post(logout))
        .route("/api/token-portal/key", get(reveal_key))
        .route("/api/token-portal/key/regenerate", post(regenerate_key))
}

/// Who is looking at the portal.
enum Viewer {
    Key(db::UserToken),
    Account(accounts::PortalUser, db::UserToken),
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn no_store(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::VARY,
        HeaderValue::from_static("Authorization, Cookie"),
    );
    response
}

fn error_response(status: StatusCode, code: &str, retry_after: Option<i64>) -> Response {
    let mut body = serde_json::json!({"error": {"code": code}});
    if let Some(secs) = retry_after {
        body["error"]["retry_after"] = secs.max(1).into();
    }
    no_store((status, Json(body)).into_response())
}

fn portal_error(error: PortalError) -> Response {
    let status = match &error {
        PortalError::InvalidEmail | PortalError::WeakPassword | PortalError::InvalidCode => {
            StatusCode::BAD_REQUEST
        }
        PortalError::InvalidCredentials => StatusCode::UNAUTHORIZED,
        PortalError::AccountDisabled => StatusCode::FORBIDDEN,
        PortalError::AlreadyRegistered => StatusCode::CONFLICT,
        PortalError::CodeCooldown(_) | PortalError::CodeLimit | PortalError::Locked(_) => {
            StatusCode::TOO_MANY_REQUESTS
        }
        PortalError::Unavailable(detail) => {
            tracing::warn!("[Portal] request failed: {detail}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    };
    error_response(status, error.code(), error.retry_after())
}

fn ok_json(value: serde_json::Value) -> Response {
    no_store(Json(value).into_response())
}

/// Runs blocking database / password-hashing work off the async runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PortalError> + Send + 'static,
) -> Result<T, PortalError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| PortalError::Unavailable(e.to_string()))?
}

// --- Client IP and rate limiting ---

struct ClientIp(String);

#[axum::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            crate::proxy::middleware::ip_filter::client_ip_from_parts(
                &parts.headers,
                &parts.extensions,
            )
            .unwrap_or_else(|| "unknown".into()),
        ))
    }
}

/// Fixed-window counters per action and client, in memory. Per-address limits
/// (code cooldown, login lockout) live in the database and survive restarts.
static WINDOWS: once_cell::sync::Lazy<DashMap<String, (i64, u32)>> =
    once_cell::sync::Lazy::new(DashMap::new);

fn rate_limit(action: &str, subject: &str, limit: u32, window: i64) -> Result<(), Response> {
    let now = now();
    if WINDOWS.len() > 50_000 {
        WINDOWS.retain(|_, (start, _)| now - *start < 3600);
    }
    let mut entry = WINDOWS
        .entry(format!("{action}:{subject}"))
        .or_insert((now, 0));
    let (start, count) = entry.value_mut();
    if now - *start >= window {
        *start = now;
        *count = 0;
    }
    if *count >= limit {
        return Err(error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            Some(window - (now - *start)),
        ));
    }
    *count += 1;
    Ok(())
}

// --- Session cookie ---

fn session_secret(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
}

/// `Secure` whenever the browser reached us over HTTPS (directly or via a proxy),
/// so plain-HTTP LAN and localhost setups still work.
fn is_https(headers: &HeaderMap) -> bool {
    let header_is = |name: &str, prefix: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with(prefix))
    };
    header_is("x-forwarded-proto", "https") || header_is(header::ORIGIN.as_str(), "https://")
}

fn session_cookie(headers: &HeaderMap, value: &str, max_age: i64) -> HeaderValue {
    let secure = if is_https(headers) { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; Path={COOKIE_PATH}; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}"
    ))
    .expect("session cookie is ASCII")
}

fn with_cookie(mut response: Response, cookie: HeaderValue) -> Response {
    response.headers_mut().append(header::SET_COOKIE, cookie);
    response
}

async fn signed_in(headers: &HeaderMap, ip: String, user_id: String) -> Response {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let now = now();
    match blocking(move || accounts::create_session(&user_id, &ip, user_agent.as_deref(), now))
        .await
    {
        Ok(secret) => with_cookie(
            ok_json(serde_json::json!({"ok": true})),
            session_cookie(headers, &secret, accounts::SESSION_TTL_SECS),
        ),
        Err(error) => portal_error(error),
    }
}

// --- Authentication ---

fn bearer_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}

/// A Bearer key wins over a cookie so key sign-in behaves exactly as before.
fn authenticate(headers: &HeaderMap) -> Result<Viewer, StatusCode> {
    if let Some(key) = bearer_key(headers) {
        if key.is_empty() || key.len() > 256 {
            return Err(StatusCode::UNAUTHORIZED);
        }
        return db::get_token_by_value(key)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .filter(|t| t.enabled)
            .map(Viewer::Key)
            .ok_or(StatusCode::UNAUTHORIZED);
    }
    session_account(headers).map(|(user, token)| Viewer::Account(user, token))
}

fn session_account(
    headers: &HeaderMap,
) -> Result<(accounts::PortalUser, db::UserToken), StatusCode> {
    let secret = session_secret(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let user = accounts::session_user(&secret, now())
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    // Pending (disabled, quota 0) keys are still visible to their own account.
    let token = db::get_token_by_owner(&user.id)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    Ok((user, token))
}

// --- Usage overview ---

async fn overview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PortalQuery>,
) -> Response {
    let mut result = authenticate(&headers).and_then(|viewer| read_overview(viewer, &query));
    if let Ok(data) = &mut result {
        let only_raw = *state.only_raw_quota_models.read().await;
        let catalog = crate::proxy::common::model_mapping::get_all_dynamic_models(
            &state.custom_mapping,
            Some(&state.token_manager),
            only_raw,
        )
        .await;
        data.available_models = permitted_catalog(catalog, &data.allowed_models);
    }
    render(result.and_then(|mut data| {
        data.model_prices = priced_models(&data.available_models)?;
        Ok(data)
    }))
}

fn priced_models(models: &[String]) -> Result<Vec<PortalModelPrice>, StatusCode> {
    let rules = billing::load_prices().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(models
        .iter()
        .filter_map(|model| {
            billing::match_price(&rules, model).map(|rule| PortalModelPrice {
                model: model.clone(),
                price: rule.price.clone(),
            })
        })
        .collect())
}

fn permitted_catalog(catalog: Vec<String>, allowed: &[String]) -> Vec<String> {
    catalog
        .into_iter()
        .filter(|model| allowed.is_empty() || allowed.contains(model))
        .collect()
}

fn render(result: Result<UsageOverview, StatusCode>) -> Response {
    match result {
        Ok(data) => no_store(Json(data).into_response()),
        Err(status) => error_response(
            status,
            if status == StatusCode::UNAUTHORIZED {
                "invalid_user_token"
            } else if status == StatusCode::BAD_REQUEST {
                "invalid_query"
            } else {
                "usage_unavailable"
            },
            None,
        ),
    }
}

fn read_overview(viewer: Viewer, query: &PortalQuery) -> Result<UsageOverview, StatusCode> {
    let (token, account_email) = match viewer {
        Viewer::Key(token) => (token, None),
        Viewer::Account(user, token) => (token, Some(user.email)),
    };
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
    let now = now();
    let daily = db::period_usage_for_token(&token.id, false, now)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let weekly = db::period_usage_for_token(&token.id, true, now)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // Expired / exhausted keys can still view their own historical usage.
    // Disabled or deleted keys are revoked for key sign-in.
    let models =
        db::model_usage_for_token(&token.id).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let suffix: String = token
        .token
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Ok(UsageOverview {
        token_hint: format!("••••{suffix}"),
        account_email,
        pending_activation: token.owner_user_id.is_some() && !token.enabled,
        expires_at: token.expires_at,
        expired: token.expires_at.is_some_and(|ts| ts <= now),
        max_concurrency: token.max_concurrency,
        token_quota: token.token_quota,
        total_tokens_used: token.total_tokens_used,
        remaining_tokens: token.remaining_tokens(),
        total_requests: token.total_requests,
        balance_micros: token.balance_micros,
        available_models: token.allowed_models.clone(),
        model_prices: Vec::new(),
        username: token.username,
        allowed_models: token.allowed_models,
        logs,
        daily,
        weekly,
        models,
    })
}

// --- Account endpoints ---

async fn portal_config() -> Response {
    match portal_settings::load() {
        Ok(settings) => ok_json(serde_json::json!({
            "registration_enabled": settings.registration_open(),
            "password_reset_enabled": settings.smtp.is_ready(),
        })),
        Err(error) => portal_error(PortalError::Unavailable(error)),
    }
}

#[derive(Deserialize)]
struct SendCodeRequest {
    email: String,
    purpose: String,
}

async fn send_code(ClientIp(ip): ClientIp, Json(body): Json<SendCodeRequest>) -> Response {
    let Some(purpose) = CodePurpose::parse(&body.purpose) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request", None);
    };
    if let Err(limited) = rate_limit("send-code", &ip, 20, 3600) {
        return limited;
    }
    let email = match accounts::normalize_email(&body.email) {
        Ok(email) => email,
        Err(error) => return portal_error(error),
    };
    let settings = match portal_settings::load() {
        Ok(settings) => settings,
        Err(error) => return portal_error(PortalError::Unavailable(error)),
    };
    if !settings.smtp.is_ready() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "mail_unavailable", None);
    }
    if purpose == CodePurpose::Register {
        if !settings.registration_enabled {
            return error_response(StatusCode::FORBIDDEN, "registration_closed", None);
        }
        if !settings.email_allowed(&email) {
            return error_response(StatusCode::FORBIDDEN, "email_not_allowed", None);
        }
    }

    let now = now();
    let address = email.clone();
    let delivery = match blocking(move || accounts::request_code(&address, purpose, now)).await {
        Ok(delivery) => delivery,
        Err(error) => return portal_error(error),
    };
    let smtp = settings.smtp;
    let accepted = ok_json(serde_json::json!({
        "ok": true,
        "resend_after": accounts::CODE_RESEND_SECS,
    }));
    match (purpose, delivery) {
        // Sign-up mail is sent inline so delivery problems surface to the user;
        // both outcomes send a message, so timing does not reveal registration.
        (CodePurpose::Register, CodeDelivery::Code { id, code }) => {
            let mail = portal_mail::code_mail(&smtp, purpose, &code);
            if let Err(error) = portal_mail::send(&smtp, &email, mail).await {
                tracing::warn!("[Portal] verification email failed: {error}");
                let _ = blocking(move || accounts::discard_code(&id)).await;
                return error_response(StatusCode::SERVICE_UNAVAILABLE, "mail_unavailable", None);
            }
            accepted
        }
        (CodePurpose::Register, CodeDelivery::AlreadyRegistered) => {
            let mail = portal_mail::already_registered_mail(&smtp);
            if let Err(error) = portal_mail::send(&smtp, &email, mail).await {
                tracing::warn!("[Portal] account notice email failed: {error}");
            }
            accepted
        }
        // Reset mail is sent in the background so the response time is the same
        // for registered and unknown addresses.
        (CodePurpose::Reset, CodeDelivery::Code { id, code }) => {
            tokio::spawn(async move {
                let mail = portal_mail::code_mail(&smtp, purpose, &code);
                if let Err(error) = portal_mail::send(&smtp, &email, mail).await {
                    tracing::warn!("[Portal] password reset email failed: {error}");
                    let _ = blocking(move || accounts::discard_code(&id)).await;
                }
            });
            accepted
        }
        _ => accepted,
    }
}

#[derive(Deserialize)]
struct RegisterRequest {
    email: String,
    code: String,
    password: String,
}

async fn register(
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> Response {
    if let Err(limited) = rate_limit("register", &ip, 20, 3600) {
        return limited;
    }
    let email = match accounts::normalize_email(&body.email) {
        Ok(email) => email,
        Err(error) => return portal_error(error),
    };
    let settings = match portal_settings::load() {
        Ok(settings) => settings,
        Err(error) => return portal_error(PortalError::Unavailable(error)),
    };
    if !settings.registration_open() {
        return error_response(StatusCode::FORBIDDEN, "registration_closed", None);
    }
    if !settings.email_allowed(&email) {
        return error_response(StatusCode::FORBIDDEN, "email_not_allowed", None);
    }
    let defaults = accounts::RegistrationDefaults {
        max_concurrency: settings.default_max_concurrency,
        allowed_models: settings.default_allowed_models,
        expires_type: settings.default_expires_type,
    };
    let now = now();
    match blocking(move || accounts::register(&email, &body.code, &body.password, defaults, now))
        .await
    {
        Ok((user, _)) => signed_in(&headers, ip, user.id).await,
        Err(error) => portal_error(error),
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

async fn login(
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    if let Err(limited) = rate_limit("login", &ip, 30, 900) {
        return limited;
    }
    let Ok(email) = accounts::normalize_email(&body.email) else {
        return portal_error(PortalError::InvalidCredentials);
    };
    let now = now();
    match blocking(move || accounts::login(&email, &body.password, now)).await {
        Ok(user) => signed_in(&headers, ip, user.id).await,
        Err(error) => portal_error(error),
    }
}

#[derive(Deserialize)]
struct ResetRequest {
    email: String,
    code: String,
    password: String,
}

async fn reset_password(ClientIp(ip): ClientIp, Json(body): Json<ResetRequest>) -> Response {
    if let Err(limited) = rate_limit("reset", &ip, 20, 3600) {
        return limited;
    }
    let email = match accounts::normalize_email(&body.email) {
        Ok(email) => email,
        Err(error) => return portal_error(error),
    };
    let now = now();
    match blocking(move || accounts::reset_password(&email, &body.code, &body.password, now)).await
    {
        Ok(()) => ok_json(serde_json::json!({"ok": true})),
        Err(error) => portal_error(error),
    }
}

async fn logout(headers: HeaderMap) -> Response {
    if let Some(secret) = session_secret(&headers) {
        if let Err(error) = blocking(move || accounts::delete_session(&secret)).await {
            return portal_error(error);
        }
    }
    with_cookie(
        ok_json(serde_json::json!({"ok": true})),
        session_cookie(&headers, "", 0),
    )
}

async fn reveal_key(headers: HeaderMap) -> Response {
    match session_account(&headers) {
        Ok((_, token)) => ok_json(serde_json::json!({ "token": token.token })),
        Err(status) => error_response(status, "not_signed_in", None),
    }
}

async fn regenerate_key(headers: HeaderMap) -> Response {
    let user = match session_account(&headers) {
        Ok((user, _)) => user,
        Err(status) => return error_response(status, "not_signed_in", None),
    };
    if let Err(limited) = rate_limit("regenerate", &user.id, 10, 3600) {
        return limited;
    }
    let now = now();
    match blocking(move || accounts::regenerate_key(&user.id, now)).await {
        Ok(token) => ok_json(serde_json::json!({ "token": token })),
        Err(error) => portal_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn user_token_catalog_restricts_permissions_without_reintroducing_stale_ids() {
        let catalog = vec!["upstream-a".into(), "upstream-b".into()];
        assert_eq!(permitted_catalog(catalog.clone(), &[]), catalog);
        assert_eq!(
            permitted_catalog(catalog.clone(), &["upstream-b".into(), "old-alias".into()]),
            vec!["upstream-b"]
        );
        assert!(permitted_catalog(catalog, &["old-alias".into()]).is_empty());
        assert!(permitted_catalog(vec![], &["old-alias".into()]).is_empty());
    }
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    fn test_routes() -> Router {
        Router::new()
            .route(
                "/api/token-portal/me",
                get(
                    |headers: HeaderMap, Query(query): Query<PortalQuery>| async move {
                        render(authenticate(&headers).and_then(|v| read_overview(v, &query)))
                    },
                ),
            )
            .route("/api/token-portal/auth/logout", post(logout))
            .route("/api/token-portal/key", get(reveal_key))
            .route("/api/token-portal/key/regenerate", post(regenerate_key))
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
        assert_eq!(v["pending_activation"], false);
        assert!(v["account_email"].is_null());
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

    async fn call(
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        bearer: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(secret) = cookie {
            builder = builder.header(
                header::COOKIE,
                format!("other=1; {SESSION_COOKIE}={secret}"),
            );
        }
        if let Some(key) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = test_routes()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn portal_account_session_sees_pending_key_and_manages_it() {
        db::init_db().unwrap();
        let now = now();
        let email = format!("portal-{}@example.com", uuid::Uuid::new_v4().simple());
        let code = match accounts::request_code(&email, CodePurpose::Register, now).unwrap() {
            CodeDelivery::Code { code, .. } => code,
            other => panic!("{other:?}"),
        };
        let defaults = accounts::RegistrationDefaults {
            max_concurrency: 0,
            allowed_models: vec![],
            expires_type: "never".into(),
        };
        let (user, token) =
            accounts::register(&email, &code, "correct horse", defaults, now).unwrap();
        let secret = accounts::create_session(&user.id, "ip", None, now).unwrap();

        // Account session sees its pending key; the key itself cannot sign in yet.
        let (status, me) = call("GET", "/api/token-portal/me", Some(&secret), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(me["account_email"], email.as_str());
        assert_eq!(me["pending_activation"], true);
        assert_eq!(me["token_quota"], 0);
        // Registered keys are billed from a balance instead of a token quota.
        assert_eq!(me["balance_micros"], 0);
        assert!(me["remaining_tokens"].is_null());
        let (status, _) = call("GET", "/api/token-portal/me", None, Some(&token.token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, revealed) = call("GET", "/api/token-portal/key", Some(&secret), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(revealed["token"], token.token.as_str());
        let (_, rotated) = call(
            "POST",
            "/api/token-portal/key/regenerate",
            Some(&secret),
            None,
        )
        .await;
        let rotated = rotated["token"].as_str().unwrap().to_string();
        assert_ne!(rotated, token.token);
        // A key alone never reveals or rotates the key.
        let (status, _) = call("GET", "/api/token-portal/key", None, Some(&rotated)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = call("POST", "/api/token-portal/auth/logout", Some(&secret), None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call("GET", "/api/token-portal/me", Some(&secret), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn portal_cookie_is_scoped_and_secure_behind_https() {
        let mut headers = HeaderMap::new();
        let plain = session_cookie(&headers, "abc", 60);
        let plain = plain.to_str().unwrap();
        assert!(plain.contains("HttpOnly") && plain.contains("SameSite=Strict"));
        assert!(plain.contains("Path=/api/token-portal") && !plain.contains("Secure"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(session_cookie(&headers, "abc", 60)
            .to_str()
            .unwrap()
            .ends_with("; Secure"));
    }
}
