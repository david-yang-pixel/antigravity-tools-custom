//! Per-account egress shared by the native account pools (WorkBuddy and the native
//! providers such as Qoder). With a pool's egress enabled, each of its accounts'
//! upstream traffic (inference, refresh, models, quota, login) leaves through one node
//! of an airport subscription, served by a managed mihomo process. An account bound to
//! a node fails closed when that node is unusable: it never falls back to the server's
//! own address, and it is never moved onto another node.
//!
//! All pools share one subscription and one mihomo process; each pool has its own
//! switch and bindings. A node is exclusive within a pool: auto-assignment and manual
//! assignment both refuse a node another account of the same pool already holds. Pools
//! talk to unrelated upstream services, so a WorkBuddy account and a Qoder account may
//! use the same node. The exit address is whatever that node currently uses; this
//! process does not allocate public addresses. `DIRECT` is an explicit per-account
//! choice and shares this server's address.
//!
//! This module only decides routes ([`Exit`]); each pool builds its own HTTP clients so
//! its TLS profile and per-account connection pools stay its own.
mod mihomo;

pub use mihomo::Node;

use crate::modules::native_providers::Provider;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};

pub const DIRECT: &str = "DIRECT";
/// Airport placeholder entries (quota, expiry, website) are not real exits.
const DEFAULT_EXCLUDE: &str =
    r"(?i)(剩余|流量|到期|过期|套餐|官网|网址|expire|traffic|remaining|reset)";

/// An account pool with its own egress switch and node bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pool {
    WorkBuddy,
    Native(Provider),
}
impl Pool {
    pub fn all() -> impl Iterator<Item = Pool> {
        std::iter::once(Pool::WorkBuddy).chain(Provider::ALL.into_iter().map(Pool::Native))
    }
    pub fn key(self) -> &'static str {
        match self {
            Pool::WorkBuddy => "workbuddy",
            Pool::Native(provider) => provider.key(),
        }
    }
    /// mihomo routing user of one account. WorkBuddy keeps bare account ids, which
    /// predate other pools; other pools prefix their key so ids can never collide.
    fn user(self, account_id: &str) -> String {
        match self {
            Pool::WorkBuddy => account_id.to_owned(),
            Pool::Native(_) => format!("{}-{account_id}", self.key()),
        }
    }
    /// Routing user for logins, which run before the account exists.
    fn login_user(self) -> String {
        match self {
            Pool::WorkBuddy => "login".into(),
            Pool::Native(_) => format!("{}-login", self.key()),
        }
    }
    fn enabled_env(self) -> String {
        match self {
            Pool::WorkBuddy => "ABV_WORKBUDDY_EGRESS_ENABLED".into(),
            Pool::Native(provider) => provider.env("EGRESS_ENABLED"),
        }
    }
    fn account_ids(self) -> Result<Vec<String>, String> {
        Ok(match self {
            Pool::WorkBuddy => crate::modules::workbuddy::accounts::all()?
                .into_iter()
                .map(|a| a.id)
                .collect(),
            Pool::Native(provider) => crate::modules::native_providers::store::all(provider)?
                .into_iter()
                .map(|a| a.id)
                .collect(),
        })
    }
}

