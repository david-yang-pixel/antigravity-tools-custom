//! Per-account egress for WorkBuddy. With egress enabled, each account's upstream
//! traffic (inference, refresh, models, resources, activity, login) leaves through
//! one node of an airport subscription, served by a managed mihomo process. An
//! account bound to a node fails closed when that node is unusable: it never falls
//! back to the server's own address. `DIRECT` is an explicit per-account choice.
mod mihomo;

pub use mihomo::Node;

use super::accounts::{self, Account, Realm};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};

pub const DIRECT: &str = "DIRECT";
/// Airport placeholder entries (quota, expiry, website) are not real exits.
const DEFAULT_EXCLUDE: &str =
    r"(?i)(剩余|流量|到期|过期|套餐|官网|网址|expire|traffic|remaining|reset)";
const PROBE_URL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub subscription_url: String,
    /// Auto-assignment only considers nodes matching `include` and not `exclude`.
    pub include: String,
    pub exclude: String,
    /// New or unbound accounts get the least-used healthy node on first use.
    pub auto_assign: bool,
    /// Executable to run. Config file / `ABV_MIHOMO_PATH` only: the admin API can
    /// never choose what the server executes.
    pub mihomo_path: String,
    /// Account id -> node name, or `DIRECT`.
    pub bindings: BTreeMap<String, String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            subscription_url: String::new(),
            include: String::new(),
            exclude: DEFAULT_EXCLUDE.into(),
            auto_assign: true,
            mihomo_path: String::new(),
            bindings: BTreeMap::new(),
        }
    }
}
#[derive(Default, Deserialize)]
pub struct Update {
    pub enabled: Option<bool>,
    /// Empty string clears the link; omitted keeps it.
    pub subscription_url: Option<String>,
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub auto_assign: Option<bool>,
}
#[derive(Serialize)]
pub struct NodeView {
    #[serde(flatten)]
    pub node: Node,
    /// Matches the auto-assignment filters.
    pub eligible: bool,
    pub accounts: usize,
}
#[derive(Serialize)]
pub struct View {
    pub enabled: bool,
    pub subscription_configured: bool,
    pub subscription_host: Option<String>,
    pub include: String,
    pub exclude: String,
    pub auto_assign: bool,
    pub env_locked: Vec<&'static str>,
    pub running: bool,
    pub binary: Option<String>,
    pub error: Option<String>,
    pub nodes: Vec<NodeView>,
    pub bindings: BTreeMap<String, String>,
}
#[derive(Serialize)]
pub struct Probe {
    pub node: String,
    pub delay_ms: Option<u64>,
    pub error: Option<String>,
}

static SETTINGS_LOCK: Mutex<()> = Mutex::new(());
static STORED: Mutex<Option<(crate::utils::fs::FileStamp, Settings)>> = Mutex::new(None);
/// Serializes auto-assignment so simultaneous first requests spread across nodes.
static ASSIGN_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);
/// Login runs before an account exists and shares one selector; one login at a time.
static LOGIN_LOCK: LazyLock<Arc<tokio::sync::Mutex<()>>> = LazyLock::new(Default::default);
static CLIENTS: LazyLock<Mutex<HashMap<String, reqwest::Client>>> = LazyLock::new(Default::default);

