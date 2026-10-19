//! Payment gateway callbacks and order reconciliation for balance top-ups.
//!
//! Balances are credited only from a signed gateway notification or from the
//! gateway's own order query; the browser's return page never credits anything.

use crate::modules::epay::{self, Merchant};
use crate::modules::payment::{self, Credit, CreditError};
use crate::modules::portal_settings::{self, PaymentSettings};
use crate::proxy::server::AppState;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, RawQuery},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const NOTIFY_PATH: &str = "/api/pay/epay/notify";
/// Orders younger than this are checked every reconciliation pass.
const RECENT_SECS: i64 = 2 * 3600;
/// Older pending orders are checked every Nth pass, until closed or given up.
const OLD_ORDER_EVERY: u64 = 15;
const PASS_LIMIT: u32 = 50;
/// A genuine notification is well under 1 KB with about ten parameters.
const NOTIFY_MAX_BYTES: usize = 16 * 1024;
const NOTIFY_MAX_PARAMS: usize = 64;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(NOTIFY_PATH, get(notify).post(notify))
        // Public and unauthenticated: never buffer more than a notification needs.
        .layer(DefaultBodyLimit::max(NOTIFY_MAX_BYTES))
}

/// The gateway retries until a body containing `success` comes back.
fn reply(acknowledged: bool) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        if acknowledged { "success" } else { "fail" },
    )
        .into_response()
}

/// Notifications are documented as GET; a form-encoded POST is accepted too.
async fn notify(RawQuery(query): RawQuery, body: Bytes) -> Response {
    let query = query.unwrap_or_default();
    if query.len() + body.len() > NOTIFY_MAX_BYTES {
        return reply(false);
    }
    let mut params = BTreeMap::new();
    for source in [query.as_bytes(), &body[..]] {
        for (name, value) in url::form_urlencoded::parse(source) {
            if params.len() >= NOTIFY_MAX_PARAMS {
                return reply(false);
            }
            params.insert(name.into_owned(), value.into_owned());
        }
    }
    let acknowledged = tokio::task::spawn_blocking(move || match portal_settings::load() {
        Ok(settings) => handle_notify(&settings.payment, &params, now()),
        Err(error) => {
            tracing::error!("[Payment] notify: settings unavailable: {error}");
            false
        }
    })
    .await
    .unwrap_or(false);
    reply(acknowledged)
}

/// Returns whether to acknowledge. Unverifiable or retryable notifications are
/// refused so the gateway tries again; settled or irrelevant ones are acknowledged.
fn handle_notify(payment: &PaymentSettings, params: &BTreeMap<String, String>, now: i64) -> bool {
    let Some(merchant) = payment.merchant() else {
        tracing::warn!("[Payment] notify received but no merchant credentials are configured");
        return false;
    };
    if !epay::verify(params, &merchant.key) {
        tracing::warn!("[Payment] notify rejected: invalid signature");
        return false;
    }
    if params.get("pid") != Some(&merchant.pid) {
        tracing::warn!("[Payment] notify rejected: merchant ID mismatch");
        return false;
    }
    let Some(out_trade_no) = params.get("out_trade_no").filter(|v| !v.is_empty()) else {
        tracing::warn!("[Payment] signed notify without an order number ignored");
        return true;
    };
    if params.get("trade_status").map(String::as_str) != Some(epay::PAID_STATUS) {
        tracing::info!("[Payment] order {out_trade_no} notify without payment ignored");
        return true;
    }
    let Some(paid_cents) = params.get("money").and_then(|m| epay::parse_money(m)) else {
        tracing::error!("[Payment] order {out_trade_no} notify has an unreadable amount");
        return false;
    };
    let trade_no = params
        .get("trade_no")
        .map(String::as_str)
        .filter(|v| !v.is_empty());
    settle(out_trade_no, trade_no, paid_cents, now, "notify")
}

/// Credits a payment the gateway has vouched for. Returns whether it is settled.
fn settle(
    out_trade_no: &str,
    trade_no: Option<&str>,
    paid_cents: i64,
    now: i64,
    via: &str,
) -> bool {
    match payment::credit_paid_order(out_trade_no, trade_no, paid_cents, now) {
        Ok(Credit::Credited { credit_micros, .. }) => {
            tracing::info!(
                "[Payment] order {out_trade_no} paid ({via}): credited {} yuan",
                epay::format_money(credit_micros / payment::MICROS_PER_CENT)
            );
            true
        }
        Ok(Credit::AlreadyPaid) => true,
        Err(CreditError::UnknownOrder) => {
            tracing::warn!("[Payment] payment for unknown order {out_trade_no} ignored ({via})");
            true
        }
        // Amount mismatches and database errors stay unacknowledged: the gateway
        // retries and the error log shows them.
        Err(error) => {
            tracing::error!("[Payment] order {out_trade_no} not credited ({via}): {error}");
            false
        }
    }
}