/// Every routing user mihomo must know: each pool's accounts plus its login user.
fn all_users() -> HashSet<String> {
    let mut users = HashSet::new();
    for pool in Pool::all() {
        users.insert(pool.login_user());
        match pool.account_ids() {
            Ok(ids) => users.extend(ids.iter().map(|id| pool.user(id))),
            Err(error) => tracing::warn!("Egress users for {}: {error}", pool.key()),
        }
    }
    users
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolSettings {
    pub enabled: bool,
    /// Account id -> node name, or `DIRECT`.
    pub bindings: BTreeMap<String, String>,
}

/// Stored in `workbuddy_egress.json`, the file WorkBuddy's egress started with. The
/// WorkBuddy pool keeps the top-level `enabled` and `bindings`; other pools live under
/// `pools`, so older versions still read this file.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub subscription_url: String,
    /// Auto-assignment only considers nodes matching `include` and not `exclude`.
    pub include: String,
    pub exclude: String,
    /// New or unbound accounts take one unused healthy node on first use.
    pub auto_assign: bool,
    /// Executable to run. Config file / `ABV_MIHOMO_PATH` only: the admin API can
    /// never choose what the server executes.
    pub mihomo_path: String,
    /// WorkBuddy account id -> node name, or `DIRECT`.
    pub bindings: BTreeMap<String, String>,
    /// Other pools by [`Pool::key`].
    pub pools: BTreeMap<String, PoolSettings>,
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
            pools: BTreeMap::new(),
        }
    }
}
static NO_BINDINGS: BTreeMap<String, String> = BTreeMap::new();
impl Settings {
    pub fn enabled(&self, pool: Pool) -> bool {
        match pool {
            Pool::WorkBuddy => self.enabled,
            _ => self.pools.get(pool.key()).is_some_and(|p| p.enabled),
        }
    }
    fn set_enabled(&mut self, pool: Pool, enabled: bool) {
        match pool {
            Pool::WorkBuddy => self.enabled = enabled,
            _ => self.pools.entry(pool.key().into()).or_default().enabled = enabled,
        }
    }
    pub fn bindings(&self, pool: Pool) -> &BTreeMap<String, String> {
        match pool {
            Pool::WorkBuddy => &self.bindings,
            _ => self
                .pools
                .get(pool.key())
                .map_or(&NO_BINDINGS, |p| &p.bindings),
        }
    }
    fn bindings_mut(&mut self, pool: Pool) -> &mut BTreeMap<String, String> {
        match pool {
            Pool::WorkBuddy => &mut self.bindings,
            _ => &mut self.pools.entry(pool.key().into()).or_default().bindings,
        }
    }
    /// mihomo runs while any pool routes through the subscription.
    fn routing(&self) -> bool {
        !self.subscription_url.is_empty() && Pool::all().any(|pool| self.enabled(pool))
    }
}

#[derive(Default, Deserialize)]
pub struct Update {
    /// This pool's switch.
    pub enabled: Option<bool>,
    /// Empty string clears the link; omitted keeps it. Shared by every pool.
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
    /// Accounts of this pool bound to the node.
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

/// Where one account's (or one login's) traffic leaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// This pool's egress is off: the pool's usual outbound path.
    Off,
    /// The account was explicitly set to `DIRECT`: this server's own address.
    Direct,
    /// Loopback SOCKS route through the account's own selector.
    Proxy { url: String, node: String },
}

static SETTINGS_LOCK: Mutex<()> = Mutex::new(());
static STORED: Mutex<Option<(crate::utils::fs::FileStamp, Settings)>> = Mutex::new(None);
/// Serializes auto-assignment so two first requests cannot take the same node.
static ASSIGN_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);
/// Node reserved by an in-progress authorization, keyed by hold token: (pool key, node,
/// time the reservation lapses). Released when the login ends; a hold whose login was
/// abandoned stops counting at that time even if nothing drops it.
static HOLDS: LazyLock<Mutex<HashMap<String, (&'static str, String, i64)>>> =
    LazyLock::new(Default::default);
/// Covers the authorization-state request before the session sets its own expiry.
const HOLD_GRACE_SECS: i64 = 120;
/// Login runs before an account exists and shares one selector per pool; one login
/// at a time within a pool.
static LOGIN_LOCKS: LazyLock<Mutex<HashMap<&'static str, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
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
        Err(_) => return Err("Cannot read egress settings".into()),
    };
    if let (Some(stamp), Ok(mut cache)) = (stamp, STORED.lock()) {
        *cache = Some((stamp, settings.clone()));
    }
    Ok(settings)
}