fn path() -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join("workbuddy_egress.json"))
}
fn stored() -> Result<Settings, String> {
    let path = path()?;
    let stamp = crate::utils::fs::file_stamp(&path);
    if let (Some(stamp), Ok(cache)) = (&stamp, STORED.lock()) {
        if let Some((_, settings)) = cache.as_ref().filter(|(cached, _)| cached == stamp) {
            return Ok(settings.clone());
        }
    }
    let settings: Settings = match std::fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data).map_err(|_| "Invalid workbuddy_egress.json")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(_) => return Err("Cannot read WorkBuddy egress settings".into()),
    };
    if let (Some(stamp), Ok(mut cache)) = (stamp, STORED.lock()) {
        *cache = Some((stamp, settings.clone()));
    }
    Ok(settings)
}
fn overrides(settings: &mut Settings) -> Result<Vec<&'static str>, String> {
    let mut locked = Vec::new();
    if let Ok(value) = std::env::var("ABV_WORKBUDDY_EGRESS_ENABLED") {
        settings.enabled = match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err("ABV_WORKBUDDY_EGRESS_ENABLED must be true or false".into()),
        };
        locked.push("enabled");
    }
    if let Ok(value) = std::env::var("ABV_WORKBUDDY_EGRESS_SUBSCRIPTION") {
        settings.subscription_url = value.trim().to_owned();
        locked.push("subscription_url");
    }
    if let Ok(value) = std::env::var("ABV_MIHOMO_PATH") {
        settings.mihomo_path = value;
    }
    Ok(locked)
}
fn validate(settings: &Settings) -> Result<(), String> {
    let url = settings.subscription_url.as_str();
    if !url.is_empty()
        && (url.len() > 4096
            || !url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")))
    {
        return Err("The subscription link must be an http(s) URL".into());
    }
    for pattern in [&settings.include, &settings.exclude] {
        if pattern.len() > 500 || regex::Regex::new(pattern).is_err() {
            return Err("Node filters must be valid regular expressions".into());
        }
    }
    if settings.bindings.len() > 1000
        || settings
            .bindings
            .iter()
            .any(|(k, v)| k.len() > 128 || v.is_empty() || v.len() > 512)
    {
        return Err("Invalid egress binding".into());
    }
    Ok(())
}
pub fn load() -> Result<Settings, String> {
    let mut settings = stored()?;
    overrides(&mut settings)?;
    validate(&settings)?;
    Ok(settings)
}
/// Read-modify-write of the stored file, never persisting environment overrides.
fn edit<T>(change: impl FnOnce(&mut Settings) -> Result<T, String>) -> Result<T, String> {
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| "WorkBuddy egress settings unavailable")?;
    let mut settings = stored()?;
    let result = change(&mut settings)?;
    validate(&settings)?;
    let data =
        serde_json::to_vec_pretty(&settings).map_err(|_| "Cannot serialize egress settings")?;
    crate::utils::fs::write_atomic(path()?, &data)
        .map_err(|_| "Cannot save WorkBuddy egress settings".to_string())?;
    Ok(result)
}

fn filters(settings: &Settings) -> (Option<regex::Regex>, Option<regex::Regex>) {
    let compile = |p: &str| (!p.is_empty()).then(|| regex::Regex::new(p).ok()).flatten();
    (compile(&settings.include), compile(&settings.exclude))
}
fn eligible(name: &str, include: &Option<regex::Regex>, exclude: &Option<regex::Regex>) -> bool {
    include.as_ref().is_none_or(|r| r.is_match(name))
        && exclude.as_ref().is_none_or(|r| !r.is_match(name))
}
/// Least-used eligible node; among equals prefer tested-alive, then lower delay.
/// Nodes whose last test failed are skipped.
fn choose(nodes: &[Node], settings: &Settings) -> Option<String> {
    let (include, exclude) = filters(settings);
    let mut load: HashMap<&str, usize> = HashMap::new();
    for node in settings.bindings.values() {
        *load.entry(node.as_str()).or_default() += 1;
    }
    nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.alive != Some(false) && eligible(&n.name, &include, &exclude))
        .min_by_key(|(index, n)| {
            (
                load.get(n.name.as_str()).copied().unwrap_or(0),
                n.alive != Some(true),
                n.delay.unwrap_or(u64::MAX),
                *index,
            )
        })
        .map(|(_, n)| n.name.clone())
}

