//! Managed mihomo process: one authenticated loopback SOCKS listener, one selector
//! group per account (`IN-USER` routes each account to its own group), and the
//! subscription loaded as a proxy provider so mihomo owns every airport protocol.
use super::Settings;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

pub(super) const PROVIDER: &str = "sub";
pub(super) const ALL_GROUP: &str = "wb-all";
pub(super) const LOGIN_USER: &str = "login";
const START_TIMEOUT: Duration = Duration::from_secs(15);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
const RESTART_BACKOFF: Duration = Duration::from_secs(15);

struct Runtime {
    child: tokio::process::Child,
    controller: String,
    secret: String,
    socks_port: u16,
    /// Routing user -> SOCKS password; a user is an account id or `LOGIN_USER`.
    users: HashMap<String, String>,
    fingerprint: String,
    /// (user, node) pairs whose selector is confirmed on that node in this config.
    verified: HashSet<(String, String)>,
}
#[derive(Default)]
struct State {
    runtime: Option<Runtime>,
    last_error: Option<String>,
    binary: Option<PathBuf>,
    retry_at: Option<tokio::time::Instant>,
}
static STATE: LazyLock<tokio::sync::Mutex<State>> = LazyLock::new(Default::default);

#[derive(Clone, Debug, serde::Serialize)]
pub struct Node {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Last measured delay in ms; `None` when never tested.
    pub delay: Option<u64>,
    /// `Some(false)` when the last test failed.
    pub alive: Option<bool>,
}

pub(super) fn group(user: &str) -> String {
    format!("wb-{user}")
}
fn dir() -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join("egress"))
}
fn exe_name() -> &'static str {
    if cfg!(windows) {
        "mihomo.exe"
    } else {
        "mihomo"
    }
}
/// Configured path, then the data directory, next to this executable, then PATH.
pub(super) fn resolve_binary(configured: &str) -> Result<PathBuf, String> {
    if !configured.trim().is_empty() {
        let path = PathBuf::from(configured.trim());
        return if path.is_file() {
            Ok(path)
        } else {
            Err("Configured mihomo path does not exist".into())
        };
    }
    let mut candidates = vec![dir()?.join("bin").join(exe_name())];
    if let Some(parent) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(parent.join(exe_name()));
    }
    if let Some(paths) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&paths).map(|p| p.join(exe_name())));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| "mihomo was not found; install it in PATH, place it in <data>/egress/bin, or set ABV_MIHOMO_PATH".into())
}
fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|_| "Cannot allocate a loopback port for mihomo".into())
}
fn random_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
/// What requires a fresh process; compares configuration, not filesystem lookups,
/// so the inference hot path never scans PATH.
fn fingerprint(settings: &Settings) -> String {
    format!("{}\n{}", settings.subscription_url, settings.mihomo_path)
}

pub(super) fn config(
    subscription: &str,
    controller_port: u16,
    secret: &str,
    socks_port: u16,
    users: &HashMap<String, String>,
) -> Value {
    let mut names: Vec<_> = users.keys().collect();
    names.sort();
    let mut groups = vec![json!({"name": ALL_GROUP, "type": "select", "use": [PROVIDER]})];
    let mut rules = Vec::new();
    for user in &names {
        groups.push(json!({"name": group(user), "type": "select", "use": [PROVIDER]}));
        rules.push(json!(format!("IN-USER,{user},{}", group(user))));
    }
    // Anything that is not an authenticated account must never leave directly.
    rules.push(json!("MATCH,REJECT"));
    json!({
        "mode": "rule",
        "log-level": "warning",
        "allow-lan": false,
        "ipv6": true,
        "unified-delay": true,
        "geo-auto-update": false,
        "external-controller": format!("127.0.0.1:{controller_port}"),
        "secret": secret,
        "profile": {"store-selected": false, "store-fake-ip": false},
        "proxy-providers": {"sub": {
            "type": "http",
            "url": subscription,
            "path": "./providers/sub.yaml",
            "interval": 21600,
            "proxy": "DIRECT",
        }},
        "listeners": [{
            "name": "workbuddy-egress",
            "type": "socks",
            "listen": "127.0.0.1",
            "port": socks_port,
            "udp": false,
            "users": names.iter().map(|u| json!({"username": u, "password": users[*u]})).collect::<Vec<_>>(),
        }],
        "proxy-groups": groups,
        "rules": rules,
    })
}

