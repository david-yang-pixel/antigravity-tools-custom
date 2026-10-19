//! User usage portal. Visitors sign in either with an API key (Bearer) or with an
//! email account (session cookie). Ownership always derives from the credential;
//! no caller-supplied token ID is ever used and no administrator DTO is returned.
use crate::modules::portal_account::{self as accounts, CodeDelivery, CodePurpose, PortalError};
use crate::modules::{billing, epay, payment, portal_mail, portal_settings, user_token_db as db};
use crate::proxy::server::AppState;
use axum::extract::{FromRequestParts, Path, Query, State};
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
    expires_at: Option<i64>,
    expired: bool,
    max_concurrency: i32,
    total_tokens_used: i64,
    total_requests: i64,
    /// Prepaid balance in micro-yuan; every key starts with a zero balance.
    balance_micros: Option<i64>,
    free_models: Vec<String>,
    available_models: Vec<String>,
    /// Prices for available paid models; explicit free-model grants take precedence.
    model_prices: Vec<PortalModelPrice>,
    /// Most recent balance top-ups, newest first.
    topup_orders: Vec<payment::PublicOrder>,
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
        .route("/api/token-portal/topup", post(create_topup))
        .route("/api/token-portal/topup/orders", get(topup_orders))
        .route("/api/token-portal/topup/orders/:order", get(topup_order))
        .route(
            "/api/token-portal/topup/orders/:order/pay",
            post(resume_topup),
        )
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
    // Account sessions retain access to their own key's history when the key is disabled.
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
    let mut data = match authenticate(&headers).and_then(|viewer| read_overview(viewer, &query)) {
        Ok(data) => data,
        Err(status) => return render(Err(status)),
    };
    let only_raw = *state.only_raw_quota_models.read().await;
    let catalog = crate::proxy::common::model_mapping::get_all_dynamic_models(
        &state.custom_mapping,
        Some(&state.token_manager),
        only_raw,
    )
    .await;
    // Use one pricing snapshot for visibility and displayed prices.
    let rules = match billing::load_prices() {
        Ok(rules) => rules,
        Err(_) => return render(Err(StatusCode::SERVICE_UNAVAILABLE)),
    };
    data.available_models = permitted_catalog(catalog, &data.free_models, &rules);
    data.model_prices = priced_models(&data.available_models, &data.free_models, &rules);
    render(Ok(data))
}

fn priced_models(
    models: &[String],
    free_models: &[String],
    rules: &[billing::ModelPrice],
) -> Vec<PortalModelPrice> {
    models
        .iter()
        .filter_map(
            |model| match billing::policy_for_model(free_models, rules, model) {
                Some(billing::BillingPolicy::Priced(price)) => Some(PortalModelPrice {
                    model: model.clone(),
                    price,
                }),
                _ => None,
            },
        )
        .collect()
}

fn permitted_catalog(
    catalog: Vec<String>,
    free_models: &[String],
    rules: &[billing::ModelPrice],
) -> Vec<String> {
    catalog
        .into_iter()
        .filter(|model| billing::policy_for_model(free_models, rules, model).is_some())
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
    // Expired keys and keys with no balance can still view their own historical usage.
    // Disabled or deleted keys are revoked for key sign-in.
    let models =
        db::model_usage_for_token(&token.id).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let topup_orders = payment::recent_orders(&token.id, 10)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .into_iter()
        .map(|order| order.for_display(now))
        .collect();
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
        expires_at: token.expires_at,
        expired: token.expires_at.is_some_and(|ts| ts <= now),
        max_concurrency: token.max_concurrency,
        total_tokens_used: token.total_tokens_used,
        total_requests: token.total_requests,
        balance_micros: Some(token.balance_micros.unwrap_or(0)),
        available_models: Vec::new(),
        model_prices: Vec::new(),
        topup_orders,
        username: token.username,
        free_models: token.free_models,
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
            "topup": {
                "enabled": settings.payment.is_ready(),
                "pay_types": settings.payment.pay_types,
                "min_amount_cents": settings.payment.min_amount_cents,
                "max_amount_cents": settings.payment.max_amount_cents,
            },
        })),
        Err(error) => portal_error(PortalError::Unavailable(error)),
    }
}