fn direct() -> Result<reqwest::Client, String> {
    super::client::http().cloned()
}
fn proxied(proxy_url: &str) -> Result<reqwest::Client, String> {
    let mut clients = CLIENTS
        .lock()
        .map_err(|_| "Egress client cache unavailable")?;
    if let Some(client) = clients.get(proxy_url) {
        return Ok(client.clone());
    }
    let proxy = reqwest::Proxy::all(proxy_url).map_err(|_| "Invalid egress route")?;
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

/// The node an account must use, auto-assigning one when allowed.
async fn binding(settings: &Settings, account_id: &str) -> Result<String, String> {
    if let Some(node) = settings.bindings.get(account_id) {
        return Ok(node.clone());
    }
    if !settings.auto_assign {
        return Err(
            "No egress node is assigned to this WorkBuddy account; choose a node or DIRECT".into(),
        );
    }
    let _guard = ASSIGN_LOCK.lock().await;
    let settings = load()?;
    if let Some(node) = settings.bindings.get(account_id) {
        return Ok(node.clone());
    }
    let nodes = mihomo::nodes(&settings).await?;
    let node = choose(&nodes, &settings).ok_or("No usable egress node in the subscription")?;
    let id = account_id.to_owned();
    edit(|s| Ok(s.bindings.entry(id).or_insert(node).clone()))
}

/// HTTP client for one account's upstream calls.
pub async fn client_for(account: &Account) -> Result<reqwest::Client, String> {
    let settings = load()?;
    if !settings.enabled {
        return direct();
    }
    let node = binding(&settings, &account.id).await?;
    if node == DIRECT {
        return direct();
    }
    let route = mihomo::route(&settings, &account.id, &node).await?;
    proxied(&route.proxy_url)
}

/// A login request's route; holding it keeps the shared login selector on `node`.
pub struct LoginRoute {
    pub client: reqwest::Client,
    pub node: Option<String>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}
/// Login runs before the account exists: it uses `preferred` (the node chosen when
/// the login started) or the node auto-assignment would pick, so the account later
/// keeps the same exit it authorized from.
pub async fn login_route(preferred: Option<&str>) -> Result<LoginRoute, String> {
    let guard = LOGIN_LOCK.clone().lock_owned().await;
    let settings = load()?;
    if !settings.enabled {
        return Ok(LoginRoute {
            client: direct()?,
            node: None,
            _guard: guard,
        });
    }
    let node = match preferred {
        Some(node) => node.to_owned(),
        None => {
            let nodes = mihomo::nodes(&settings).await?;
            choose(&nodes, &settings).ok_or("No usable egress node in the subscription")?
        }
    };
    if node == DIRECT {
        return Ok(LoginRoute {
            client: direct()?,
            node: Some(node),
            _guard: guard,
        });
    }
    let route = mihomo::route(&settings, mihomo::LOGIN_USER, &node).await?;
    Ok(LoginRoute {
        client: proxied(&route.proxy_url)?,
        node: Some(node),
        _guard: guard,
    })
}
/// Keeps an authorized account on its login node unless it already has a binding.
pub fn adopt(account_id: &str, node: Option<&str>) {
    let Some(node) = node else { return };
    let id = account_id.to_owned();
    let node = node.to_owned();
    if let Err(error) = edit(|s| {
        s.bindings.entry(id).or_insert(node);
        Ok(())
    }) {
        tracing::warn!("WorkBuddy egress binding: {error}");
    }
}
pub fn forget(account_id: &str) {
    let id = account_id.to_owned();
    if let Err(error) = edit(|s| {
        s.bindings.remove(&id);
        Ok(())
    }) {
        tracing::warn!("WorkBuddy egress binding: {error}");
    }
}

pub async fn view() -> Result<View, String> {
    let mut settings = stored()?;
    let env_locked = overrides(&mut settings)?;
    validate(&settings)?;
    let (nodes, error) = if settings.enabled && !settings.subscription_url.is_empty() {
        match mihomo::nodes(&settings).await {
            Ok(nodes) => (nodes, None),
            Err(error) => (Vec::new(), Some(error)),
        }
    } else {
        (Vec::new(), None)
    };
    let status = mihomo::status().await;
    let ids: std::collections::HashSet<String> =
        accounts::all()?.into_iter().map(|a| a.id).collect();
    let bindings: BTreeMap<_, _> = settings
        .bindings
        .iter()
        .filter(|(id, _)| ids.contains(*id))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let (include, exclude) = filters(&settings);
    let nodes: Vec<NodeView> = nodes
        .into_iter()
        .map(|node| NodeView {
            eligible: eligible(&node.name, &include, &exclude),
            accounts: bindings.values().filter(|n| **n == node.name).count(),
            node,
        })
        .collect();
    Ok(View {
        enabled: settings.enabled,
        subscription_configured: !settings.subscription_url.is_empty(),
        subscription_host: url::Url::parse(&settings.subscription_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned)),
        include: settings.include,
        exclude: settings.exclude,
        auto_assign: settings.auto_assign,
        env_locked,
        running: status.running,
        binary: status.binary,
        // A startup error is stale once the subscription has produced nodes.
        error: error.or(status.error.filter(|_| nodes.is_empty())),
        nodes,
        bindings,
    })
}
pub async fn save(update: Update) -> Result<View, String> {
    let mut probe = stored()?;
    let locked = overrides(&mut probe)?;
    edit(|s| {
        if let Some(enabled) = update.enabled.filter(|_| !locked.contains(&"enabled")) {
            s.enabled = enabled;
        }
        if let Some(url) = update
            .subscription_url
            .filter(|_| !locked.contains(&"subscription_url"))
        {
            s.subscription_url = url.trim().to_owned();
        }
        if let Some(include) = update.include {
            s.include = include.trim().to_owned();
        }
        if let Some(exclude) = update.exclude {
            s.exclude = exclude.trim().to_owned();
        }
        if let Some(auto) = update.auto_assign {
            s.auto_assign = auto;
        }
        Ok(())
    })?;
    reconcile().await;
    view().await
}
async fn reconcile() {
    match load() {
        Ok(settings) if settings.enabled && !settings.subscription_url.is_empty() => {
            mihomo::supervise(&settings).await
        }
        Ok(_) => mihomo::stop().await,
        Err(error) => tracing::warn!("WorkBuddy egress settings: {error}"),
    }
}
/// `None` clears the binding (auto-assignment picks again), `DIRECT` bypasses egress.
pub async fn set_account(account_id: &str, node: Option<String>) -> Result<View, String> {
    accounts::get(account_id)?;
    let settings = load()?;
    if let Some(name) = node.as_deref().filter(|n| *n != DIRECT) {
        let nodes = mihomo::nodes(&settings).await?;
        if !nodes.iter().any(|n| n.name == name) {
            return Err("This node is not in the current subscription".into());
        }
    }
    let id = account_id.to_owned();
    edit(|s| {
        match node {
            Some(node) => s.bindings.insert(id, node),
            None => s.bindings.remove(&id),
        };
        Ok(())
    })?;
    mihomo::forget_selection(account_id).await;
    let settings = load()?;
    if settings.enabled && !settings.bindings.contains_key(account_id) && settings.auto_assign {
        binding(&settings, account_id).await?;
    }
    view().await
}
fn probe_url(realm: Realm) -> String {
    format!("{}/", super::client::base(realm))
}
/// Latency from this server to the account's WorkBuddy endpoint along its real path.
pub async fn test_account(account_id: &str) -> Result<Probe, String> {
    let account = accounts::get(account_id)?;
    let settings = load()?;
    let node = if settings.enabled {
        binding(&settings, account_id).await?
    } else {
        DIRECT.to_owned()
    };
    let url = probe_url(account.realm);
    let result = if node == DIRECT {
        let started = std::time::Instant::now();
        direct()?
            .head(&url)
            .timeout(PROBE_URL_TIMEOUT)
            .send()
            .await
            .map(|_| started.elapsed().as_millis() as u64)
            .map_err(|_| "WorkBuddy is unreachable from this server".to_string())
    } else {
        mihomo::delay(&settings, account_id, &node, &url).await
    };
    Ok(match result {
        Ok(delay) => Probe {
            node,
            delay_ms: Some(delay.max(1)),
            error: None,
        },
        Err(error) => Probe {
            node,
            delay_ms: None,
            error: Some(error),
        },
    })
}
pub async fn test_nodes() -> Result<View, String> {
    mihomo::test_all(&load()?).await?;
    view().await
}
pub async fn refresh() -> Result<View, String> {
    mihomo::refresh(&load()?).await?;
    view().await
}