/// Fields fixed by environment variables.
#[derive(Default)]
struct Locks {
    subscription: bool,
    enabled: HashSet<&'static str>,
}
impl Locks {
    fn fields(&self, pool: Pool) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.enabled.contains(pool.key()) {
            fields.push("enabled");
        }
        if self.subscription {
            fields.push("subscription_url");
        }
        fields
    }
}
fn overrides(settings: &mut Settings) -> Result<Locks, String> {
    let mut locks = Locks::default();
    for pool in Pool::all() {
        let name = pool.enabled_env();
        if let Ok(value) = std::env::var(&name) {
            let enabled = match value.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return Err(format!("{name} must be true or false")),
            };
            settings.set_enabled(pool, enabled);
            locks.enabled.insert(pool.key());
        }
    }
    // The subscription is shared; the WorkBuddy name predates the other pools.
    if let Ok(value) = std::env::var("ABV_EGRESS_SUBSCRIPTION")
        .or_else(|_| std::env::var("ABV_WORKBUDDY_EGRESS_SUBSCRIPTION"))
    {
        settings.subscription_url = value.trim().to_owned();
        locks.subscription = true;
    }
    if let Ok(value) = std::env::var("ABV_MIHOMO_PATH") {
        settings.mihomo_path = value;
    }
    Ok(locks)
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
    let invalid = |bindings: &BTreeMap<String, String>| {
        bindings.len() > 1000
            || bindings
                .iter()
                .any(|(k, v)| k.len() > 128 || v.is_empty() || v.len() > 512)
    };
    if settings.pools.len() > 16
        || invalid(&settings.bindings)
        || settings.pools.values().any(|p| invalid(&p.bindings))
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
        .map_err(|_| "Egress settings unavailable")?;
    let mut settings = stored()?;
    let result = change(&mut settings)?;
    validate(&settings)?;
    let data =
        serde_json::to_vec_pretty(&settings).map_err(|_| "Cannot serialize egress settings")?;
    crate::utils::fs::write_atomic(path()?, &data)
        .map_err(|_| "Cannot save egress settings".to_string())?;
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
fn held_nodes(pool: Pool) -> HashSet<String> {
    let now = now();
    HOLDS
        .lock()
        .map(|mut holds| {
            holds.retain(|_, (_, _, until)| *until > now);
            holds
                .values()
                .filter(|(key, _, _)| *key == pool.key())
                .map(|(_, node, _)| node.clone())
                .collect()
        })
        .unwrap_or_default()
}
/// `true` when another saved account of this pool already has this node.
/// `DIRECT` is not a subscription node and may be selected for more than one account.
fn bound_elsewhere(settings: &Settings, pool: Pool, node: &str, except: Option<&str>) -> bool {
    if node == DIRECT {
        return false;
    }
    settings
        .bindings(pool)
        .iter()
        .any(|(id, bound)| bound == node && Some(id.as_str()) != except)
}
/// `true` when another account or an in-progress login of this pool already has this node.
fn node_in_use(settings: &Settings, pool: Pool, node: &str, except: Option<&str>) -> bool {
    bound_elsewhere(settings, pool, node, except) || held_nodes(pool).contains(node)
}
/// One node no account of this pool uses. Tested-alive nodes win, then lower delay.
/// Nodes whose last test failed are skipped. A node already given to an account of
/// this pool is never handed out again.
fn choose(nodes: &[Node], settings: &Settings, pool: Pool) -> Option<String> {
    let (include, exclude) = filters(settings);
    let held = held_nodes(pool);
    let used: HashSet<&str> = settings
        .bindings(pool)
        .values()
        .filter(|name| name.as_str() != DIRECT)
        .map(String::as_str)
        .collect();
    nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.is_subscription_proxy()
                && n.alive != Some(false)
                && eligible(&n.name, &include, &exclude)
                && !used.contains(n.name.as_str())
                && !held.contains(&n.name)
        })
        .min_by_key(|(index, n)| (n.alive != Some(true), n.delay.unwrap_or(u64::MAX), *index))
        .map(|(_, n)| n.name.clone())
}

/// Keeps a subscription node off the pool's free list until authorization finishes.
pub struct LoginHold {
    token: String,
    active: bool,
}
impl LoginHold {
    fn reserve(pool: Pool, node: &str) -> Result<Self, String> {
        let token = uuid::Uuid::new_v4().simple().to_string();
        HOLDS
            .lock()
            .map_err(|_| "Egress reservation unavailable")?
            .insert(
                token.clone(),
                (pool.key(), node.to_owned(), now() + HOLD_GRACE_SECS),
            );
        Ok(Self {
            token,
            active: true,
        })
    }
    /// Keeps the reservation until `at`, the end of the authorization session.
    pub fn hold_until(&mut self, at: i64) {
        if !self.active {
            return;
        }
        if let Ok(mut holds) = HOLDS.lock() {
            if let Some((_, _, until)) = holds.get_mut(&self.token) {
                *until = at;
            }
        }
    }
    pub fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if let Ok(mut holds) = HOLDS.lock() {
            holds.remove(&self.token);
        }
    }
}
impl Drop for LoginHold {
    fn drop(&mut self) {
        self.release();
    }
}

