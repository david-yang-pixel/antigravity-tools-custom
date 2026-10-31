//! Fixed-host activity API. Never accept an upstream-provided URL or relay raw errors.
use super::super::{
    accounts::{Account, Realm},
    client, resources,
};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::time::Duration;

pub(super) const TASKS: &str = "/v2/activity/growth/tasks";
pub(super) const STREAK: &str = "/activity/growth/streak";
pub(super) const TRAVEL: &str = "/activity/growth/buddy/travel/status";
pub(super) const CHANCES: &str = "/activity/growth/lottery/chances";

#[derive(Debug)]
pub(super) struct Error {
    pub status: &'static str,
    pub detail: String,
    http: u16,
}
impl Error {
    fn unknown(detail: &str) -> Self {
        Self {
            status: "unknown",
            detail: detail.into(),
            http: 0,
        }
    }
}

pub(super) async fn request(
    account: &Account,
    method: Method,
    path: &str,
    body: Option<Value>,
    web: bool,
) -> Result<Value, Error> {
    if account.realm != Realm::Cn {
        return Err(Error {
            status: "unsupported",
            detail: "Domestic accounts only".into(),
            http: 0,
        });
    }
    let mut headers =
        resources::headers(account).map_err(|_| Error::unknown("Invalid activity identity"))?;
    let mut base = client::base(Realm::Cn);
    if web {
        base = "https://www.workbuddy.cn".into();
        #[cfg(test)]
        if let Ok(value) = std::env::var("STELLAR_TEST_WORKBUDDY_BASE") {
            base = value;
        }
        for (key, value) in [
            ("origin", "https://www.workbuddy.cn"),
            ("referer", "https://www.workbuddy.cn/profile/growth-center"),
            ("x-client-platform", "web"),
            ("x-domain", "https://www.workbuddy.cn"),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
    }
    let mut req = client::http_for(account)
        .await
        .map_err(|error| Error::unknown(&error))?
        .request(method, format!("{base}{path}"))
        .headers(headers)
        .timeout(Duration::from_secs(15));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let response = req
        .send()
        .await
        .map_err(|_| Error::unknown("Activity request interrupted; result unconfirmed"))?;
    parse(response).await
}

async fn parse(response: reqwest::Response) -> Result<Value, Error> {
    let http = response.status();
    if matches!(http, StatusCode::NOT_FOUND | StatusCode::GONE) {
        return Err(Error {
            status: "unsupported",
            detail: format!("Activity endpoint unavailable (HTTP {})", http.as_u16()),
            http: http.as_u16(),
        });
    }
    let value = client::read_json_body(response)
        .await
        .map_err(|_| Error::unknown("Activity response invalid; result unconfirmed"))?;
    let code = value["code"]
        .as_i64()
        .or_else(|| value["code"].as_str().and_then(|v| v.parse().ok()));
    if http.is_success() && code == Some(0) {
        return value
            .get("data")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or_else(|| Error::unknown("Activity response missing data; result unconfirmed"));
    }
    let msg = value["msg"]
        .as_str()
        .or_else(|| value["message"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // Only explicit business outcomes are benign. A 403 alone is never eligibility evidence.
    let skipped = (http == StatusCode::CONFLICT
        && ["duplicate", "已领取"].iter().any(|m| msg.contains(m)))
        || (http == StatusCode::BAD_REQUEST
            && [
                "already_claimed",
                "already claimed",
                "insufficient lottery chance balance",
                "lottery disabled",
            ]
            .iter()
            .any(|m| msg.contains(m)))
        || (http == StatusCode::FORBIDDEN && msg.contains("连续登录天数不足"));
    Err(Error {
        status: if skipped {
            "skipped"
        } else if http.is_server_error() || (http.is_success() && code.is_none()) {
            "unknown"
        } else {
            "failed"
        },
        detail: if skipped {
            "Already claimed, not eligible, or no free lottery chances".into()
        } else {
            format!(
                "Activity rejected (HTTP {}, code {})",
                http.as_u16(),
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "missing".into())
            )
        },
        http: http.as_u16(),
    })
}

pub(super) async fn claim_task(account: &Account, code: &str) -> Result<Value, Error> {
    let path = format!("/activity/growth/tasks/{code}/claim");
    match request(account, Method::POST, &path, None, false).await {
        // Documented compatibility path, only after an explicit rejection. Never
        // replay timeouts/5xx or send credentials to a URL supplied by the response.
        Err(e) if e.http == 400 && e.status == "failed" => {
            request(account, Method::POST, &path, None, true).await
        }
        result => result,
    }
}

pub(super) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
pub(super) fn amount(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}
pub(super) fn safe_name(account: &Account, value: Option<&str>, fallback: &str) -> String {
    let value = value.unwrap_or(fallback);
    if [
        &account.access_token,
        &account.refresh_token,
        &account.device_token,
    ]
    .iter()
    .any(|s| !s.is_empty() && value.contains(s.as_str()))
    {
        return fallback.into();
    }
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn activity_errors_are_bounded_and_do_not_leak_or_hide_auth_failures() {
        for (status, body, expected) in [
            (409, json!({"code":1,"msg":"duplicate"}), "skipped"),
            (
                400,
                json!({"code":1,"msg":"insufficient lottery chance balance"}),
                "skipped",
            ),
            (
                401,
                json!({"code":1,"msg":"already_claimed secret-token"}),
                "failed",
            ),
            (
                403,
                json!({"code":1,"msg":"forbidden secret-token"}),
                "failed",
            ),
            (404, json!({}), "unsupported"),
            (500, json!({"code":1,"msg":"secret-token"}), "unknown"),
            (200, json!({"data":{}}), "unknown"),
            (200, json!({"code":0}), "unknown"),
        ] {
            let response = axum::http::Response::builder()
                .status(status)
                .body(body.to_string())
                .unwrap()
                .into();
            let error = parse(response).await.unwrap_err();
            assert_eq!(error.status, expected);
            assert!(!error.detail.contains("secret-token"));
        }
        assert!(!identifier("../claim?token=secret"));
        assert!(identifier("Sequential_Tasks_2"));
    }
}