// --- Balance top-ups ---

#[derive(Deserialize)]
struct TopupRequest {
    /// Yuan with at most two decimals, e.g. "12.50".
    amount: String,
    pay_type: String,
}

/// Creates a pending order and returns the signed cashier form the browser
/// submits to the gateway to add to the key's balance.
async fn create_topup(
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Json(body): Json<TopupRequest>,
) -> Response {
    if let Err(limited) = rate_limit("topup", &ip, 30, 3600) {
        return limited;
    }
    let (token, user_id) = match authenticate(&headers) {
        Ok(Viewer::Key(token)) => (token, None),
        Ok(Viewer::Account(user, token)) => (token, Some(user.id)),
        Err(status) => return error_response(status, "invalid_user_token", None),
    };
    // The balance of an expired key cannot be spent until an administrator renews it.
    if token.expires_at.is_some_and(|ts| ts <= now()) {
        return error_response(StatusCode::BAD_REQUEST, "topup_key_expired", None);
    }
    let payment_settings = match portal_settings::load() {
        Ok(settings) => settings.payment,
        Err(error) => return portal_error(PortalError::Unavailable(error)),
    };
    if !payment_settings.is_ready() {
        return error_response(StatusCode::BAD_REQUEST, "topup_disabled", None);
    }
    if !payment_settings.pay_types.contains(&body.pay_type) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_pay_type", None);
    }
    let Some(cents) = epay::parse_money(&body.amount) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_amount", None);
    };
    if cents < payment_settings.min_amount_cents || cents > payment_settings.max_amount_cents {
        return error_response(StatusCode::BAD_REQUEST, "amount_out_of_range", None);
    }
    if let Err(limited) = rate_limit("topup-key", &token.id, 20, 3600) {
        return limited;
    }
    let pending = {
        let token_id = token.id.clone();
        blocking(move || {
            payment::pending_count(&token_id, now() - payment::ORDER_TTL_SECS)
                .map_err(PortalError::Unavailable)
        })
        .await
    };
    match pending {
        Ok(count) if count >= payment::MAX_PENDING_PER_KEY => {
            return error_response(StatusCode::TOO_MANY_REQUESTS, "too_many_pending", None)
        }
        Ok(_) => {}
        Err(error) => return portal_error(error),
    }
    let token_id = token.id.clone();
    let pay_type = body.pay_type.clone();
    let order = match blocking(move || {
        payment::create_order(&token_id, user_id.as_deref(), cents, &pay_type, now())
            .map_err(PortalError::Unavailable)
    })
    .await
    {
        Ok(order) => order,
        Err(error) => return portal_error(error),
    };
    cashier_response(&payment_settings, order, now())
}

/// Always rebuild the form from the stored order, never from browser-supplied
/// amounts or methods. Both new payments and resumed payments use this gate.
fn cashier_response(
    settings: &portal_settings::PaymentSettings,
    order: payment::PublicOrder,
    now: i64,
) -> Response {
    if order.status == "paid" {
        return order_response(order, now);
    }
    if !order.can_pay(now) {
        return error_response(StatusCode::GONE, "order_expired", None);
    }
    let Some(merchant) = settings.merchant().filter(|_| settings.is_ready()) else {
        return error_response(StatusCode::BAD_REQUEST, "topup_disabled", None);
    };
    if !settings.pay_types.contains(&order.pay_type) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_pay_type", None);
    }
    let name = format!(
        "{} {} 元",
        settings.subject,
        epay::format_money(order.amount_cents)
    );
    let (action, fields) = epay::submit_form(
        &merchant,
        &order.pay_type,
        &order.out_trade_no,
        &settings.notify_url(),
        &settings.return_url(),
        &name,
        order.amount_cents,
    );
    ok_json(serde_json::json!({
        "order": order,
        "action": action,
        "fields": fields,
        "server_time": now,
    }))
}

fn order_response(order: payment::PublicOrder, now: i64) -> Response {
    ok_json(serde_json::json!({ "order": order.for_display(now), "server_time": now }))
}