/// The node an account must use, auto-assigning one when allowed.
/// An existing binding is never moved. A dead node fails this account; it does
/// not send the same machine identity out through another address.
async fn binding(settings: &Settings, pool: Pool, account_id: &str) -> Result<String, String> {
    if let Some(node) = settings.bindings(pool).get(account_id) {
        return Ok(node.clone());
    }
    if !settings.auto_assign {
        return Err("No egress node is assigned to this account; choose a node or DIRECT".into());
    }
    let _guard = ASSIGN_LOCK.lock().await;
    let settings = load()?;
    if let Some(node) = settings.bindings(pool).get(account_id) {
        return Ok(node.clone());
    }
    let nodes = mihomo::nodes(&settings).await?;
    let node =
        choose(&nodes, &settings, pool).ok_or("No free egress node is left in the subscription")?;
    let id = account_id.to_owned();
    edit(|s| {
        if let Some(existing) = s.bindings(pool).get(&id) {
            return Ok(existing.clone());
        }
        if node_in_use(s, pool, &node, None) {
            return Err("No free egress node is left in the subscription".into());
        }
        Ok(s.bindings_mut(pool).entry(id).or_insert(node).clone())
    })
}

/// Route of one saved account's upstream calls.
pub async fn exit_for(pool: Pool, account_id: &str) -> Result<Exit, String> {
    let settings = load()?;
    if !settings.enabled(pool) {
        return Ok(Exit::Off);
    }
    let node = binding(&settings, pool, account_id).await?;
    if node == DIRECT {
        return Ok(Exit::Direct);
    }
    let route = mihomo::route(&settings, &pool.user(account_id), &node).await?;
    Ok(Exit::Proxy {
        url: route.proxy_url,
        node,
    })
}

/// A login's route; holding it keeps the pool's login selector on `node`.
pub struct LoginRoute {
    pub exit: Exit,
    pub node: Option<String>,
    /// Set when this login claimed a free subscription node. Move it into the
    /// authorization session so the node stays reserved until the account is bound.
    pub hold: Option<LoginHold>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}
fn login_lock(pool: Pool) -> Result<Arc<tokio::sync::Mutex<()>>, String> {
    Ok(LOGIN_LOCKS
        .lock()
        .map_err(|_| "Egress login unavailable")?
        .entry(pool.key())
        .or_default()
        .clone())
}
/// Login runs before the account exists: it uses `preferred` (the node chosen when
/// the login started, or a saved account's own binding) or the node auto-assignment
/// would pick, so the account later keeps the same exit it authorized from.
pub async fn login_route(pool: Pool, preferred: Option<&str>) -> Result<LoginRoute, String> {
    let guard = login_lock(pool)?.lock_owned().await;
    let settings = load()?;
    if !settings.enabled(pool) {
        return Ok(LoginRoute {
            exit: Exit::Off,
            node: None,
            hold: None,
            _guard: guard,
        });
    }
    let (node, hold) = match preferred {
        Some(node) => (node.to_owned(), None),
        None => {
            let _assign = ASSIGN_LOCK.lock().await;
            let settings = load()?;
            let nodes = mihomo::nodes(&settings).await?;
            let node = choose(&nodes, &settings, pool)
                .ok_or("No free egress node is left in the subscription")?;
            let hold = LoginHold::reserve(pool, &node)?;
            (node, Some(hold))
        }
    };
    if node == DIRECT {
        return Ok(LoginRoute {
            exit: Exit::Direct,
            node: Some(DIRECT.to_owned()),
            hold: None,
            _guard: guard,
        });
    }
    let route = mihomo::route(&settings, &pool.login_user(), &node).await?;
    Ok(LoginRoute {
        exit: Exit::Proxy {
            url: route.proxy_url,
            node: node.clone(),
        },
        node: Some(node),
        hold,
        _guard: guard,
    })
}
/// The node an existing account must reauthorize through: its own binding, so the
/// new token is issued to the address that already carries this account. `None`
/// when the pool's egress is off or the account has no binding yet.
pub fn bound_node(pool: Pool, account_id: &str) -> Result<Option<String>, String> {
    let settings = load()?;
    if !settings.enabled(pool) {
        return Ok(None);
    }
    Ok(settings.bindings(pool).get(account_id).cloned())
}
/// Keeps an authorized account on its login node unless it already has a binding.
/// A node already given to a different account of the pool is left untouched.
pub fn adopt(pool: Pool, account_id: &str, node: Option<&str>) {
    let Some(node) = node else { return };
    let id = account_id.to_owned();
    let node = node.to_owned();
    if let Err(error) = edit(|s| {
        if s.bindings(pool).contains_key(&id) || bound_elsewhere(s, pool, &node, Some(&id)) {
            return Ok(());
        }
        s.bindings_mut(pool).insert(id, node);
        Ok(())
    }) {
        tracing::warn!("{} egress binding: {error}", pool.key());
    }
}
pub fn forget(pool: Pool, account_id: &str) {
    let id = account_id.to_owned();
    if let Err(error) = edit(|s| {
        s.bindings_mut(pool).remove(&id);
        Ok(())
    }) {
        tracing::warn!("{} egress binding: {error}", pool.key());
    }
}

