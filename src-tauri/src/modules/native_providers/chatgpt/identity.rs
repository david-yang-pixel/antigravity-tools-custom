//! Account identity from the OAuth JWTs. Payloads are decoded without verifying the
//! signature: the tokens come straight from the issuer or from the administrator.
use super::{client, endpoints, headers};
use crate::modules::native_providers::{http::Failure, now, number, text, Account};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const OPENAI_ISSUER: &str = "https://auth.openai.com";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Claims {
    pub email: String,
    pub sub: String,
    pub user_id: String,
    pub account_id: String,
    pub plan: String,
    pub exp: i64,
    issuer: String,
    profile_email: String,
}

impl Claims {
    fn subject(&self) -> Option<&str> {
        [&self.user_id, &self.sub]
            .into_iter()
            .map(String::as_str)
            .find(|s| !s.is_empty())
    }
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
        issuer: text(&value, &["iss"]),
        profile_email: text(&value["https://api.openai.com/profile"], &["email"]),
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

#[derive(Deserialize)]
struct Workspace {
    #[serde(alias = "account_id")]
    id: String,
    can_access_with_session: Option<bool>,
    #[serde(default)]
    is_deactivated: bool,
    #[serde(default)]
    is_fedramp_compliant_workspace: bool,
    workspace_backend_origin: Option<String>,
    plan_type: Option<String>,
}

fn authorized_workspace(response: &Value, id: &str) -> Option<Workspace> {
    let accounts = &response["accounts"];
    let workspace: Workspace = if let Some(list) = accounts.as_array() {
        let mut matches = list.iter().filter(|entry| entry["id"] == id);
        let entry = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        serde_json::from_value(entry.clone()).ok()?
    } else {
        // The map can contain a synthetic default entry. Only consider an ordered workspace.
        if !response["account_ordering"]
            .as_array()?
            .iter()
            .any(|item| item == id)
        {
            return None;
        }
        let entry = accounts.get(id)?;
        let can_access: Option<bool> =
            serde_json::from_value(entry["can_access_with_session"].clone()).ok()?;
        if can_access == Some(false) {
            return None;
        }
        serde_json::from_value(entry["account"].clone()).ok()?
    };
    (workspace.id == id
        && !workspace.is_deactivated
        && !workspace.is_fedramp_compliant_workspace
        && workspace.can_access_with_session != Some(false)
        && workspace
            .workspace_backend_origin
            .as_deref()
            .is_none_or(|origin| origin == "https://chatgpt.com"))
    .then_some(workspace)
}

/// The export's intended workspace and user, captured before a refresh can change the claims.
pub struct ExportedWorkspace {
    account_id: String,
    subject: String,
}

impl ExportedWorkspace {
    pub fn from_access(a: &Account) -> Option<Self> {
        let account_id = Uuid::parse_str(&a.credentials.chatgpt_account_id).ok()?;
        let access = usable_access_claims(&a.credentials.access_token)?;
        Some(Self {
            account_id: account_id.to_string(),
            subject: access.subject()?.to_owned(),
        })
    }

    /// Confirm the intended workspace with the current bearer on the ordinary OpenAI route.
    pub async fn verify(&self, http: &reqwest::Client, a: &mut Account) -> Result<(), Failure> {
        let access = usable_access_claims(&a.credentials.access_token)
            .ok_or_else(|| Failure::new(401, "ChatGPT access token is unusable"))?;
        let wrong_workspace =
            |claims: &Claims| !claims.account_id.is_empty() && claims.account_id != self.account_id;
        if access.subject() != Some(self.subject.as_str())
            || wrong_workspace(&access)
            || claims(&a.credentials.id_token)
                .as_ref()
                .is_some_and(wrong_workspace)
        {
            return Err(Failure::new(
                403,
                "ChatGPT access token does not match the exported workspace and user",
            ));
        }
        let workspace =
            check_workspace(http, &a.credentials.access_token, &self.account_id).await?;

        a.uid = format!("{}:{}", self.account_id, self.subject);
        a.credentials.chatgpt_account_id = self.account_id.clone();
        a.credentials.verified_export_workspace = true;
        a.credentials.user_type = workspace.plan_type.unwrap_or_default();
        a.nickname = if access.email.is_empty() {
            access.profile_email
        } else {
            access.email
        };
        a.expires_at = access.exp;
        Ok(())
    }
}

fn usable_access_claims(token: &str) -> Option<Claims> {
    claims(token).filter(|access| access.issuer == OPENAI_ISSUER && access.exp > now() + 300)
}

async fn check_workspace(
    http: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> Result<Workspace, Failure> {
    let url = endpoints::url(&endpoints::current().backend, "/wham/accounts/check")?;
    let mut request_headers = HeaderMap::new();
    headers::apply_identity(&mut request_headers, true);
    request_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let response = http
        .get(url)
        .headers(request_headers)
        .bearer_auth(access_token)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| Failure::sending(&error, "ChatGPT workspace check connection failed"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Failure::new(
            status.as_u16(),
            format!("ChatGPT workspace check failed (HTTP {})", status.as_u16()),
        ));
    }
    let body: Value = serde_json::from_slice(&client::read_bytes(response).await?)
        .map_err(|_| Failure::new(502, "ChatGPT workspace check returned invalid JSON"))?;
    authorized_workspace(&body, account_id).ok_or_else(|| {
        Failure::new(
            403,
            "ChatGPT did not confirm access to the exported workspace; sign in to that workspace in Codex and import again",
        )
    })
}

/// A workspace verified at import remains pinned to the same authenticated user and ID.
pub(super) fn workspace_refresh_expiry(a: &Account) -> Option<i64> {
    let access = claims(&a.credentials.access_token)?;
    let subject = access.subject()?;
    let id = &a.credentials.chatgpt_account_id;
    (access.issuer == OPENAI_ISSUER
        && a.uid == format!("{id}:{subject}")
        && (access.account_id.is_empty() || access.account_id == *id)
        && access.exp > now())
    .then_some(access.exp)
}
