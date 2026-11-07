use super::{Account, Provider};
use crate::modules::egress::{self, Exit, Pool};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

#[derive(Clone, Debug)]
pub struct Failure {
    pub status: u16,
    pub message: String,
    /// How the account is charged for this failure; `None` means the status decides.
    pub penalty: Option<Penalty>,
    /// Another account may serve the request.
    pub failover: bool,
}
/// A provider's own verdict on what a failure means for the account that served it.
#[derive(Clone, Debug)]
pub enum Penalty {
    /// Counts against the breaker, honouring the provider's retry hint.
    Breaker { retry_after: Option<u64> },
    /// Unusable until `at` (unix seconds), whether or not the breaker is enabled.
    Until { at: i64, message: &'static str },
    /// The request was at fault, not the account.
    Neutral,
    /// Credentials are dead: turn the account off.
    Disable { message: String },
}
impl Failure {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            penalty: None,
            failover: false,
        }
    }
    pub fn classified(
        status: u16,
        message: impl Into<String>,
        penalty: Penalty,
        failover: bool,
    ) -> Self {
        Self {
            penalty: Some(penalty),
            failover,
            ..Self::new(status, message)
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
/// Inference preparation failures are safe to move to another account. Once the
/// transport is invoked, delivery is uncertain and the connection retry cap applies.
#[derive(Debug)]
pub enum DispatchFailure {
    Preparation(Failure),
    Connection(Failure),
}
impl From<Failure> for DispatchFailure {
    fn from(error: Failure) -> Self {
        Self::Preparation(error)
    }
}
impl From<DispatchFailure> for Failure {
    fn from(error: DispatchFailure) -> Self {
        match error {
            DispatchFailure::Preparation(error) | DispatchFailure::Connection(error) => error,
        }
    }
}
/// Clients by owner (an account id, or `<provider>:login`) with the route each was built
/// for. Accounts never share a connection pool: one socket never carries two accounts'
/// tokens, and a route change rebuilds only that account's client.
type ClientRoute = (String, Option<String>);
static CLIENTS: LazyLock<Mutex<HashMap<String, (ClientRoute, reqwest::Client)>>> =
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
fn build(proxy: &str) -> Result<reqwest::Client, Failure> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if !proxy.is_empty() {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|_| Failure::new(502, "Cannot initialize provider connection"))?,
        );
    }
    builder
        .build()
        .map_err(|_| Failure::new(502, "Cannot initialize provider connection"))
}
fn cached(owner: &str, exit: &Exit) -> Result<reqwest::Client, Failure> {
    let route = proxy_url(exit);
    // Mihomo's URL identifies the selector, not its selected node. A node change
    // must discard keep-alive connections even if that URL remains identical.
    let node = match exit {
        Exit::Proxy { node, .. } => Some(node.clone()),
        _ => None,
    };
    let fingerprint = (route.clone(), node);
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
pub async fn client_for(account: &Account) -> Result<reqwest::Client, Failure> {
    let exit = egress::exit_for(Pool::Native(account.provider), &account.id).await?;
    cached(&account.id, &exit)
}
/// Client for one saved account on an already resolved route.
pub fn client_on(account: &Account, exit: &Exit) -> Result<reqwest::Client, Failure> {
    cached(&account.id, exit)
}
/// Client for an authorization that has no saved account yet.
pub fn login_client(provider: Provider, exit: &Exit) -> Result<reqwest::Client, Failure> {
    cached(&format!("{}:login", provider.key()), exit)
}
#[cfg(test)]
pub(crate) fn cached_for(owner: &str) -> bool {
    CLIENTS
        .lock()
        .is_ok_and(|clients| clients.contains_key(owner))
}
/// Drops a removed account's connections.
pub fn forget(account_id: &str) {
    if let Ok(mut clients) = CLIENTS.lock() {
        clients.remove(account_id);
    }
}