#[derive(Default, Deserialize)]
struct TopupOrdersQuery {
    #[serde(default)]
    status: payment::OrderFilter,
    page: Option<u32>,
    page_size: Option<u32>,
}

async fn topup_orders(headers: HeaderMap, Query(query): Query<TopupOrdersQuery>) -> Response {
    let token = match authenticate(&headers) {
        Ok(Viewer::Key(token) | Viewer::Account(_, token)) => token,
        Err(status) => return error_response(status, "invalid_user_token", None),
    };
    match blocking(move || {
        payment::order_page(
            &token.id,
            query.status,
            query.page.unwrap_or(1),
            query.page_size.unwrap_or(10),
            now(),
        )
        .map_err(PortalError::Unavailable)
    })
    .await
    {
        Ok(page) => ok_json(serde_json::to_value(page).expect("order page is serializable")),
        Err(error) => portal_error(error),
    }
}

/// Restores the same order's cashier form, after checking ownership and asking
/// the gateway whether it was paid while the user was away. It never inserts an
/// order or extends the original deadline.
async fn resume_topup(headers: HeaderMap, Path(order_no): Path<String>) -> Response {
    let token = match authenticate(&headers) {
        Ok(Viewer::Key(token) | Viewer::Account(_, token)) => token,
        Err(status) => return error_response(status, "invalid_user_token", None),
    };
    if order_no.is_empty() || order_no.len() > 32 || !order_no.bytes().all(|b| b.is_ascii_digit()) {
        return error_response(StatusCode::NOT_FOUND, "order_not_found", None);
    }
    let lookup = {
        let (token_id, order_no) = (token.id.clone(), order_no.clone());
        move || payment::order_for_token(&token_id, &order_no).map_err(PortalError::Unavailable)
    };
    let order = match blocking(lookup).await {
        Ok(Some(order)) => order,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "order_not_found", None),
        Err(error) => return portal_error(error),
    };
    if order.status == "paid" {
        return order_response(order, now());
    }
    if !order.can_pay(now()) {
        return error_response(StatusCode::GONE, "order_expired", None);
    }
    if token.expires_at.is_some_and(|ts| ts <= now()) {
        return error_response(StatusCode::BAD_REQUEST, "topup_key_expired", None);
    }
    if let Err(limited) = rate_limit("topup-resume", &token.id, 30, 600) {
        return limited;
    }
    let settings =
        match blocking(|| portal_settings::load().map_err(PortalError::Unavailable)).await {
            Ok(settings) => settings.payment,
            Err(error) => return portal_error(error),
        };
    resume_with_settings(&token.id, order, &settings).await
}

async fn resume_with_settings(
    token_id: &str,
    order: payment::PublicOrder,
    settings: &portal_settings::PaymentSettings,
) -> Response {
    let Some(merchant) = settings.merchant().filter(|_| settings.is_ready()) else {
        return error_response(StatusCode::BAD_REQUEST, "topup_disabled", None);
    };
    if !settings.pay_types.contains(&order.pay_type) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_pay_type", None);
    }
    let client = match epay::http_client() {
        Ok(client) => client,
        Err(_) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "payment_check_failed",
                None,
            )
        }
    };
    if crate::proxy::handlers::payment::reconcile_order(&client, &merchant, &order.out_trade_no)
        .await
        .is_err()
    {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "payment_check_failed",
            None,
        );
    }
    // A callback can settle the order during the network request. Reload it and
    // check the clock again before issuing a signed form.
    let token_id = token_id.to_string();
    match blocking(move || {
        payment::order_for_token(&token_id, &order.out_trade_no).map_err(PortalError::Unavailable)
    })
    .await
    {
        Ok(Some(order)) => cashier_response(settings, order, now()),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "order_not_found", None),
        Err(error) => portal_error(error),
    }
}

