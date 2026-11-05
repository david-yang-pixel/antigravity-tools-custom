//! WorkBuddy's side of the shared per-account egress ([`crate::modules::egress`]):
//! the route decides where an account leaves, this module builds its Chrome-profile
//! clients. A bound account fails closed when its node is unusable; it never falls
//! back to the server's own address.
use super::accounts::{self, Account};
use crate::modules::egress::{self as shared, Exit, Pool};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

pub use shared::{LoginHold, Probe, Update, View};

const POOL: Pool = Pool::WorkBuddy;
const PROBE_URL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

static CLIENTS: LazyLock<Mutex<HashMap<String, rquest::Client>>> = LazyLock::new(Default::default);
/// Direct accounts still share this server's address. They must not also share
/// one HTTP/2 connection, or several tokens ride the same socket.
static DIRECT_CLIENTS: LazyLock<Mutex<HashMap<String, rquest::Client>>> =
    LazyLock::new(Default::default);

fn direct() -> Result<rquest::Client, String> {
    super::client::http().cloned()
}
fn direct_for(account_id: &str) -> Result<rquest::Client, String> {
    let mut clients = DIRECT_CLIENTS
        .lock()
        .map_err(|_| "Egress client cache unavailable")?;
    if let Some(client) = clients.get(account_id) {
        return Ok(client.clone());
    }
    let client = super::client::builder()
        .build()
        .map_err(|_| "Cannot initialize WorkBuddy connection")?;
    if clients.len() >= 1024 {
        clients.clear();
    }
    clients.insert(account_id.to_owned(), client.clone());
    Ok(client)
}
fn proxied(proxy_url: &str) -> Result<rquest::Client, String> {
    let mut clients = CLIENTS
        .lock()
        .map_err(|_| "Egress client cache unavailable")?;
    if let Some(client) = clients.get(proxy_url) {
        return Ok(client.clone());
    }
    let proxy = rquest::Proxy::all(proxy_url).map_err(|_| "Invalid egress route")?;
    let client = super::client::builder()
        .proxy(proxy)
        .build()
        .map_err(|_| "Cannot initialize WorkBuddy egress connection")?;
    if clients.len() >= 1024 {
        clients.clear();
    }
    clients.insert(proxy_url.to_owned(), client.clone());
    Ok(client)
}

/// HTTP client for one account's upstream calls.
pub async fn client_for(account: &Account) -> Result<rquest::Client, String> {
    match shared::exit_for(POOL, &account.id).await? {
        Exit::Off | Exit::Direct => direct_for(&account.id),
        Exit::Proxy { url, .. } => proxied(&url),
    }
}

/// A login request's route; holding it keeps the shared login selector on `node`.
pub struct LoginRoute {
    pub client: rquest::Client,
    pub node: Option<String>,
    /// Set when this login claimed a free subscription node. Move it into the
    /// authorization session so the node stays reserved until the account is bound.
    pub hold: Option<LoginHold>,
    _route: shared::LoginRoute,
}
pub async fn login_route(preferred: Option<&str>) -> Result<LoginRoute, String> {
    let mut route = shared::login_route(POOL, preferred).await?;
    let client = match &route.exit {
        Exit::Off | Exit::Direct => direct()?,
        Exit::Proxy { url, .. } => proxied(url)?,
    };
    Ok(LoginRoute {
        client,
        node: route.node.clone(),
        hold: route.hold.take(),
        _route: route,
    })
}
pub fn bound_node(account_id: &str) -> Result<Option<String>, String> {
    shared::bound_node(POOL, account_id)
}
pub fn adopt(account_id: &str, node: Option<&str>) {
    shared::adopt(POOL, account_id, node)
}
pub fn forget(account_id: &str) {
    if let Ok(mut clients) = DIRECT_CLIENTS.lock() {
        clients.remove(account_id);
    }
    shared::forget(POOL, account_id)
}
pub async fn view() -> Result<View, String> {
    shared::view(POOL).await
}
pub async fn save(update: Update) -> Result<View, String> {
    shared::save(POOL, update).await
}
pub async fn set_account(account_id: &str, node: Option<String>) -> Result<View, String> {
    shared::set_account(POOL, account_id, node).await
}
/// Latency from this server to the account's WorkBuddy endpoint along its real path.
pub async fn test_account(account_id: &str) -> Result<Probe, String> {
    let account = accounts::get(account_id)?;
    let url = format!("{}/", super::client::base(account.realm));
    let direct_url = url.clone();
    shared::test_account(POOL, account_id, &url, |_| async move {
        let started = std::time::Instant::now();
        direct_for(account_id)?
            .head(&direct_url)
            .timeout(PROBE_URL_TIMEOUT)
            .send()
            .await
            .map(|_| started.elapsed().as_millis() as u64)
            .map_err(|_| "WorkBuddy is unreachable from this server".to_string())
    })
    .await
}
pub async fn test_nodes() -> Result<View, String> {
    shared::test_nodes(POOL).await
}
pub async fn refresh() -> Result<View, String> {
    shared::refresh(POOL).await
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_egress_keeps_direct_connections() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let account =
            accounts::parse_import(serde_json::json!({"uid":"u","accessToken":"t"})).unwrap();
        assert!(client_for(&account).await.is_ok());
        let route = login_route(None).await.unwrap();
        assert!(route.node.is_none());
    }
}