pub async fn view(pool: Pool) -> Result<View, String> {
    let mut settings = stored()?;
    let locks = overrides(&mut settings)?;
    validate(&settings)?;
    let (nodes, error) = if settings.enabled(pool) && !settings.subscription_url.is_empty() {
        match mihomo::nodes(&settings).await {
            Ok(nodes) => (nodes, None),
            Err(error) => (Vec::new(), Some(error)),
        }
    } else {
        (Vec::new(), None)
    };
    let status = mihomo::status().await;
    let ids: HashSet<String> = pool.account_ids()?.into_iter().collect();
    let bindings: BTreeMap<_, _> = settings
        .bindings(pool)
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
        enabled: settings.enabled(pool),
        subscription_configured: !settings.subscription_url.is_empty(),
        subscription_host: url::Url::parse(&settings.subscription_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned)),
        include: settings.include,
        exclude: settings.exclude,
        auto_assign: settings.auto_assign,
        env_locked: locks.fields(pool),
        running: status.running,
        binary: status.binary,
        // A startup error is stale once the subscription has produced nodes.
        error: error.or(status.error.filter(|_| nodes.is_empty())),
        nodes,
        bindings,
    })
}
pub async fn save(pool: Pool, update: Update) -> Result<View, String> {
    let mut probe = stored()?;
    let locked = overrides(&mut probe)?.fields(pool);
    edit(|s| {
        if let Some(enabled) = update.enabled.filter(|_| !locked.contains(&"enabled")) {
            s.set_enabled(pool, enabled);
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
    view(pool).await
}
async fn reconcile() {
    match load() {
        Ok(settings) if settings.routing() => mihomo::supervise(&settings).await,
        Ok(_) => mihomo::stop().await,
        Err(error) => tracing::warn!("Egress settings: {error}"),
    }
}
/// `None` clears the binding (auto-assignment picks again), `DIRECT` bypasses egress.
pub async fn set_account(
    pool: Pool,
    account_id: &str,
    node: Option<String>,
) -> Result<View, String> {
    if !pool.account_ids()?.iter().any(|id| id == account_id) {
        return Err("Account no longer exists".into());
    }
    let settings = load()?;
    if let Some(name) = node.as_deref().filter(|n| *n != DIRECT) {
        let nodes = mihomo::nodes(&settings).await?;
        if !nodes.iter().any(|n| n.name == name) {
            return Err("This node is not in the current subscription".into());
        }
    }
    let id = account_id.to_owned();
    let assigned = node.clone();
    edit(|s| {
        if let Some(name) = assigned.as_deref().filter(|name| *name != DIRECT) {
            if node_in_use(s, pool, name, Some(&id)) {
                return Err("This egress node is already in use".into());
            }
        }
        match assigned {
            Some(node) => {
                s.bindings_mut(pool).insert(id, node);
            }
            None => {
                s.bindings_mut(pool).remove(&id);
            }
        }
        Ok(())
    })?;
    mihomo::forget_selection(&pool.user(account_id)).await;
    let settings = load()?;
    if settings.enabled(pool)
        && !settings.bindings(pool).contains_key(account_id)
        && settings.auto_assign
    {
        binding(&settings, pool, account_id).await?;
    }
    view(pool).await
}
/// Latency from this server to `url` along the account's real path. `direct` measures
/// the path the pool uses without a node (egress off or `DIRECT`).
pub async fn test_account<F, Fut>(
    pool: Pool,
    account_id: &str,
    url: &str,
    direct: F,
) -> Result<Probe, String>
where
    F: FnOnce(Exit) -> Fut,
    Fut: std::future::Future<Output = Result<u64, String>>,
{
    let settings = load()?;
    let node = if settings.enabled(pool) {
        binding(&settings, pool, account_id).await?
    } else {
        DIRECT.to_owned()
    };
    let result = if node == DIRECT {
        direct(if settings.enabled(pool) {
            Exit::Direct
        } else {
            Exit::Off
        })
        .await
    } else {
        mihomo::delay(&settings, &pool.user(account_id), &node, url).await
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
pub async fn test_nodes(pool: Pool) -> Result<View, String> {
    mihomo::test_all(&load()?).await?;
    view(pool).await
}
pub async fn refresh(pool: Pool) -> Result<View, String> {
    mihomo::refresh(&load()?).await?;
    view(pool).await
}

/// Keeps mihomo running while any pool routes through the subscription and restarts
/// it after a crash.
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

    const WB: Pool = Pool::WorkBuddy;
    const QODER: Pool = Pool::Native(Provider::Qoder);

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
        assert_eq!(choose(&nodes, &settings, WB).as_deref(), Some("JP 02"));
        settings.bindings.insert("a".into(), "JP 02".into());
        assert_eq!(choose(&nodes, &settings, WB).as_deref(), Some("HK 01"));
        settings.bindings.insert("b".into(), "HK 01".into());
        assert_eq!(choose(&nodes, &settings, WB), None);
        settings.include = "^HK".into();
        assert_eq!(choose(&nodes, &settings, WB), None);
        settings.bindings.remove("b");
        assert_eq!(choose(&nodes, &settings, WB).as_deref(), Some("HK 01"));
        settings.include = "^SG".into();
        assert_eq!(choose(&nodes, &settings, WB), None);
    }

    #[test]
    fn auto_assignment_never_chooses_a_subscription_direct_entry() {
        let direct = node(DIRECT, Some(1), Some(true));
        let mixed_case = node(" direct ", Some(1), Some(true));
        let mut renamed = node("fast-direct-node", Some(1), Some(true));
        renamed.kind = "Direct".into();
        // Even explicit include filters cannot turn a subscription entry into
        // the administrator's direct-route selection.
        let settings = Settings {
            include: ".*".into(),
            exclude: String::new(),
            ..Default::default()
        };
        for pool in [WB, QODER] {
            for rejected in [&direct, &mixed_case, &renamed] {
                assert_eq!(
                    choose(std::slice::from_ref(rejected), &settings, pool),
                    None
                );
                assert_eq!(
                    choose(
                        &[rejected.clone(), node("proxy", Some(50), Some(true))],
                        &settings,
                        pool
                    )
                    .as_deref(),
                    Some("proxy")
                );
            }
        }
    }

    #[test]
    fn nodes_are_exclusive_within_a_pool_but_not_across_pools() {
        let nodes = [node("JP 02", Some(40), Some(true))];
        let mut settings = Settings::default();
        settings.bindings.insert("wb".into(), "JP 02".into());
        assert_eq!(choose(&nodes, &settings, WB), None);
        assert_eq!(choose(&nodes, &settings, QODER).as_deref(), Some("JP 02"));
        assert!(!node_in_use(&settings, QODER, "JP 02", None));
        settings
            .bindings_mut(QODER)
            .insert("q1".into(), "JP 02".into());
        assert!(node_in_use(&settings, QODER, "JP 02", Some("q2")));
        assert!(!node_in_use(&settings, QODER, "JP 02", Some("q1")));
        assert_eq!(choose(&nodes, &settings, QODER), None);
        assert_eq!(settings.bindings(WB).len(), 1);
    }

    #[test]
    fn pool_switches_are_independent_and_the_workbuddy_layout_is_kept() {
        let mut settings = Settings::default();
        settings.set_enabled(QODER, true);
        assert!(settings.enabled(QODER) && !settings.enabled(WB));
        settings.subscription_url = "https://sub.example/x".into();
        assert!(settings.routing());
        settings.set_enabled(QODER, false);
        assert!(!settings.routing());
        settings.set_enabled(WB, true);
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["enabled"], true);
        assert_eq!(value["pools"]["qoder"]["enabled"], false);
        // A file written before pools existed still loads.
        let legacy: Settings =
            serde_json::from_value(serde_json::json!({"enabled": true, "bindings": {"a": "HK"}}))
                .unwrap();
        assert_eq!(legacy.bindings(WB)["a"], "HK");
        assert!(legacy.bindings(QODER).is_empty());
    }

    #[test]
    fn routing_users_never_collide_across_pools() {
        assert_eq!(WB.user("abc"), "abc");
        assert_eq!(QODER.user("abc"), "qoder-abc");
        assert_ne!(WB.login_user(), QODER.login_user());
    }

    #[test]
    fn direct_bindings_do_not_occupy_a_subscription_node() {
        let nodes = [node("JP 02", Some(40), Some(true))];
        let mut settings = Settings::default();
        settings.bindings.insert("a".into(), DIRECT.into());
        assert!(!node_in_use(&settings, WB, DIRECT, Some("b")));
        assert_eq!(choose(&nodes, &settings, WB).as_deref(), Some("JP 02"));
        settings.bindings.insert("b".into(), "JP 02".into());
        assert!(node_in_use(&settings, WB, "JP 02", Some("c")));
        assert!(!node_in_use(&settings, WB, "JP 02", Some("b")));
    }

    #[test]
    fn an_authorization_hold_keeps_its_node_off_the_free_list() {
        let nodes = [
            node("hold-fast", Some(10), Some(true)),
            node("hold-slow", Some(90), Some(true)),
        ];
        let mut hold = LoginHold::reserve(WB, "hold-fast").unwrap();
        assert_eq!(
            choose(&nodes, &Settings::default(), WB).as_deref(),
            Some("hold-slow")
        );
        assert!(node_in_use(&Settings::default(), WB, "hold-fast", None));
        // Another pool's login does not see this pool's hold.
        assert_eq!(
            choose(&nodes, &Settings::default(), QODER).as_deref(),
            Some("hold-fast")
        );
        hold.release();
        assert_eq!(
            choose(&nodes, &Settings::default(), WB).as_deref(),
            Some("hold-fast")
        );
    }

    #[test]
    fn an_abandoned_authorization_hold_lapses_at_its_deadline() {
        let nodes = [node("lapse-only", Some(10), Some(true))];
        let mut hold = LoginHold::reserve(WB, "lapse-only").unwrap();
        assert_eq!(choose(&nodes, &Settings::default(), WB), None);
        // The session ended without release (e.g. the admin closed the dialog).
        hold.hold_until(now() - 1);
        assert_eq!(
            choose(&nodes, &Settings::default(), WB).as_deref(),
            Some("lapse-only")
        );
        std::mem::forget(hold);
    }

    #[test]
    fn untested_nodes_are_usable_but_rank_after_tested_ones() {
        let nodes = [node("A", None, None), node("B", Some(300), Some(true))];
        assert_eq!(
            choose(&nodes, &Settings::default(), WB).as_deref(),
            Some("B")
        );
        let nodes = [node("A", None, None)];
        assert_eq!(
            choose(&nodes, &Settings::default(), WB).as_deref(),
            Some("A")
        );
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
        adopt(WB, "acct", Some("HK 01"));
        adopt(WB, "acct", Some("JP 02"));
        assert_eq!(load().unwrap().bindings["acct"], "HK 01");
        forget(WB, "acct");
        assert!(load().unwrap().bindings.is_empty());
        adopt(WB, "one", Some("node-a"));
        adopt(WB, "two", Some("node-a"));
        adopt(QODER, "three", Some("node-a"));
        let settings = load().unwrap();
        assert_eq!(
            settings.bindings(WB).get("one").map(String::as_str),
            Some("node-a")
        );
        assert!(settings.bindings(WB).get("two").is_none());
        assert_eq!(
            settings.bindings(QODER).get("three").map(String::as_str),
            Some("node-a")
        );
    }

    #[tokio::test]
    async fn a_disabled_pool_keeps_its_usual_path() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        assert_eq!(exit_for(QODER, "any").await.unwrap(), Exit::Off);
        let route = login_route(QODER, None).await.unwrap();
        assert!(route.node.is_none());
        assert_eq!(route.exit, Exit::Off);
    }
}
