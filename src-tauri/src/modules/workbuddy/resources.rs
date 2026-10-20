//! Native billing resources are informational; downstream RMB billing never reads them.
use super::{
    accounts::{Account, Realm},
    client,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Credits {
    pub available: f64,
    pub updated_at: i64,
    pub expiries: Vec<Expiry>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Expiry {
    pub at: i64,
    pub amount: f64,
}
fn billing_base(realm: Realm) -> String {
    #[cfg(test)]
    if let Ok(base) = std::env::var("STELLAR_TEST_WORKBUDDY_BASE") {
        return base;
    }
    match realm {
        Realm::Cn => "https://www.codebuddy.cn",
        Realm::Global => "https://www.workbuddy.ai",
    }
    .into()
}
async fn post(account: &Account, path: &str, body: &Value) -> Result<reqwest::Response, String> {
    let headers = headers(account)?;
    client::http()?
        .post(format!("{}{path}", billing_base(account.realm)))
        .headers(headers)
        .json(body)
        .timeout(Duration::from_secs(25))
        .send()
        .await
        .map_err(|_| "Cannot connect to WorkBuddy account resources".into())
}
pub(super) fn headers(account: &Account) -> Result<reqwest::header::HeaderMap, String> {
    let mut headers = client::headers(account.realm, Some(account))?;
    for (key, value) in [
        ("user-agent", "WorkBuddy/5.5.4"),
        ("x-enterprise-id", account.enterprise_id.as_str()),
        ("x-tenant-id", account.enterprise_id.as_str()),
        ("x-domain", account.domain.as_str()),
        ("x-device-token", account.device_token.as_str()),
    ] {
        if !value.is_empty() {
            headers.insert(
                key,
                value
                    .parse()
                    .map_err(|_| "Invalid WorkBuddy resource header")?,
            );
        }
    }
    Ok(headers)
}
fn packages(mut value: &Value) -> Result<&Vec<Value>, String> {
    for _ in 0..5 {
        if let Some(items) = value.as_array() {
            return Ok(items);
        }
        if let Some(items) = value.get("Accounts").and_then(Value::as_array) {
            return Ok(items);
        }
        value = ["Response", "Data", "data", "response"]
            .iter()
            .find_map(|key| value.get(key).filter(|v| v.is_object() || v.is_array()))
            .ok_or("WorkBuddy credit response has an unknown format")?;
    }
    Err("WorkBuddy credit response has too many wrappers".into())
}
fn number(item: &Value, key: &str) -> f64 {
    item[key].as_f64().filter(|n| n.is_finite()).unwrap_or(0.0)
}
fn remain(item: &Value) -> f64 {
    let size = number(item, "CycleCapacitySize");
    if size > 0.0 {
        let remaining = number(item, "CycleCapacityRemain").clamp(0.0, size);
        remaining.min((size - number(item, "CycleCapacityUsed")).max(0.0))
    } else {
        number(item, "CapacityRemain").max(0.0)
    }
}
fn add_packages(credits: &mut Credits, items: &[Value]) -> Result<(), String> {
    for item in items {
        if !item.is_object() {
            return Err("Invalid WorkBuddy resource package".into());
        }
        // Unknown fields are not evidence that the account has zero credits.
        if !item["CycleCapacityRemain"].is_number() && !item["CapacityRemain"].is_number() {
            return Err("WorkBuddy package is missing remaining credits".into());
        }
        let amount = remain(item);
        credits.available += amount;
        if amount > 0.0 {
            let raw = item["CycleEndTime"]
                .as_str()
                .or_else(|| item["PackageEndTime"].as_str());
            if let Some(at) = raw.and_then(|s| {
                chrono::DateTime::parse_from_str(&format!("{s} +0800"), "%Y-%m-%d %H:%M:%S %z").ok()
            }) {
                credits.expiries.push(Expiry {
                    at: at.timestamp(),
                    amount,
                });
            }
        }
    }
    Ok(())
}
pub async fn credits(account: &Account) -> Result<Credits, String> {
    // Tencent timestamps are UTC+8, independent of the Docker host timezone.
    let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
    let mut result = Credits::default();
    for page in 1..=20 {
        let body = json!({"PageNumber":page,"PageSize":100,"ProductCode":"p_tcaca","Status":[0,3],
            "PackageEndTimeRangeBegin":now.format("%Y-%m-%d %H:%M:%S").to_string(),
            "PackageEndTimeRangeEnd":(now + chrono::Duration::days(365*101)).format("%Y-%m-%d %H:%M:%S").to_string()});
        let path = if account.realm == Realm::Cn {
            "/v2/billing/meter/get-user-resource"
        } else {
            "/billing/meter/get-user-resource"
        };
        let mut response = post(account, path, &body).await?;
        if account.realm == Realm::Global && response.status() == reqwest::StatusCode::NOT_FOUND {
            response = post(account, "/v2/billing/meter/get-user-resource", &body).await?;
        }
        let value = client::json_response(response).await?;
        let items = packages(&value)?;
        add_packages(&mut result, items)?;
        if items.len() < 100 {
            result.available = (result.available * 100.0).round() / 100.0;
            result.updated_at = super::accounts::now();
            result.expiries.sort_by_key(|e| e.at);
            return Ok(result);
        }
    }
    Err("Too many WorkBuddy resource pages; refusing to display a partial balance".into())
}
pub async fn checkin(account: &Account) -> Result<&'static str, String> {
    if account.realm == Realm::Global {
        return Ok("unsupported");
    }
    let response = post(account, "/v2/billing/meter/daily-checkin", &json!({})).await?;
    checkin_response(response).await
}

async fn checkin_response(response: reqwest::Response) -> Result<&'static str, String> {
    let status = response.status();
    // Tencent returns HTTP 400 for an already completed daily check-in.
    // Inspect only this endpoint's bounded business response, not all HTTP errors.
    let value = client::read_json_body(response).await.map_err(|error| {
        if status.is_success() {
            error
        } else {
            format!("WorkBuddy check-in returned HTTP {}", status.as_u16())
        }
    })?;
    let code = value["code"]
        .as_i64()
        .or_else(|| value["code"].as_str().and_then(|v| v.parse().ok()));
    let message = value["msg"]
        .as_str()
        .or_else(|| value["message"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let already = matches!(code, Some(10001 | 14001))
        && ["今天已签到", "今日已签到", "already checked in today"]
            .iter()
            .any(|marker| message.contains(marker));
    if already && (status.is_success() || status == reqwest::StatusCode::BAD_REQUEST) {
        return Ok("already");
    }
    if status.is_success() && code == Some(0) {
        return Ok("success");
    }
    // Include safe diagnostic codes, never relay a body that could contain credentials.
    match code {
        Some(code) => Err(format!(
            "WorkBuddy check-in rejected (HTTP {}, code {code})",
            status.as_u16()
        )),
        None => Err(format!(
            "WorkBuddy check-in returned HTTP {} without a result code",
            status.as_u16()
        )),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn workbuddy_checkin_recognizes_already_without_hiding_real_errors() {
        for (status, body, expected) in [
            (200, json!({"code":0}), Some("success")),
            (
                400,
                json!({"code":10001,"msg":"今天已签到，请明天再来"}),
                Some("already"),
            ),
            (
                200,
                json!({"code":"10001","msg":"今日已签到"}),
                Some("already"),
            ),
            (
                400,
                json!({"code":14001,"message":"Already checked in today"}),
                Some("already"),
            ),
            (
                400,
                json!({"code":10001,"msg":"积分不足 secret_diagnostic"}),
                None,
            ),
            (200, json!({"code":10001}), None),
            (401, json!({"code":10001,"msg":"今日已签到"}), None),
            (500, json!({"code":10001,"msg":"今日已签到"}), None),
            (400, json!({"code":0}), None),
            (400, json!({"message":"secret_diagnostic"}), None),
        ] {
            let response = axum::http::Response::builder()
                .status(status)
                .body(body.to_string())
                .unwrap()
                .into();
            let result = checkin_response(response).await;
            match expected {
                Some(value) => assert_eq!(result.unwrap(), value),
                None => {
                    let error = result.unwrap_err();
                    assert!(error.contains(&format!("HTTP {status}")));
                    assert!(!error.contains("secret_diagnostic"));
                }
            }
        }
        for body in ["not JSON".to_owned(), "x".repeat(2 * 1024 * 1024 + 1)] {
            let response = axum::http::Response::builder()
                .status(400)
                .body(body)
                .unwrap()
                .into();
            assert!(checkin_response(response).await.is_err());
        }
        // Other endpoints must still reject the exact same HTTP 400 response.
        let response = axum::http::Response::builder()
            .status(400)
            .body(json!({"code":10001,"msg":"今日已签到"}).to_string())
            .unwrap()
            .into();
        assert!(client::raw_json_response(response).await.is_err());
    }
    #[test]
    fn workbuddy_resource_balance_bounds_and_expiry() {
        let v = json!({"Response":{"Data":{"Accounts":[
            {"CycleCapacitySize":100,"CycleCapacityRemain":200,"CycleCapacityUsed":40,"CycleEndTime":"2026-10-20 08:00:00"},
            {"CapacityRemain":-100}, {"CapacityRemain":25},
            {"CycleCapacitySize":100,"CycleCapacityRemain":50,"CycleCapacityUsed":120}
        ]}}});
        let mut c = Credits::default();
        add_packages(&mut c, packages(&v).unwrap()).unwrap();
        assert_eq!(c.available, 85.0);
        assert_eq!(c.expiries.len(), 1);
        assert_eq!(c.expiries[0].at, 1792454400);
        assert!(packages(&json!({"unexpected":[]})).is_err());
        assert!(add_packages(&mut c, &[json!({"Name":"unknown"})]).is_err());
        let mut zero = Credits::default();
        add_packages(&mut zero, &[json!({"CapacityRemain":0})]).unwrap();
        assert_eq!(zero.available, 0.0);
    }
}
