//! First-party Qoder device authorization, quota API and COSY model/inference service.
#[path = "qoder/releases.rs"]
pub mod releases;
#[path = "qoder/sign.rs"]
pub mod sign;
use super::{
    http::{self, DispatchFailure, Failure},
    *,
};
use reqwest::{header::HeaderMap, Method};
use serde_json::json;

pub fn website(realm: Realm) -> &'static str {
    match realm {
        Realm::Cn => "https://qoder.com.cn",
        Realm::Global => "https://qoder.com",
    }
}
pub fn openapi(realm: Realm) -> &'static str {
    match realm {
        Realm::Cn => "https://openapi.qoder.com.cn",
        Realm::Global => "https://openapi.qoder.sh",
    }
}
pub fn gateway(realm: Realm) -> &'static str {
    match realm {
        Realm::Cn => "https://gateway.qoder.com.cn",
        Realm::Global => "https://api3.qoder.sh",
    }
}
pub fn client_id(realm: Realm) -> &'static str {
    match realm {
        Realm::Cn => "1c5e33e1-364d-4ce6-b02c-acaa81274a5c",
        Realm::Global => "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb",
    }
}
pub fn headers(a: &Account) -> Result<HeaderMap, Failure> {
    let mut h = HeaderMap::new();
    for (key, value) in [
        (
            "authorization",
            format!("Bearer {}", a.credentials.access_token),
        ),
        (
            "user-agent",
            format!(
                "{}/{}",
                if sign::legacy() { "Qoder" } else { "qoder" },
                sign::version()
            ),
        ),
        ("accept", "application/json".into()),
        ("content-type", "application/json".into()),
        ("x-machine-id", a.credentials.machine_id.clone()),
        ("x-request-id", uuid::Uuid::new_v4().simple().to_string()),
        ("origin", website(a.realm).into()),
    ] {
        http::insert(&mut h, key, &value)?;
    }
    if !sign::legacy() {
        for name in ["content-type", "x-machine-id", "x-request-id", "origin"] {
            h.remove(name);
        }
    }
    Ok(h)
}
pub fn apply_tokens(a: &mut Account, value: &Value) -> Result<(), Failure> {
    let v = payload(value);
    let access = text(v, &["token", "device_token", "access_token", "accessToken"]);
    if access.is_empty() {
        return Err(Failure::new(
            401,
            "Qoder returned no access token; authorize the account again",
        ));
    }
    a.credentials.access_token = access;
    let refresh = text(v, &["refresh_token", "refreshToken"]);
    if !refresh.is_empty() {
        a.credentials.refresh_token = refresh;
    }
    a.expires_at = v
        .get("expires_at")
        .or_else(|| v.get("expiresAt"))
        .and_then(epoch)
        .or_else(|| number(v, &["expires_in"]).map(|ms| now() + (ms / 1000.0) as i64))
        .unwrap_or_else(|| {
            now()
                + if a.credentials.access_token.starts_with("jt-") {
                    86400
                } else {
                    30 * 86400
                }
        });
    if a.uid.is_empty() {
        a.uid = text(v, &["user_id", "uid"]);
    }
    Ok(())
}
pub async fn refresh(
    client: &qoder_client::transport::Client,
    a: &mut Account,
) -> Result<(), Failure> {
    let refresh = &a.credentials.refresh_token;
    let mut attempts = Vec::new();
    if !refresh.is_empty() {
        attempts.push((
            if refresh.starts_with("drt-") {
                "/api/v1/deviceToken/refresh"
            } else {
                "/api/v1/jobToken/refresh"
            },
            json!({"refresh_token":refresh}),
        ));
    }
    if !a.credentials.personal_token.is_empty() {
        attempts.push((
            "/api/v1/jobToken/exchange",
            json!({"personal_token":a.credentials.personal_token}),
        ));
    }
    let mut last = Failure::new(
        401,
        "Qoder has no refresh credential; authorize the account again",
    );
    for (path, body) in attempts {
        match http::json(
            client,
            Provider::Qoder,
            Method::POST,
            &format!("{}{path}", openapi(a.realm)),
            headers(a)?,
            Some(body),
        )
        .await
        {
            Ok(value) => return apply_tokens(a, &value),
            Err(error) => last = error,
        }
    }
    Err(last)
}
pub async fn identity(
    client: &qoder_client::transport::Client,
    a: &mut Account,
) -> Result<(), Failure> {
    let value = http::json(
        client,
        Provider::Qoder,
        Method::GET,
        &format!("{}/api/v1/userinfo", openapi(a.realm)),
        headers(a)?,
        None,
    )
    .await?;
    let v = payload(&value);
    let uid = text(v, &["id", "user_id", "uid"]);
    if uid.is_empty() {
        return Err(Failure::new(
            401,
            "Qoder did not return an account identity",
        ));
    }
    a.uid = uid;
    a.nickname = text(v, &["name", "nickname", "email"]);
    a.credentials.user_type = text(v, &["user_type", "userType"]);
    if a.credentials.user_type.is_empty() {
        a.credentials.user_type = "personal_professional_trial".into();
    }
    a.credentials.organization_id = text(v, &["organization_id", "organizationId"]);
    a.credentials.organization_name = text(v, &["organization_name", "organizationName"]);
    a.credentials.organization_tags = v.get("organization_tags").cloned();
    a.credentials.data_policy_agreed = v.get("data_policy_agreed").and_then(Value::as_bool);
    Ok(())
}
pub async fn models(
    client: &qoder_client::transport::Client,
    a: &Account,
) -> Result<Vec<Model>, Failure> {
    let signed = sign::prepare(a, "{}".into(), None, client.uses_proxy()).await?;
    let response = client
        .get(http::official_url(Provider::Qoder, &signed.url)?)
        .headers(signed_headers(&signed.headers)?)
        .body(signed.body.unwrap_or_default())
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| Failure::new(502, "Qoder model discovery failed"))?;
    let value = http::read_json(response).await?;
    parse_models(&value)
}
pub fn parse_models(value: &Value) -> Result<Vec<Model>, Failure> {
    let rows = payload(value)
        .get("chat")
        .and_then(Value::as_array)
        .ok_or("Qoder returned an unrecognized model catalog")?;
    let mut models = BTreeMap::new();
    for row in rows {
        let id = text(row, &["key", "id"]);
        if !crate::modules::custom_upstream::valid_model_id(&id)
            || row.get("enable") == Some(&Value::Bool(false))
        {
            continue;
        }
        models.insert(
            id.clone(),
            Model {
                name: text(row, &["display_name", "name"]),
                id,
                context_window: number(row, &["max_input_tokens"]).unwrap_or(0.0).max(0.0) as u64,
                vision: row["is_vl"] == true,
                reasoning: row["is_reasoning"] == true,
                is_free: row
                    .get("is_free")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| {
                        number(row, &["price_factor"]).is_some_and(|price| price == 0.0)
                    }),
            },
        );
    }
    if models.is_empty() {
        return Err("Qoder returned no enabled models".into());
    }
    Ok(models.into_values().take(2000).collect())
}
pub async fn quota(
    client: &qoder_client::transport::Client,
    a: &Account,
) -> Result<Quota, Failure> {
    let value = http::json(
        client,
        Provider::Qoder,
        Method::GET,
        &format!("{}/api/v2/quota/usage", openapi(a.realm)),
        headers(a)?,
        None,
    )
    .await?;
    parse_quota(&value)
}
pub fn parse_quota(value: &Value) -> Result<Quota, Failure> {
    let v = payload(value);
    let mut packs = Vec::new();
    for (keys, name, shared) in [
        (&["userQuota"][..], "套餐额度", false),
        (&["addOnQuota"][..], "附加额度", false),
        (
            &[
                "orgResourcePackage",
                "sharedQuota",
                "teamQuota",
                "poolQuota",
            ][..],
            "共享额度",
            true,
        ),
    ] {
        let Some(q) = keys
            .iter()
            .find_map(|key| v.get(*key).filter(|q| q.is_object()))
        else {
            continue;
        };
        if shared && q.get("available") == Some(&Value::Bool(false)) {
            continue;
        }
        let total = number(q, &["total", "cap"]);
        let remaining = number(q, &["remaining", "remain"]);
        let used = number(q, &["used"]);
        if total.is_none() && remaining.is_none() {
            continue;
        }
        let unlimited = total.is_some_and(|n| n < 0.0);
        packs.push(QuotaPack {
            name: name.into(),
            total: if unlimited { None } else { total },
            remaining: if unlimited { None } else { remaining },
            used,
            unlimited,
            expires_at: q.get("expiresAt").and_then(epoch),
        });
    }
    if packs.is_empty() {
        return Err(
            "Qoder quota response is unrecognized; the previous balance has been retained".into(),
        );
    }
    Ok(Quota::from_packs("credits", packs))
}
pub async fn chat(
    client: &qoder_client::transport::Client,
    a: &Account,
    model: &str,
    payload: Value,
    timeout: std::time::Duration,
) -> Result<reqwest::Response, DispatchFailure> {
    let deadline = tokio::time::Instant::now() + timeout;
    let body = super::wire::qoder_request(a, model, payload)?;
    let signed = tokio::time::timeout_at(
        deadline,
        sign::prepare(a, body.to_string(), Some(model), client.uses_proxy()),
    )
    .await
    .map_err(|_| Failure::new(504, "Qoder request preparation timed out"))??;
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(Failure::new(504, "Qoder request timed out before dispatch").into());
    }
    client
        .post(http::official_url(Provider::Qoder, &signed.url)?)
        .headers(signed_headers(&signed.headers)?)
        .body(signed.body.unwrap_or_default())
        .timeout(remaining)
        .send()
        .await
        .map_err(|_| {
            DispatchFailure::Connection(Failure::new(502, "Qoder inference connection failed"))
        })
}

fn signed_headers(values: &BTreeMap<String, String>) -> Result<HeaderMap, Failure> {
    let mut headers = HeaderMap::new();
    for (key, value) in values {
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| "Invalid Qoder signed header name")?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| "Invalid Qoder signed header value")?;
        headers.insert(name, value);
    }
    Ok(headers)
}
