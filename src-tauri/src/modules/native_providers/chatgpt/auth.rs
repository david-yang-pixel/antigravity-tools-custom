//! Refresh-token exchange against auth.openai.com.
use super::{endpoints, headers, identity};
use crate::modules::native_providers::{
    http::{Failure, Penalty},
    now, number, Account,
};
use reqwest::header::HeaderMap;
use serde_json::Value;

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPE: &str = "openid profile email";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// The refresh token can never work again: the administrator must re-import the account.
const DEAD: [&str; 7] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_reused",
    "refresh_token_invalidated",
    "app_session_terminated",
    "access_denied",
];
/// Provider-wide configuration problems; they say nothing about this account.
const CONFIG: [&str; 3] = ["invalid_client", "unauthorized_client", "invalid_scope"];

fn dead(message: String) -> Failure {
    Failure::classified(401, message.clone(), Penalty::Disable { message }, false)
}

fn rejection(status: u16, body: &[u8]) -> Failure {
    let text = String::from_utf8_lossy(body).to_lowercase();
    let json: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = [&json["error"]["code"], &json["error"], &json["code"]]
        .into_iter()
        .find_map(Value::as_str)
        .unwrap_or_default();
    let found = |list: &[&'static str]| {
        list.iter()
            .copied()
            .find(|c| code.eq_ignore_ascii_case(c) || text.contains(c))
    };
    if let Some(c) = found(&CONFIG) {
        return Failure::new(503, format!("ChatGPT token refresh is misconfigured ({c})"));
    }
    if let Some(c) = found(&DEAD) {
        return dead(format!("ChatGPT refresh token is no longer valid ({c})"));
    }
    let status = if (400..600).contains(&status) {
        status
    } else {
        502
    };
    Failure::new(
        status,
        format!("ChatGPT token refresh failed (HTTP {status})"),
    )
}

/// Exchanges the refresh token. The account is only modified when the whole answer is usable.
pub async fn refresh(client: &reqwest::Client, a: &mut Account) -> Result<(), Failure> {
    if a.credentials.refresh_token.trim().is_empty() {
        return Err(dead("ChatGPT account has no refresh token".into()));
    }
    let url = endpoints::url(&endpoints::current().auth, "/oauth/token")?;
    let mut h = HeaderMap::new();
    headers::apply_identity(&mut h, false);
    let response = client
        .post(url)
        .headers(h)
        .timeout(TIMEOUT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", a.credentials.refresh_token.as_str()),
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
        ])
        .send()
        .await
        .map_err(|e| Failure::sending(&e, "ChatGPT token refresh connection failed"))?;
    let status = response.status().as_u16();
    let body = super::client::read_bytes(response).await?;
    if !(200..300).contains(&status) {
        return Err(rejection(status, &body));
    }
    let json: Value = serde_json::from_slice(&body)
        .map_err(|_| Failure::new(502, "ChatGPT token refresh returned invalid JSON"))?;
    let text = |key: &str| {
        json.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
    };
    let access = text("access_token");
    if access.is_empty() {
        return Err(Failure::new(
            502,
            "ChatGPT token refresh returned no access token",
        ));
    }
    let mut next = a.clone();
    next.credentials.access_token = access.into();
    for (key, slot) in [
        ("id_token", &mut next.credentials.id_token),
        ("refresh_token", &mut next.credentials.refresh_token),
    ] {
        // A missing or empty value never overwrites what is stored.
        if !text(key).is_empty() {
            *slot = text(key).into();
        }
    }
    next.expires_at = number(&json, &["expires_in"])
        .filter(|n| *n > 0.0)
        .map_or(0, |n| now() + n as i64);
    // Claims missing from the new tokens leave the known identity untouched.
    let _ = identity::apply(&mut next);
    *a = next;
    Ok(())
}
