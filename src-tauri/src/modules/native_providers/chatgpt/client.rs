//! Requests to the ChatGPT backend: one canonical Codex client identity per call.
use super::{endpoints, headers as identity, quota};
use crate::modules::native_providers::http::{DispatchFailure, Failure};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

const MAX_BODY: usize = 32 * 1024 * 1024;
/// The only client headers that reach the upstream.
const FORWARDED: [&str; 5] = [
    "accept-language",
    "x-codex-turn-state",
    "x-codex-turn-metadata",
    "x-codex-window-id",
    "x-codex-installation-id",
];
const BETA_FEATURES: &str = "x-codex-beta-features";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Responses,
    Compact,
    /// Model catalog: a JSON GET without a conversation.
    Models,
}
impl Endpoint {
    pub fn path(self) -> &'static str {
        match self {
            Self::Responses => "/codex/responses",
            Self::Compact => "/codex/responses/compact",
            Self::Models => "/codex/models",
        }
    }
    fn accept(self) -> &'static str {
        match self {
            Self::Responses => "text/event-stream",
            Self::Compact | Self::Models => "application/json",
        }
    }
}

/// Reads a body, refusing anything above 32 MiB.
pub async fn read_bytes(mut response: reqwest::Response) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Failure::new(502, "ChatGPT response interrupted"))?
    {
        if bytes.len() + chunk.len() > MAX_BODY {
            return Err(Failure::new(502, "ChatGPT response exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// A UUID-shaped id that is stable for one conversation and scoped to one account.
pub fn session_id(scope: &str, account_id: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("{scope}:{account_id}:{key}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

fn value(text: &str) -> Result<HeaderValue, Failure> {
    HeaderValue::from_str(text)
        .map_err(|_| Failure::new(400, "ChatGPT credential is not a valid header value"))
}

/// Every header `send` puts on the request; the WebSocket relay reuses it for its handshake.
pub fn headers(
    a: &crate::modules::native_providers::Account,
    session: &str,
    forwarded: &HeaderMap,
    endpoint: Endpoint,
) -> Result<HeaderMap, Failure> {
    let account_id = a.credentials.chatgpt_account_id.trim();
    if account_id.is_empty() {
        return Err(Failure::new(
            400,
            "ChatGPT account has no chatgpt_account_id",
        ));
    }
    let mut h = HeaderMap::new();
    for name in FORWARDED {
        if let Some(v) = forwarded.get(name) {
            h.insert(HeaderName::from_static(name), v.clone());
        }
    }
    h.insert(
        reqwest::header::AUTHORIZATION,
        value(&format!("Bearer {}", a.credentials.access_token))?,
    );
    h.insert("chatgpt-account-id", value(account_id)?);
    h.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static(endpoint.accept()),
    );
    if endpoint != Endpoint::Models {
        h.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert("session_id", value(session)?);
        h.insert("conversation_id", value(session)?);
        h.insert(
            BETA_FEATURES,
            forwarded
                .get(BETA_FEATURES)
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("remote_compaction_v2")),
        );
    }
    identity::apply_identity(&mut h, true);
    Ok(h)
}

/// Sends one Codex request and returns the raw answer for the caller to classify. Quota
/// headers are recorded on every answer, successful or not.
pub async fn send(
    client: &reqwest::Client,
    a: &crate::modules::native_providers::Account,
    body: Value,
    endpoint: Endpoint,
    session: &str,
    forwarded: &HeaderMap,
    timeout: Duration,
) -> Result<reqwest::Response, DispatchFailure> {
    let url = endpoints::url(&endpoints::current().backend, endpoint.path())?;
    let h = headers(a, session, forwarded, endpoint)?;
    let response = client
        .post(url)
        .headers(h)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .map_err(|_| {
            DispatchFailure::Connection(Failure::new(502, "ChatGPT inference connection failed"))
        })?;
    quota::record(a, response.headers());
    Ok(response)
}