/// Asks the gateway about one order and credits it when paid. `Ok(false)`
/// means the gateway reports it unpaid; a payment that could not be credited
/// is an error, so the order is never closed as unpaid.
pub async fn reconcile_order(
    client: &reqwest::Client,
    merchant: &Merchant,
    out_trade_no: &str,
) -> Result<bool, String> {
    let remote = epay::query_order(client, merchant, out_trade_no).await?;
    if !remote.paid {
        return Ok(false);
    }
    if remote.pid.as_deref().is_some_and(|pid| pid != merchant.pid) {
        return Err("order query answered for another merchant".into());
    }
    let paid_cents = remote
        .amount_cents
        .ok_or("order query reported a payment without an amount")?;
    let out_trade_no = out_trade_no.to_string();
    tokio::task::spawn_blocking(move || {
        settle(
            &out_trade_no,
            remote.trade_no.as_deref(),
            paid_cents,
            now(),
            "query",
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .then_some(true)
    .ok_or_else(|| "paid order could not be credited".to_string())
}

async fn reconcile_pass(pass: u64) -> Result<(), String> {
    let now = now();
    let payment = tokio::task::spawn_blocking(portal_settings::load)
        .await
        .map_err(|e| e.to_string())??
        .payment;
    // Pending orders are reconciled even while new top-ups are switched off.
    // Without credentials nothing is closed either: closing is only decided on
    // the gateway's answer.
    if let Some(merchant) = payment.merchant() {
        let since = if pass.is_multiple_of(OLD_ORDER_EVERY) {
            now - payment::PENDING_GIVE_UP_SECS
        } else {
            now - RECENT_SECS
        };
        let orders = tokio::task::spawn_blocking(move || {
            payment::pending_orders(since, now - 30, PASS_LIMIT)
        })
        .await
        .map_err(|e| e.to_string())??;
        if !orders.is_empty() {
            let client = epay::http_client()?;
            for (out_trade_no, created_at) in orders {
                let result = reconcile_order(&client, &merchant, &out_trade_no).await;
                let expired = created_at < now - payment::ORDER_TTL_SECS;
                let no = out_trade_no.clone();
                tokio::task::spawn_blocking(move || match result {
                    // The gateway says unpaid and the order is past its TTL.
                    Ok(false) if expired => payment::close_order(&no, now).map(|_| ()),
                    Ok(_) => payment::mark_checked(&no, now),
                    Err(error) => {
                        tracing::debug!("[Payment] reconcile {no}: {error}");
                        payment::mark_checked(&no, now)
                    }
                })
                .await
                .map_err(|e| e.to_string())??;
            }
        }
        // Orders the gateway never answered for (e.g. never reached its cashier)
        // are given up after a week; a later payment would still credit them.
        tokio::task::spawn_blocking(move || {
            payment::close_stale(now - payment::PENDING_GIVE_UP_SECS, now)
        })
        .await
        .map_err(|e| e.to_string())??;
    }
    Ok(())
}

/// Polls the gateway for orders whose notification never arrived (delivery is
/// not guaranteed and stops after a few hours). Stops with the proxy server.
pub fn spawn_reconciler(cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut pass = 0u64;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            if let Err(error) = reconcile_pass(pass).await {
                tracing::warn!("[Payment] reconciliation pass failed: {error}");
            }
            pass = pass.wrapping_add(1);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::user_token_db as tokens;

    fn settings() -> PaymentSettings {
        PaymentSettings {
            enabled: true,
            pid: "1001".into(),
            key: "notify-key".into(),
            public_url: "https://site.example".into(),
            ..Default::default()
        }
    }

    fn signed(pairs: &[(&str, &str)], key: &str) -> BTreeMap<String, String> {
        let mut params: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        params.insert("sign_type".into(), "MD5".into());
        let signature = epay::sign(&params, key);
        params.insert("sign".into(), signature);
        params
    }

    fn order(cents: i64) -> (tokens::UserToken, payment::PublicOrder) {
        tokens::init_db().unwrap();
        let key = tokens::create_token(
            format!("notify-{}", uuid::Uuid::new_v4().simple()),
            "never".into(),
            None,
            0,
            None,
            None,
            None,
            0,
            0,
            vec![],
        )
        .unwrap();
        tokens::update_token_with_balance(
            &key.id,
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
            Some(Some(0)),
        )
        .unwrap();
        let order = payment::create_order(&key.id, None, cents, "alipay", 1_000).unwrap();
        (key, order)
    }

    fn notify_for(
        order: &payment::PublicOrder,
        money: &str,
        status: &str,
        key: &str,
    ) -> BTreeMap<String, String> {
        signed(
            &[
                ("pid", "1001"),
                ("trade_no", &format!("Z{}", order.out_trade_no)),
                ("out_trade_no", &order.out_trade_no),
                ("type", "alipay"),
                ("name", "API 调用额度充值 5.00 元"),
                ("money", money),
                ("trade_status", status),
                ("param", ""),
            ],
            key,
        )
    }

    fn balance(id: &str) -> Option<i64> {
        tokens::get_token_by_id(id).unwrap().unwrap().balance_micros
    }

    #[test]
    fn payment_notify_credits_once_and_rejects_forgeries() {
        let (key, order) = order(500);
        let config = settings();
        // Forged with another key, or for another merchant: refused, nothing credited.
        assert!(!handle_notify(
            &config,
            &notify_for(&order, "5.00", "TRADE_SUCCESS", "wrong"),
            2_000
        ));
        let mut other_pid = notify_for(&order, "5.00", "TRADE_SUCCESS", "notify-key");
        other_pid.insert("pid".into(), "9999".into());
        let resigned = epay::sign(&other_pid, "notify-key");
        other_pid.insert("sign".into(), resigned);
        assert!(!handle_notify(&config, &other_pid, 2_000));
        // A paid amount that differs from the order is refused for retry and alerting.
        assert!(!handle_notify(
            &config,
            &notify_for(&order, "0.01", "TRADE_SUCCESS", "notify-key"),
            2_000
        ));
        assert_eq!(balance(&key.id), Some(0));
        // Unpaid status: acknowledged, nothing credited.
        assert!(handle_notify(
            &config,
            &notify_for(&order, "5.00", "WAIT_BUYER_PAY", "notify-key"),
            2_000
        ));
        assert_eq!(balance(&key.id), Some(0));
        // The real notification, then its retries.
        let genuine = notify_for(&order, "5", "TRADE_SUCCESS", "notify-key");
        assert!(handle_notify(&config, &genuine, 2_001));
        assert!(handle_notify(&config, &genuine, 2_002));
        assert_eq!(balance(&key.id), Some(5_000_000));
        let stored = payment::order_for_token(&key.id, &order.out_trade_no)
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, "paid");
    }

    #[test]
    fn payment_notify_needs_configured_credentials() {
        let (_, order) = order(500);
        let unconfigured = PaymentSettings::default();
        assert!(!handle_notify(
            &unconfigured,
            &notify_for(&order, "5.00", "TRADE_SUCCESS", ""),
            2_000
        ));
        // A signed notification for an order this site never created is acknowledged.
        let mut unknown = notify_for(&order, "5.00", "TRADE_SUCCESS", "notify-key");
        unknown.insert("out_trade_no".into(), "99999999".into());
        let resigned = epay::sign(&unknown, "notify-key");
        unknown.insert("sign".into(), resigned);
        assert!(handle_notify(&settings(), &unknown, 2_000));
    }

    #[tokio::test]
    async fn payment_notify_route_accepts_get_and_post() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_, order) = order(500);
        // The route reads live settings, which are not configured in tests, so a
        // valid-looking callback is refused: only transport parsing is checked here.
        let app = Router::new().route(NOTIFY_PATH, get(notify).post(notify));
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(notify_for(&order, "5.00", "TRADE_SUCCESS", "notify-key"))
            .finish();
        for request in [
            Request::get(format!("{NOTIFY_PATH}?{query}"))
                .body(Body::empty())
                .unwrap(),
            Request::post(NOTIFY_PATH)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(query.clone()))
                .unwrap(),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/plain; charset=utf-8"
            );
            let body = axum::body::to_bytes(response.into_body(), 64)
                .await
                .unwrap();
            assert!(body == "fail" || body == "success");
        }
        // Oversized or parameter-stuffed callbacks are refused before parsing further.
        let stuffed: String = (0..NOTIFY_MAX_PARAMS + 1)
            .map(|i| format!("p{i}=1&"))
            .collect();
        let huge = format!("x={}", "a".repeat(NOTIFY_MAX_BYTES));
        for query in [stuffed, huge] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(format!("{NOTIFY_PATH}?{query}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(response.into_body(), 64)
                .await
                .unwrap();
            assert_eq!(body, "fail");
        }
    }
}