/// Status of one of the viewer's orders. An unpaid order is checked with the
/// gateway directly, so the return page confirms even when the notification is
/// slow or cannot reach this site.
async fn topup_order(headers: HeaderMap, Path(order_no): Path<String>) -> Response {
    let token = match authenticate(&headers) {
        Ok(Viewer::Key(token) | Viewer::Account(_, token)) => token,
        Err(status) => return error_response(status, "invalid_user_token", None),
    };
    if order_no.is_empty() || order_no.len() > 32 || !order_no.bytes().all(|b| b.is_ascii_digit()) {
        return error_response(StatusCode::NOT_FOUND, "order_not_found", None);
    }
    let lookup = {
        let (token_id, order_no) = (token.id.clone(), order_no.clone());
        move || payment::order_for_token(&token_id, &order_no).map_err(PortalError::Unavailable)
    };
    let order = match blocking(lookup.clone()).await {
        Ok(Some(order)) => order,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "order_not_found", None),
        Err(error) => return portal_error(error),
    };
    // Closed orders are checked too, so a payment that arrived after closing can
    // still be credited when its owner looks it up.
    if order.status == "paid"
        || now() - order.created_at < 5
        || rate_limit("topup-check", &order_no, 30, 600).is_err()
        || rate_limit("topup-check-key", &token.id, 60, 600).is_err()
    {
        return order_response(order, now());
    }
    let merchant = portal_settings::load()
        .ok()
        .and_then(|s| s.payment.merchant());
    if let (Some(merchant), Ok(client)) = (merchant, epay::http_client()) {
        if let Err(error) =
            crate::proxy::handlers::payment::reconcile_order(&client, &merchant, &order_no).await
        {
            tracing::debug!("[Payment] on-demand check of {order_no}: {error}");
        }
    }
    match blocking(lookup).await {
        Ok(Some(order)) => order_response(order, now()),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "order_not_found", None),
        Err(error) => portal_error(error),
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
        allowed_models: Vec::new(),
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
    use crate::proxy::monitor::prompt_log_tests::TestDataDir;

    #[test]
    fn user_token_portal_catalog_requires_explicit_free_grant_or_price() {
        let catalog = vec![
            "upstream-a".into(),
            "upstream-b".into(),
            "free-model".into(),
            "Free-Model".into(),
            "unpriced".into(),
        ];
        let rules = vec![billing::ModelPrice {
            pattern: "upstream-*".into(),
            price: billing::Price::PerCall {
                call_price_micros: 1000,
            },
        }];
        let free = vec!["models/free-model".into(), "old-alias".into()];
        assert_eq!(
            permitted_catalog(catalog.clone(), &free, &rules),
            vec!["upstream-a", "upstream-b", "free-model"]
        );
        assert_eq!(
            permitted_catalog(catalog.clone(), &[], &rules),
            vec!["upstream-a", "upstream-b"]
        );
        assert_eq!(
            permitted_catalog(catalog.clone(), &free, &[]),
            vec!["free-model"]
        );
        assert!(permitted_catalog(catalog, &[], &[]).is_empty());
        assert!(permitted_catalog(vec![], &free, &rules).is_empty());
        assert_eq!(
            permitted_catalog(
                vec!["models/free-model".into()],
                &["free-model".into()],
                &[]
            ),
            vec!["models/free-model"]
        );
    }

    #[test]
    fn user_token_portal_prices_exclude_free_grants_and_use_specific_price() {
        let rules = vec![
            billing::ModelPrice {
                pattern: "model-*".into(),
                price: billing::Price::PerCall {
                    call_price_micros: 1000,
                },
            },
            billing::ModelPrice {
                pattern: "model-paid".into(),
                price: billing::Price::PerToken {
                    input_price_micros: 2_000_000,
                    output_price_micros: 8_000_000,
                    cached_price_micros: Some(500_000),
                },
            },
        ];
        let models = vec![
            "model-free".into(),
            "model-paid".into(),
            "model-other".into(),
            "unpriced".into(),
        ];
        let prices = priced_models(&models, &["model-free".into()], &rules);
        assert_eq!(prices.len(), 2);
        assert_eq!(prices[0].model, "model-paid");
        assert_eq!(prices[0].price, rules[1].price);
        assert_eq!(prices[1].model, "model-other");
        assert_eq!(prices[1].price, rules[0].price);
        let dto = serde_json::to_value(prices).unwrap();
        assert_eq!(dto[0]["mode"], "per_token");
        assert_eq!(dto[0]["cached_price_micros"], 500_000);
        assert_eq!(dto[1]["call_price_micros"], 1000);
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
            .route("/api/token-portal/topup/orders", get(topup_orders))
            .route("/api/token-portal/topup/orders/:order", get(topup_order))
            .route(
                "/api/token-portal/topup/orders/:order/pay",
                post(resume_topup),
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
            0,
            Vec::new(),
        )
        .unwrap()
    }
    fn grant_free_models(token: &db::UserToken, models: &[&str]) {
        db::update_token_with_policy(
            &token.id,
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
            None,
            Some(models.iter().map(|model| (*model).to_owned()).collect()),
        )
        .unwrap();
    }
    #[tokio::test]
    async fn user_token_portal_only_returns_authenticated_keys_aggregates() {
        let _dir = TestDataDir::new();
        let a = key("portal-a");
        let b = key("portal-b");
        grant_free_models(&a, &["model-a", "model-b"]);
        grant_free_models(&b, &["other-model"]);
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
        assert_eq!(v["balance_micros"], 0);
        assert_eq!(v["free_models"], serde_json::json!(["model-a", "model-b"]));
        for obsolete in [
            "token_quota",
            "remaining_tokens",
            "allowed_models",
            "pending_activation",
        ] {
            assert!(v.get(obsolete).is_none());
        }
        assert!(v["account_email"].is_null());
        assert_eq!(v["models"].as_array().unwrap().len(), 2);
        assert_eq!(v["models"][0]["input_tokens"], 30);
        assert_eq!(v["models"][1]["failed_requests"], 1);
    }
    #[tokio::test]
    async fn user_token_portal_validates_time_filters() {
        let _dir = TestDataDir::new();
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
    async fn user_token_portal_zero_balance_history_and_revocation() {
        let _dir = TestDataDir::new();
        let a = key("revoked");
        grant_free_models(&a, &["model-a"]);
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
    async fn payment_portal_history_and_resume_enforce_ownership_and_deadlines() {
        let _dir = TestDataDir::new();
        let a = key("payment-owner-a");
        let b = key("payment-owner-b");
        let order = payment::create_order(&b.id, None, 100, "alipay", now()).unwrap();
        let expired =
            payment::create_order(&a.id, None, 100, "alipay", now() - payment::ORDER_TTL_SECS)
                .unwrap();
        let path = format!("/api/token-portal/topup/orders/{}/pay", order.out_trade_no);
        assert_eq!(
            call("POST", &path, None, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call("POST", &path, None, Some(&a.token)).await.0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = call(
            "POST",
            &format!(
                "/api/token-portal/topup/orders/{}/pay",
                expired.out_trade_no
            ),
            None,
            Some(&a.token),
        )
        .await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(body["error"]["code"], "order_expired");
        let (status, body) = call(
            "GET",
            &format!(
                "/api/token-portal/topup/orders?status=all&token_id={}",
                b.id
            ),
            None,
            Some(&a.token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 1);
        assert_eq!(body["orders"][0]["out_trade_no"], expired.out_trade_no);
        assert_eq!(body["orders"][0]["status"], "closed");
        assert_eq!(body["pending_orders"], serde_json::json!([]));
        assert!(body.get("server_time").unwrap().is_i64());
        let text = body.to_string();
        for forbidden in [&a.token, &b.token, &a.id, &b.id, &order.out_trade_no] {
            assert!(!text.contains(forbidden));
        }
        for query in ["status=bogus", "page=-1", "page_size=99999999999"] {
            assert_eq!(
                call(
                    "GET",
                    &format!("/api/token-portal/topup/orders?{query}"),
                    None,
                    Some(&a.token)
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        // A paid order never yields another signed payment form, even after expiry.
        payment::credit_paid_order(&expired.out_trade_no, None, 100, now()).unwrap();
        let (status, body) = call(
            "POST",
            &format!(
                "/api/token-portal/topup/orders/{}/pay",
                expired.out_trade_no
            ),
            None,
            Some(&a.token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["order"]["status"], "paid");
        assert!(body.get("fields").is_none());
    }

    async fn payment_gateway(
        body: serde_json::Value,
    ) -> (
        portal_settings::PaymentSettings,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/api.php",
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            portal_settings::PaymentSettings {
                enabled: true,
                gateway: format!("http://{addr}"),
                pid: "testmerchant".into(),
                key: "test-only-key".into(),
                public_url: "http://localhost:1420".into(),
                ..Default::default()
            },
            task,
        )
    }

    async fn payment_json(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn payment_resume_reuses_order_and_refuses_unverified_or_paid_checkout() {
        let _dir = TestDataDir::new();
        let owner = key("payment-resume");
        let order = payment::create_order(&owner.id, None, 125, "alipay", now()).unwrap();
        let (settings, gateway) =
            payment_gateway(serde_json::json!({"code": 1, "status": 0})).await;
        let (status, body) =
            payment_json(resume_with_settings(&owner.id, order.clone(), &settings).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["fields"]["out_trade_no"], order.out_trade_no);
        assert_eq!(body["fields"]["money"], "1.25");
        assert_eq!(body["fields"]["type"], "alipay");
        assert_eq!(body["order"]["expires_at"], order.expires_at);
        assert!(!body.to_string().contains("test-only-key"));
        let fields: std::collections::BTreeMap<String, String> =
            serde_json::from_value(body["fields"].clone()).unwrap();
        assert!(epay::verify(&fields, &settings.key));
        assert_eq!(payment::recent_orders(&owner.id, 10).unwrap().len(), 1);
        gateway.abort();

        // A navigation/connection interruption can leave a local order that
        // never reached the gateway. Explicit absence permits the same checkout.
        let (settings, gateway) =
            payment_gateway(serde_json::json!({"code": 0, "msg": "订单号不存在"})).await;
        let (status, body) =
            payment_json(resume_with_settings(&owner.id, order.clone(), &settings).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["fields"]["out_trade_no"], order.out_trade_no);
        assert_eq!(body["order"]["expires_at"], order.expires_at);
        assert_eq!(payment::recent_orders(&owner.id, 10).unwrap().len(), 1);
        gateway.abort();

        let (settings, gateway) =
            payment_gateway(serde_json::json!({"code": -1, "msg": "unavailable"})).await;
        let (status, body) =
            payment_json(resume_with_settings(&owner.id, order.clone(), &settings).await).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "payment_check_failed");
        assert!(body.get("fields").is_none());
        gateway.abort();

        // The gateway knows it was paid but its notification hasn't arrived yet.
        let (settings, gateway) = payment_gateway(
            serde_json::json!({"code": 1, "status": 1, "money": "1.25", "pid": "testmerchant"}),
        )
        .await;
        let (status, body) =
            payment_json(resume_with_settings(&owner.id, order.clone(), &settings).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["order"]["status"], "paid");
        assert!(body.get("fields").is_none());
        assert_eq!(
            db::get_token_by_id(&owner.id)
                .unwrap()
                .unwrap()
                .balance_micros,
            Some(1_250_000)
        );
        let (_, body) = payment_json(resume_with_settings(&owner.id, order, &settings).await).await;
        assert_eq!(body["order"]["status"], "paid");
        assert_eq!(
            db::get_token_by_id(&owner.id)
                .unwrap()
                .unwrap()
                .balance_micros,
            Some(1_250_000)
        );
        gateway.abort();
    }

    #[tokio::test]
    async fn portal_account_session_manages_enabled_zero_balance_key() {
        let _dir = TestDataDir::new();
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

        // A newly registered active account can use either sign-in method at zero balance.
        let (status, me) = call("GET", "/api/token-portal/me", Some(&secret), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(me["account_email"], email.as_str());
        assert_eq!(me["balance_micros"], 0);
        assert_eq!(me["free_models"], serde_json::json!([]));
        assert!(token.enabled);
        for obsolete in [
            "token_quota",
            "remaining_tokens",
            "allowed_models",
            "pending_activation",
        ] {
            assert!(me.get(obsolete).is_none());
        }
        let (status, _) = call("GET", "/api/token-portal/me", None, Some(&token.token)).await;
        assert_eq!(status, StatusCode::OK);

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
