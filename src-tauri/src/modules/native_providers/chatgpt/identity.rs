//! Account identity from the OAuth JWTs. Payloads are decoded without verifying the
//! signature: the tokens come straight from the issuer or from the administrator.
use crate::modules::native_providers::{http::Failure, number, text, Account};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;

const AUTH_CLAIM: &str = "https://api.openai.com/auth";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Claims {
    pub email: String,
    pub sub: String,
    pub user_id: String,
    pub account_id: String,
    pub plan: String,
    pub exp: i64,
}

pub fn claims(jwt: &str) -> Option<Claims> {
    let segment = jwt.trim().split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(segment.trim_end_matches('=')).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let auth = value.get(AUTH_CLAIM).unwrap_or(&Value::Null);
    Some(Claims {
        email: text(&value, &["email"]),
        sub: text(&value, &["sub"]),
        user_id: text(auth, &["chatgpt_user_id", "user_id"]),
        account_id: text(auth, &["chatgpt_account_id"]),
        plan: text(auth, &["chatgpt_plan_type"]),
        exp: number(&value, &["exp"]).map_or(0, |n| n as i64),
    })
}

/// Fills the account identity from its tokens. The id token wins; the access token fills
/// whatever it leaves empty.
pub fn apply(a: &mut Account) -> Result<(), Failure> {
    let id = claims(&a.credentials.id_token).unwrap_or_default();
    let access = claims(&a.credentials.access_token).unwrap_or_default();
    let pick = |f: fn(&Claims) -> &String| {
        [&id, &access]
            .into_iter()
            .map(f)
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default()
    };
    let account_id = pick(|c| &c.account_id);
    if account_id.is_empty() {
        return Err(Failure::new(
            400,
            "This ChatGPT token has no chatgpt_account_id claim",
        ));
    }
    let user = [pick(|c| &c.user_id), pick(|c| &c.sub)]
        .into_iter()
        .find(|s| !s.is_empty());
    a.uid = match user {
        Some(user) => format!("{account_id}:{user}"),
        None => account_id.clone(),
    };
    a.credentials.chatgpt_account_id = account_id;
    a.credentials.user_type = pick(|c| &c.plan);
    let email = pick(|c| &c.email);
    if !email.is_empty() {
        a.nickname = email;
    }
    if a.expires_at == 0 {
        a.expires_at = access.exp;
    }
    Ok(())
}