/// Keeps mihomo running while egress is configured and restarts it after a crash.
pub fn start() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    tauri::async_runtime::spawn(async {
        loop {
            reconcile().await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, delay: Option<u64>, alive: Option<bool>) -> Node {
        Node {
            name: name.into(),
            kind: "Trojan".into(),
            delay,
            alive,
        }
    }

    #[test]
    fn auto_assignment_spreads_accounts_and_skips_placeholders_and_dead_nodes() {
        let nodes = [
            node("剩余流量：100GB", None, None),
            node("HK 01", Some(80), Some(true)),
            node("JP 02", Some(40), Some(true)),
            node("SG 03", None, Some(false)),
        ];
        let mut settings = Settings::default();
        assert_eq!(choose(&nodes, &settings).as_deref(), Some("JP 02"));
        settings.bindings.insert("a".into(), "JP 02".into());
        assert_eq!(choose(&nodes, &settings).as_deref(), Some("HK 01"));
        settings.bindings.insert("b".into(), "HK 01".into());
        // Every eligible node is used once: share the fastest rather than fail.
        assert_eq!(choose(&nodes, &settings).as_deref(), Some("JP 02"));
        settings.include = "^HK".into();
        assert_eq!(choose(&nodes, &settings).as_deref(), Some("HK 01"));
        settings.include = "^SG".into();
        assert_eq!(choose(&nodes, &settings), None);
    }

    #[test]
    fn untested_nodes_are_usable_but_rank_after_tested_ones() {
        let nodes = [node("A", None, None), node("B", Some(300), Some(true))];
        assert_eq!(choose(&nodes, &Settings::default()).as_deref(), Some("B"));
        let nodes = [node("A", None, None)];
        assert_eq!(choose(&nodes, &Settings::default()).as_deref(), Some("A"));
    }

    #[test]
    fn settings_reject_non_http_links_and_bad_filters() {
        let mut settings = Settings {
            subscription_url: "ftp://x".into(),
            ..Default::default()
        };
        assert!(validate(&settings).is_err());
        settings.subscription_url = "https://sub.example/api?token=x".into();
        assert!(validate(&settings).is_ok());
        settings.include = "(".into();
        assert!(validate(&settings).is_err());
    }

    #[test]
    fn bindings_persist_without_environment_overrides() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        adopt("acct", Some("HK 01"));
        adopt("acct", Some("JP 02"));
        assert_eq!(load().unwrap().bindings["acct"], "HK 01");
        forget("acct");
        assert!(load().unwrap().bindings.is_empty());
    }

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
