use super::{Account, Provider};
use crate::modules::egress::{self, Exit, Pool};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Method,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

#[derive(Clone, Debug)]
pub struct Failure {
    pub status: u16,
    pub message: String,
}
impl Failure {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Failure {}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new(502, message)
    }
}
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::new(502, message)
    }
}
/// Clients by owner (an account id, or `login`) with the route each was built for.
/// Accounts never share a connection pool: one socket never carries two accounts'
/// tokens, and a route change rebuilds only that account's client.
type ClientRoute = (bool, String, Option<String>);
static CLIENTS: LazyLock<Mutex<HashMap<String, (ClientRoute, qoder_client::transport::Client)>>> =
    LazyLock::new(Default::default);

/// The proxy behind `exit`. With egress off this is the gateway's own upstream proxy
/// setting (the path native providers used before egress existed); `DIRECT` is this
/// server's own address; a node is its loopback SOCKS route.
pub(super) fn proxy_url(exit: &Exit) -> String {
    match exit {
        Exit::Off => crate::modules::config::load_app_config()
            .ok()
            .map(|config| config.proxy.upstream_proxy)
            .filter(|proxy| proxy.enabled)
            .map(|proxy| proxy.url.trim().to_owned())
            .unwrap_or_default(),
        Exit::Direct => String::new(),
        Exit::Proxy { url, .. } => url.clone(),
    }
}
/// No overall timeout: callers bound each request by its own deadline, which also covers
/// long streams. Redirects are never followed so credentials cannot be replayed to another
/// host, and environment proxies are ignored.
fn build(proxy: &str) -> Result<qoder_client::transport::Client, Failure> {
    let build = if super::qoder::sign::legacy() {
        qoder_client::transport::Client::legacy
    } else {
        qoder_client::transport::Client::new
    };
    build(proxy).map_err(|_| Failure::new(502, "Cannot initialize Qoder connection"))
}
fn cached(owner: &str, exit: &Exit) -> Result<qoder_client::transport::Client, Failure> {
    let route = proxy_url(exit);
    // Mihomo's URL identifies the selector, not its selected node. A node change
    // must discard keep-alive connections even if that URL remains identical.
    let node = match exit {
        Exit::Proxy { node, .. } => Some(node.clone()),
        _ => None,
    };
    let fingerprint = (super::qoder::sign::legacy(), route.clone(), node);
    let mut clients = CLIENTS
        .lock()
        .map_err(|_| Failure::new(503, "Provider connections unavailable"))?;
    if let Some((built_for, client)) = clients.get(owner) {
        if *built_for == fingerprint {
            return Ok(client.clone());
        }
    }
    let client = build(&route)?;
    if clients.len() >= 1024 {
        clients.clear();
    }
    clients.insert(owner.to_owned(), (fingerprint, client.clone()));
    Ok(client)
}
/// Client for one saved account: its own egress node, `DIRECT`, or (egress off) the
/// gateway's usual outbound path. A bound account whose node is unusable fails here.
pub async fn client_for(account: &Account) -> Result<qoder_client::transport::Client, Failure> {
    let exit = egress::exit_for(Pool::Native(account.provider), &account.id).await?;
    cached(&account.id, &exit)
}
/// Client for one saved account on an already resolved route.
pub fn client_on(
    account: &Account,
    exit: &Exit,
) -> Result<qoder_client::transport::Client, Failure> {
    cached(&account.id, exit)
}
/// Client for an authorization that has no saved account yet.
pub fn login_client(
    provider: Provider,
    exit: &Exit,
) -> Result<qoder_client::transport::Client, Failure> {
    cached(&format!("{}:login", provider.key()), exit)
}
/// Drops a removed account's connections.
pub fn forget(account_id: &str) {
    if let Ok(mut clients) = CLIENTS.lock() {
        clients.remove(account_id);
    }
}
pub fn insert(headers: &mut HeaderMap, key: &'static str, value: &str) -> Result<(), Failure> {
    headers.insert(
        key,
        HeaderValue::from_str(value)
            .map_err(|_| Failure::new(400, "Invalid provider identity header"))?,
    );
    Ok(())
}
/// Credential-bearing requests are restricted to the provider's HTTPS service family. A
/// model response or imported account cannot redirect credentials to a new host.
pub fn official_url(provider: Provider, raw: &str) -> Result<url::Url, Failure> {
    let url =
        url::Url::parse(raw).map_err(|_| Failure::new(400, "Invalid provider service URL"))?;
    let domains: &[&str] = match provider {
        Provider::Qoder => &["qoder.com", "qoder.com.cn", "qoder.sh"],
    };
    let allowed = url.host_str().is_some_and(|host| {
        domains
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    });
    if !allowed
        || url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port().is_some_and(|p| p != 443)
    {
        return Err(Failure::new(
            400,
            "Provider returned an untrusted service URL",
        ));
    }
    Ok(url)
}
pub async fn json(
    client: &qoder_client::transport::Client,
    provider: Provider,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Option<Value>,
) -> Result<Value, Failure> {
    let url = official_url(provider, url)?;
    let mut request = client
        .request(method, url)
        .headers(headers)
        .timeout(std::time::Duration::from_secs(20));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| Failure::new(502, "Provider connection failed or timed out"))?;
    read_json(response).await
}
pub async fn read_json(response: reqwest::Response) -> Result<Value, Failure> {
    let status = response.status().as_u16();
    if status == 202 {
        return Ok(Value::Null);
    }
    if !(200..300).contains(&status) {
        return Err(Failure::new(
            status,
            format!("Provider returned HTTP {status}"),
        ));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Failure::new(502, "Provider response interrupted"))?
    {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err(Failure::new(502, "Provider response exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Failure::new(502, "Provider returned invalid text"))?;
    let text = qoder_client::signing::decrypt(text)
        .map_err(|_| Failure::new(502, "Cannot decrypt Qoder response"))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| Failure::new(502, "Provider returned invalid JSON"))?;
    if value
        .pointer("/ResponseMetadata/Error")
        .is_some_and(|v| !v.is_null())
    {
        return Err(Failure::new(
            502,
            "Provider rejected the authorization request",
        ));
    }
    Ok(value)
}
