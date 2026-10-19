//! EPay (易支付) merchant protocol, as served by ZPay and compatible gateways.
//!
//! Signature: drop `sign`, `sign_type` and empty values, sort the rest by name in
//! ASCII order, join as `k=v&k=v` with raw (not URL-encoded) values, append the
//! merchant key directly and take the lowercase hex MD5 of the UTF-8 bytes.
//! Order queries are unsigned and carry the key itself, so their URLs must never
//! be logged.

use md5::{Digest, Md5};
use std::collections::BTreeMap;
use std::time::Duration;

pub const PAID_STATUS: &str = "TRADE_SUCCESS";

/// Gateway credentials.
#[derive(Clone)]
pub struct Merchant {
    /// Base URL without a trailing slash, e.g. `https://zpayz.cn`.
    pub gateway: String,
    pub pid: String,
    pub key: String,
}

pub fn sign(params: &BTreeMap<String, String>, key: &str) -> String {
    let joined = params
        .iter()
        .filter(|(name, value)| !value.is_empty() && *name != "sign" && *name != "sign_type")
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    Md5::digest(format!("{joined}{key}").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Checks a gateway callback over every parameter it carried.
pub fn verify(params: &BTreeMap<String, String>, key: &str) -> bool {
    let Some(given) = params.get("sign") else {
        return false;
    };
    let expected = sign(params, key);
    let given = given.to_ascii_lowercase();
    // Constant-time comparison so the signature cannot be guessed byte by byte.
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// `1234` → `"12.34"`; the gateway takes yuan with at most two decimals.
pub fn format_money(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// `"12.3"` / `"12.30"` / `"12"` → `1230`. Rejects signs, exponents and
/// more than two decimals instead of rounding.
pub fn parse_money(value: &str) -> Option<i64> {
    let value = value.trim();
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || whole.len() > 12
        || fraction.len() > 2
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (value.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let cents: i64 = format!("{whole}{fraction:0<2}").parse().ok()?;
    Some(cents)
}

/// Fields for the gateway cashier (`submit.php`), signed and ready to POST
/// from the browser. Returns the form action and its fields.
pub fn submit_form(
    merchant: &Merchant,
    pay_type: &str,
    out_trade_no: &str,
    notify_url: &str,
    return_url: &str,
    name: &str,
    amount_cents: i64,
) -> (String, BTreeMap<String, String>) {
    let mut fields: BTreeMap<String, String> = [
        ("pid", merchant.pid.as_str()),
        ("type", pay_type),
        ("out_trade_no", out_trade_no),
        ("notify_url", notify_url),
        ("return_url", return_url),
        ("name", name),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    fields.insert("money".into(), format_money(amount_cents));
    let signature = sign(&fields, &merchant.key);
    fields.insert("sign".into(), signature);
    fields.insert("sign_type".into(), "MD5".into());
    (format!("{}/submit.php", merchant.gateway), fields)
}

/// The gateway's view of one order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOrder {
    pub paid: bool,
    pub amount_cents: Option<i64>,
    pub trade_no: Option<String>,
    pub pid: Option<String>,
}

/// The gateway mixes numbers and strings for the same field.
fn loose_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

/// Interprets an `api.php?act=order` response body.
pub fn parse_order_response(body: &str) -> Result<RemoteOrder, String> {
    let json: serde_json::Value = serde_json::from_str(body.trim())
        .map_err(|_| "order query returned a non-JSON body".to_string())?;
    let field = |name: &str| json.get(name).and_then(loose_string);
    if field("code").as_deref() != Some("1") {
        return Err(format!(
            "order query failed: {}",
            field("msg").unwrap_or_else(|| "unknown error".into())
        ));
    }
    Ok(RemoteOrder {
        paid: field("status").as_deref() == Some("1"),
        amount_cents: field("money").as_deref().and_then(parse_money),
        trade_no: field("trade_no"),
        pid: field("pid"),
    })
}

/// One shared client (connection pool) for every gateway call.
pub fn http_client() -> Result<reqwest::Client, String> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to build payment HTTP client: {e}"))?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// Asks the gateway whether an order was paid. Errors never include the URL,
/// which carries the merchant key.
pub async fn query_order(
    client: &reqwest::Client,
    merchant: &Merchant,
    out_trade_no: &str,
) -> Result<RemoteOrder, String> {
    let url = format!("{}/api.php", merchant.gateway);
    let response = client
        .get(url)
        .query(&[
            ("act", "order"),
            ("pid", merchant.pid.as_str()),
            ("key", merchant.key.as_str()),
            ("out_trade_no", out_trade_no),
        ])
        .send()
        .await
        .map_err(|e| format!("order query request failed: {}", e.without_url()))?;
    let body = response
        .text()
        .await
        .map_err(|e| format!("order query response unreadable: {}", e.without_url()))?;
    parse_order_response(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn epay_signature_matches_reference_vector() {
        // Reference: the documented rule, cross-checked against a copy of the
        // official PHP demo's get_sign(). Empty `param` and `sign_type` are skipped.
        let fields = params(&[
            ("money", "10.00"),
            ("name", "账户余额充值10元"),
            ("notify_url", "https://example.com/api/pay/zpay/notify"),
            ("out_trade_no", "20261019000001"),
            ("pid", "1001"),
            ("return_url", "https://example.com/pay/return"),
            ("type", "alipay"),
            ("param", ""),
            ("sign_type", "MD5"),
        ]);
        assert_eq!(
            sign(&fields, "TESTKEY123"),
            "333a5e22c9fe1dc36554ba4fe50b1447"
        );
    }

    #[test]
    fn epay_verify_accepts_valid_and_rejects_tampered_callbacks() {
        let mut notify = params(&[
            ("pid", "1001"),
            ("trade_no", "2026101912345"),
            ("out_trade_no", "20261019000001"),
            ("type", "alipay"),
            ("name", "充值"),
            ("money", "5.00"),
            ("trade_status", "TRADE_SUCCESS"),
            ("param", ""),
            ("sign_type", "MD5"),
        ]);
        let signature = sign(&notify, "K");
        notify.insert("sign".into(), signature.to_ascii_uppercase());
        assert!(verify(&notify, "K"), "case-insensitive");
        assert!(!verify(&notify, "other-key"));
        let mut tampered = notify.clone();
        tampered.insert("money".into(), "500.00".into());
        assert!(!verify(&tampered, "K"));
        // An extra parameter is covered by the signature too.
        let mut extra = notify.clone();
        extra.insert("bonus".into(), "1".into());
        assert!(!verify(&extra, "K"));
        notify.remove("sign");
        assert!(!verify(&notify, "K"));
    }

    #[test]
    fn epay_money_round_trips_without_rounding() {
        assert_eq!(format_money(500), "5.00");
        assert_eq!(format_money(123456), "1234.56");
        assert_eq!(format_money(7), "0.07");
        for (text, cents) in [
            ("5", 500),
            ("5.0", 500),
            ("5.00", 500),
            ("0.01", 1),
            ("12.3", 1230),
        ] {
            assert_eq!(parse_money(text), Some(cents), "{text}");
        }
        for bad in [
            "", "-1", "+1", "1.", ".5", "1.234", "1e3", "1,00", "abc", "1.2.3",
        ] {
            assert_eq!(parse_money(bad), None, "{bad}");
        }
    }

    #[test]
    fn epay_submit_form_is_signed_over_its_own_fields() {
        let merchant = Merchant {
            gateway: "https://zpayz.cn".into(),
            pid: "1001".into(),
            key: "K".into(),
        };
        let (action, fields) = submit_form(
            &merchant,
            "alipay",
            "20261019000001",
            "https://site/api/pay/epay/notify",
            "https://site/usage",
            "API 调用额度充值 5.00 元",
            500,
        );
        assert_eq!(action, "https://zpayz.cn/submit.php");
        assert_eq!(fields["money"], "5.00");
        assert_eq!(fields["sign_type"], "MD5");
        assert!(verify(&fields, "K"));
    }

    #[test]
    fn epay_order_response_parsing_is_loose_about_types() {
        let paid = parse_order_response(
            r#"{"code":1,"msg":"查询订单号成功！","trade_no":"2026101912345","out_trade_no":"20261019000001","type":"alipay","pid":"1001","money":"5.00","status":1}"#,
        )
        .unwrap();
        assert_eq!(
            paid,
            RemoteOrder {
                paid: true,
                amount_cents: Some(500),
                trade_no: Some("2026101912345".into()),
                pid: Some("1001".into()),
            }
        );
        let unpaid = parse_order_response(r#"{"code":"1","status":"0","money":5}"#).unwrap();
        assert!(!unpaid.paid);
        assert_eq!(unpaid.amount_cents, Some(500));
        assert!(parse_order_response(r#"{"code":0,"msg":"订单号不存在"}"#)
            .unwrap_err()
            .contains("订单号不存在"));
        assert!(parse_order_response("<html>ThinkPHP error</html>").is_err());
    }
}