fn controller_client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    });
    &CLIENT
}
struct Api {
    controller: String,
    secret: String,
}
impl Api {
    fn url(&self, segments: &[&str]) -> Result<url::Url, String> {
        let mut url = url::Url::parse(&self.controller).map_err(|_| "Invalid mihomo controller")?;
        url.path_segments_mut()
            .map_err(|_| "Invalid mihomo controller")?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }
    async fn send(
        &self,
        method: reqwest::Method,
        url: url::Url,
        body: Option<Value>,
    ) -> Result<reqwest::Response, String> {
        let mut request = controller_client()
            .request(method, url)
            .bearer_auth(&self.secret);
        if let Some(body) = body {
            request = request.json(&body);
        }
        request
            .send()
            .await
            .map_err(|_| "mihomo controller is unreachable".into())
    }
    async fn get(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Value, String> {
        let mut url = self.url(segments)?;
        url.query_pairs_mut().extend_pairs(query);
        let response = self.send(reqwest::Method::GET, url, None).await?;
        let status = response.status();
        let value = response.json::<Value>().await.unwrap_or(Value::Null);
        if status.is_success() {
            Ok(value)
        } else {
            Err(value["message"]
                .as_str()
                .map(|m| format!("mihomo: {m}"))
                .unwrap_or_else(|| format!("mihomo returned HTTP {}", status.as_u16())))
        }
    }
    async fn put(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Value,
    ) -> Result<(), String> {
        let mut url = self.url(segments)?;
        url.query_pairs_mut().extend_pairs(query);
        let response = self.send(reqwest::Method::PUT, url, Some(body)).await?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status().as_u16();
            let value = response.json::<Value>().await.unwrap_or(Value::Null);
            Err(value["message"]
                .as_str()
                .map(|m| format!("mihomo: {m}"))
                .unwrap_or_else(|| format!("mihomo returned HTTP {status}")))
        }
    }
    async fn nodes(&self) -> Result<Vec<Node>, String> {
        let value = self.get(&["providers", "proxies", PROVIDER], &[]).await?;
        Ok(parse_nodes(&value))
    }
    async fn select(&self, user: &str, node: &str) -> Result<(), String> {
        self.put(&["proxies", &group(user)], &[], json!({"name": node}))
            .await
            .map_err(|_| "This node is not in the current subscription".to_string())?;
        let now = self.get(&["proxies", &group(user)], &[]).await?;
        if now["now"].as_str() == Some(node) {
            Ok(())
        } else {
            Err("mihomo did not switch to the selected node".into())
        }
    }
}
pub(super) fn parse_nodes(value: &Value) -> Vec<Node> {
    value["proxies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let name = p["name"].as_str()?.to_owned();
            let last = p["history"].as_array().and_then(|h| h.last());
            let delay = last.and_then(|h| h["delay"].as_u64());
            Some(Node {
                name,
                kind: p["type"].as_str().unwrap_or_default().to_owned(),
                delay: delay.filter(|d| *d > 0),
                alive: delay.map(|d| d > 0),
            })
        })
        .collect()
}

/// A loopback SOCKS route bound to one routing user's selector.
pub(super) struct Route {
    pub proxy_url: String,
}

fn alive(runtime: &mut Runtime) -> bool {
    matches!(runtime.child.try_wait(), Ok(None))
}
fn kill_stale(dir: &Path) {
    let pid_file = dir.join("mihomo.pid");
    let Some(pid) = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return;
    };
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All);
    if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
        // Only a mihomo left behind by a previous run of this app, never a reused pid.
        let ours = process
            .cmd()
            .iter()
            .any(|arg| Path::new(arg) == dir || arg.to_string_lossy() == dir.to_string_lossy());
        if process.name().to_string_lossy().contains("mihomo") && ours {
            process.kill();
        }
    }
    let _ = std::fs::remove_file(pid_file);
}

async fn wait_ready(api: &Api, child: &mut tokio::process::Child) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "mihomo exited during startup ({status}); see egress/mihomo.log"
            ));
        }
        if api.get(&["version"], &[]).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("mihomo did not start in time; see egress/mihomo.log".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
async fn wait_provider(api: &Api) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + PROVIDER_TIMEOUT;
    loop {
        if api.nodes().await.is_ok_and(|n| !n.is_empty()) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("The subscription returned no usable nodes; check the link".into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn write_config(dir: &Path, value: &Value) -> Result<PathBuf, String> {
    let path = dir.join("config.yaml");
    // JSON is valid YAML; serde_json keeps every node name and secret escaped.
    let data = serde_json::to_vec_pretty(value).map_err(|_| "Cannot build mihomo config")?;
    crate::utils::fs::write_atomic(&path, &data)?;
    Ok(path)
}

async fn start(
    state: &mut State,
    settings: &Settings,
    users: HashSet<String>,
) -> Result<(), String> {
    if let Some(at) = state.retry_at {
        if tokio::time::Instant::now() < at {
            return Err(state
                .last_error
                .clone()
                .unwrap_or_else(|| "mihomo is restarting".into()));
        }
    }
    let result = spawn(state, settings, users).await;
    match &result {
        Ok(()) => {
            state.last_error = None;
            state.retry_at = None;
        }
        Err(error) => {
            tracing::warn!("WorkBuddy egress: {error}");
            state.last_error = Some(error.clone());
            state.retry_at = Some(tokio::time::Instant::now() + RESTART_BACKOFF);
        }
    }
    result
}
async fn spawn(
    state: &mut State,
    settings: &Settings,
    users: HashSet<String>,
) -> Result<(), String> {
    state.runtime = None;
    let binary = resolve_binary(&settings.mihomo_path)?;
    state.binary = Some(binary.clone());
    let dir = dir()?;
    std::fs::create_dir_all(dir.join("providers")).map_err(|_| "Cannot create egress directory")?;
    kill_stale(&dir);
    let (controller_port, socks_port) = (free_port()?, free_port()?);
    let secret = random_secret();
    let users: HashMap<_, _> = users.into_iter().map(|u| (u, random_secret())).collect();
    let config_path = write_config(
        &dir,
        &config(
            &settings.subscription_url,
            controller_port,
            &secret,
            socks_port,
            &users,
        ),
    )?;
    let log =
        std::fs::File::create(dir.join("mihomo.log")).map_err(|_| "Cannot create mihomo log")?;
    let log_err = log.try_clone().map_err(|_| "Cannot create mihomo log")?;
    use crate::utils::command::CommandExtWrapper;
    let mut child = tokio::process::Command::new(&binary)
        .arg("-d")
        .arg(&dir)
        .arg("-f")
        .arg(&config_path)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .kill_on_drop(true)
        .creation_flags_windows()
        .spawn()
        .map_err(|_| "Cannot start mihomo")?;
    if let Some(pid) = child.id() {
        let _ = std::fs::write(dir.join("mihomo.pid"), pid.to_string());
    }
    let api = Api {
        controller: format!("http://127.0.0.1:{controller_port}"),
        secret: secret.clone(),
    };
    wait_ready(&api, &mut child).await?;
    let runtime = Runtime {
        child,
        controller: api.controller.clone(),
        secret,
        socks_port,
        users,
        fingerprint: fingerprint(settings),
        verified: HashSet::new(),
    };
    state.runtime = Some(runtime);
    wait_provider(&api).await
}

/// Starts, restarts or reconfigures mihomo so that `users` can route. Returns the API.
async fn ensure(state: &mut State, settings: &Settings, users: &[&str]) -> Result<Api, String> {
    if settings.subscription_url.is_empty() {
        return Err("Add an airport subscription link first".into());
    }
    let healthy = match state.runtime.as_mut() {
        Some(runtime) => alive(runtime) && runtime.fingerprint == fingerprint(settings),
        None => false,
    };
    if !healthy {
        let mut all = account_users();
        all.extend(users.iter().map(|u| u.to_string()));
        start(state, settings, all).await?;
    }
    let runtime = state.runtime.as_mut().ok_or("mihomo is not running")?;
    let api = Api {
        controller: runtime.controller.clone(),
        secret: runtime.secret.clone(),
    };
    if users.iter().any(|u| !runtime.users.contains_key(*u)) {
        // A new account joined: add its user and selector, keeping every other password.
        let mut wanted = account_users();
        wanted.extend(users.iter().map(|u| u.to_string()));
        for user in wanted {
            runtime.users.entry(user).or_insert_with(random_secret);
        }
        let dir = dir()?;
        let ctl_port = url::Url::parse(&runtime.controller)
            .ok()
            .and_then(|u| u.port())
            .ok_or("Invalid mihomo controller")?;
        let path = write_config(
            &dir,
            &config(
                &settings.subscription_url,
                ctl_port,
                &runtime.secret,
                runtime.socks_port,
                &runtime.users,
            ),
        )?;
        api.put(
            &["configs"],
            &[("force", "true")],
            json!({"path": path.to_string_lossy()}),
        )
        .await?;
        // Reloading rebuilds selectors; every selection must be applied again.
        runtime.verified.clear();
    }
    Ok(api)
}
fn account_users() -> HashSet<String> {
    let mut users: HashSet<String> = super::accounts::all()
        .map(|accounts| accounts.into_iter().map(|a| a.id).collect())
        .unwrap_or_default();
    users.insert(LOGIN_USER.into());
    users
}

/// Route `user` through `node`, confirming the selector before any traffic uses it.
pub(super) async fn route(settings: &Settings, user: &str, node: &str) -> Result<Route, String> {
    let mut state = STATE.lock().await;
    let api = ensure(&mut state, settings, &[user]).await?;
    let runtime = state.runtime.as_mut().ok_or("mihomo is not running")?;
    let key = (user.to_owned(), node.to_owned());
    if !runtime.verified.contains(&key) {
        api.select(user, node).await?;
        runtime.verified.retain(|(u, _)| u != user);
        runtime.verified.insert(key);
    }
    let password = runtime.users.get(user).ok_or("mihomo route is missing")?;
    Ok(Route {
        proxy_url: format!(
            "socks5h://{user}:{password}@127.0.0.1:{}",
            runtime.socks_port
        ),
    })
}
pub(super) async fn nodes(settings: &Settings) -> Result<Vec<Node>, String> {
    let mut state = STATE.lock().await;
    let api = ensure(&mut state, settings, &[]).await?;
    drop(state);
    api.nodes().await
}
/// Measures `url` through the user's own selector, so the result reflects the exact
/// path that account's traffic takes.
pub(super) async fn delay(
    settings: &Settings,
    user: &str,
    node: &str,
    url: &str,
) -> Result<u64, String> {
    route(settings, user, node).await?;
    let api = {
        let state = STATE.lock().await;
        let runtime = state.runtime.as_ref().ok_or("mihomo is not running")?;
        Api {
            controller: runtime.controller.clone(),
            secret: runtime.secret.clone(),
        }
    };
    let value = api
        .get(
            &["proxies", &group(user), "delay"],
            &[("url", url), ("timeout", "8000")],
        )
        .await
        .map_err(|_| "Node timed out or cannot reach WorkBuddy".to_string())?;
    value["delay"]
        .as_u64()
        .filter(|d| *d > 0)
        .ok_or_else(|| "Node timed out or cannot reach WorkBuddy".into())
}
pub(super) async fn test_all(settings: &Settings) -> Result<(), String> {
    let mut state = STATE.lock().await;
    let api = ensure(&mut state, settings, &[]).await?;
    drop(state);
    // Failed nodes are recorded in provider history as delay 0; the call itself may
    // report an error when every node fails, which is still a completed test.
    let _ = api
        .get(
            &["group", ALL_GROUP, "delay"],
            &[
                ("url", "https://www.gstatic.com/generate_204"),
                ("timeout", "5000"),
            ],
        )
        .await;
    Ok(())
}
pub(super) async fn refresh(settings: &Settings) -> Result<(), String> {
    let mut state = STATE.lock().await;
    let api = ensure(&mut state, settings, &[]).await?;
    api.put(&["providers", "proxies", PROVIDER], &[], json!({}))
        .await?;
    if let Some(runtime) = state.runtime.as_mut() {
        runtime.verified.clear();
    }
    Ok(())
}
/// Drops a confirmed selection so the next request reapplies the user's binding.
pub(super) async fn forget_selection(user: &str) {
    if let Some(runtime) = STATE.lock().await.runtime.as_mut() {
        runtime.verified.retain(|(u, _)| u != user);
    }
}
pub(super) async fn stop() {
    let mut state = STATE.lock().await;
    if let Some(mut runtime) = state.runtime.take() {
        let _ = runtime.child.kill().await;
    }
    state.last_error = None;
    state.retry_at = None;
}
/// Keeps mihomo alive while egress is configured; restarts it after a crash.
pub(super) async fn supervise(settings: &Settings) {
    let mut state = STATE.lock().await;
    let _ = ensure(&mut state, settings, &[]).await;
}
pub(super) struct Status {
    pub running: bool,
    pub binary: Option<String>,
    pub error: Option<String>,
}
pub(super) async fn status() -> Status {
    let mut state = STATE.lock().await;
    let running = state.runtime.as_mut().is_some_and(alive);
    Status {
        running,
        binary: state.binary.as_ref().map(|p| p.display().to_string()),
        error: state.last_error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_routes_each_user_to_its_selector_and_rejects_the_rest() {
        let users = HashMap::from([
            ("acct-b".to_string(), "pb".to_string()),
            (LOGIN_USER.to_string(), "pl".to_string()),
        ]);
        let value = config("https://sub.example/x?token=1", 9090, "s", 7891, &users);
        assert_eq!(value["external-controller"], "127.0.0.1:9090");
        assert_eq!(value["listeners"][0]["listen"], "127.0.0.1");
        assert_eq!(value["listeners"][0]["users"].as_array().unwrap().len(), 2);
        let rules: Vec<_> = value["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        assert_eq!(
            rules,
            [
                "IN-USER,acct-b,wb-acct-b",
                "IN-USER,login,wb-login",
                "MATCH,REJECT"
            ]
        );
        let groups: Vec<_> = value["proxy-groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["name"].as_str().unwrap())
            .collect();
        assert_eq!(groups, [ALL_GROUP, "wb-acct-b", "wb-login"]);
        assert_eq!(
            value["proxy-providers"][PROVIDER]["url"],
            "https://sub.example/x?token=1"
        );
    }

    #[test]
    fn provider_history_distinguishes_untested_failed_and_alive_nodes() {
        let nodes = parse_nodes(&json!({"proxies": [
            {"name": "a", "type": "Trojan", "history": []},
            {"name": "b", "type": "Vmess", "history": [{"delay": 0}]},
            {"name": "c", "type": "Shadowsocks", "history": [{"delay": 300}, {"delay": 120}]},
        ]}));
        assert_eq!((nodes[0].delay, nodes[0].alive), (None, None));
        assert_eq!((nodes[1].delay, nodes[1].alive), (None, Some(false)));
        assert_eq!((nodes[2].delay, nodes[2].alive), (Some(120), Some(true)));
    }
}
