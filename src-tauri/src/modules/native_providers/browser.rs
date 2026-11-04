//! Temporary server-owned official login browser. The public API exposes only
//! screenshots and bounded input actions; CDP and grants stay on private pipes.
use super::*;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
};

type Reply = Result<Value, String>;
struct Browser {
    commands: mpsc::Sender<Value>,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
}
static BROWSERS: LazyLock<Mutex<HashMap<String, Arc<Browser>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SLOTS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));

#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub image: String,
    pub width: u32,
    pub height: u32,
    pub location: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    Click { x: f64, y: f64 },
    Text { text: String },
    Key { key: String },
    Scroll { delta_y: f64 },
    Reload,
}
impl Input {
    pub fn validate(&self) -> Result<(), String> {
        let valid = match self {
            Self::Click { x, y } => {
                x.is_finite()
                    && y.is_finite()
                    && (0.0..=1000.0).contains(x)
                    && (0.0..=720.0).contains(y)
            }
            Self::Text { text } => !text.is_empty() && text.len() <= 2048,
            Self::Key { key } => [
                "Enter",
                "Tab",
                "ShiftTab",
                "Backspace",
                "Delete",
                "Escape",
                "ArrowLeft",
                "ArrowRight",
                "ArrowUp",
                "ArrowDown",
                "Home",
                "End",
                "SelectAll",
            ]
            .contains(&key.as_str()),
            Self::Scroll { delta_y } => delta_y.is_finite() && delta_y.abs() <= 1200.0,
            Self::Reload => true,
        };
        if valid {
            Ok(())
        } else {
            Err("Invalid login browser input".into())
        }
    }
}

fn browser(id: &str) -> Result<Arc<Browser>, String> {
    BROWSERS
        .lock()
        .map_err(|_| "TRAE_BROWSER_UNAVAILABLE")?
        .get(id)
        .cloned()
        .ok_or_else(|| "TRAE_BROWSER_STOPPED".into())
}
pub fn running(id: &str) -> bool {
    browser(id).is_ok()
}

async fn rpc(id: &str, mut command: Value) -> Reply {
    let browser = browser(id)?;
    let request = uuid::Uuid::new_v4().to_string();
    command["id"] = json!(request);
    let (tx, rx) = oneshot::channel();
    {
        let mut pending = browser
            .pending
            .lock()
            .map_err(|_| "TRAE_BROWSER_UNAVAILABLE")?;
        if pending.len() >= 16 {
            return Err("TRAE_BROWSER_BUSY".into());
        }
        pending.insert(request.clone(), tx);
    }
    let result = if browser.commands.try_send(command).is_err() {
        Err("TRAE_BROWSER_BUSY".into())
    } else {
        match tokio::time::timeout(Duration::from_secs(25), rx).await {
            Ok(Ok(reply)) => reply,
            _ => Err("TRAE_BROWSER_UNAVAILABLE".into()),
        }
    };
    if let Ok(mut pending) = browser.pending.lock() {
        pending.remove(&request);
    }
    result
}

pub fn close(id: &str) {
    if let Ok(browser) = browser(id) {
        let _ = browser.commands.try_send(json!({"op":"close"}));
    }
}

fn port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|_| "TRAE_BROWSER_START_FAILED")?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|_| "TRAE_BROWSER_START_FAILED".into())
}

struct PrivateDirectory(PathBuf);
impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn start(
    id: &str,
    auth_url: &str,
    callback_url: &str,
    trace: &str,
    expires_at: i64,
) -> Result<(), String> {
    // Reserve before any await or process spawn, including concurrent starts.
    let slot = SLOTS
        .clone()
        .try_acquire_owned()
        .map_err(|_| "TRAE_BROWSER_BUSY")?;
    let proxy = std::env::var("ABV_TRAE_BROWSER_PROXY")
        .ok()
        .filter(|v| !v.is_empty());
    if let Some(proxy) = &proxy {
        let parsed = url::Url::parse(proxy).map_err(|_| "Invalid login browser proxy")?;
        if !["http", "https", "socks5"].contains(&parsed.scheme())
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.path() != "/" && !parsed.path().is_empty()
        {
            return Err("Login browser proxy requires an HTTP(S) or SOCKS5 URL without credentials or a path".into());
        }
    }
    // Give a precise error for the known regional refusal before opening a blank viewer.
    let mut page =
        http::official_url(Provider::Trae, auth_url).map_err(|_| "Invalid TRAE login page")?;
    page.set_query(None);
    if proxy.is_none() {
        if let Ok(client) = http::client() {
            if let Ok(mut response) = client
                .get(page)
                .timeout(Duration::from_secs(8))
                .send()
                .await
            {
                if response.status() == reqwest::StatusCode::FORBIDDEN {
                    let mut body = Vec::new();
                    while body.len() < 32 * 1024 {
                        match response.chunk().await {
                            Ok(Some(chunk)) => body.extend_from_slice(
                                &chunk[..chunk.len().min(32 * 1024 - body.len())],
                            ),
                            _ => break,
                        }
                    }
                    if String::from_utf8_lossy(&body).contains("当前区域不支持") {
                        return Err("TRAE_BROWSER_REGION_BLOCKED".into());
                    }
                }
            }
        }
    }
    {
        let browsers = BROWSERS.lock().map_err(|_| "TRAE_BROWSER_UNAVAILABLE")?;
        if browsers.len() >= 2 {
            return Err("TRAE_BROWSER_BUSY".into());
        }
    }
    let directory = PrivateDirectory(
        crate::modules::account::get_data_dir()?
            .join("native-login-browser")
            .join(id),
    );
    std::fs::create_dir_all(&directory.0).map_err(|_| "TRAE_BROWSER_START_FAILED")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "TRAE_BROWSER_START_FAILED")?;
    }
    let script = directory.0.join("runner.mjs");
    crate::utils::fs::write_atomic(&script, include_bytes!("browser_runner.mjs"))?;
    let debug_port = port()?;
    let candidates = std::env::var("ABV_TRAE_BROWSER_PATH")
        .ok()
        .filter(|p| !p.is_empty())
        .map(|p| vec![p])
        .unwrap_or_else(|| {
            vec![
                "chromium".into(),
                "chromium-browser".into(),
                "google-chrome".into(),
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
                "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe".into(),
                "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe".into(),
            ]
        });
    let mut chrome = None;
    for executable in candidates {
        let mut command = Command::new(executable);
        command
            .args([
                "--headless",
                "--disable-dev-shm-usage",
                "--disable-gpu",
                "--no-first-run",
                "--no-default-browser-check",
                "--window-size=1000,720",
                "--remote-debugging-address=127.0.0.1",
            ])
            .arg(format!("--remote-debugging-port={debug_port}"))
            .arg(format!(
                "--user-data-dir={}",
                directory.0.join("profile").display()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if std::env::var("ABV_TRAE_BROWSER_NO_SANDBOX").as_deref() == Ok("true") {
            command.arg("--no-sandbox");
        }
        if let Some(proxy) = &proxy {
            command
                .arg(format!("--proxy-server={proxy}"))
                .arg("--proxy-bypass-list=127.0.0.1;localhost");
        }
        command.arg("about:blank");
        if let Ok(child) = command.spawn() {
            chrome = Some(child);
            break;
        }
    }
    let mut chrome = chrome.ok_or("TRAE_BROWSER_START_FAILED")?;
    let node = std::env::var("ABV_TRAE_BROWSER_NODE").unwrap_or_else(|_| "node".into());
    let child = Command::new(node)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(_) => {
            let _ = chrome.kill().await;
            return Err("TRAE_BROWSER_START_FAILED".into());
        }
    };
    let (tx, rx) = mpsc::channel(32);
    let browser = Arc::new(Browser {
        commands: tx,
        pending: Mutex::new(HashMap::new()),
    });
    {
        let mut browsers = BROWSERS.lock().map_err(|_| "TRAE_BROWSER_UNAVAILABLE")?;
        if browsers.len() >= 2 || browsers.contains_key(id) {
            return Err("TRAE_BROWSER_BUSY".into());
        }
        browsers.insert(id.into(), browser.clone());
    }
    let owned_id = id.to_owned();
    tokio::spawn(actor(
        owned_id, browser, rx, child, chrome, directory, expires_at, slot,
    ));
    let result = rpc(id, json!({"op":"start","debug_port":debug_port,"auth_url":auth_url,"callback_url":callback_url,"trace":trace,"expires_at":expires_at})).await;
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            close(id);
            Err(error)
        }
    }
}

async fn actor(
    id: String,
    browser: Arc<Browser>,
    mut commands: mpsc::Receiver<Value>,
    mut node: Child,
    mut chrome: Child,
    _directory: PrivateDirectory,
    expires_at: i64,
    _slot: tokio::sync::OwnedSemaphorePermit,
) {
    let mut input = node.stdin.take().expect("piped browser stdin");
    let mut output = BufReader::new(node.stdout.take().expect("piped browser stdout")).lines();
    let expiry = tokio::time::sleep(Duration::from_secs(
        expires_at.saturating_sub(now()).max(0) as u64
    ));
    tokio::pin!(expiry);
    loop {
        tokio::select! {
            _ = &mut expiry => break,
            _ = node.wait() => break,
            _ = chrome.wait() => break,
            command = commands.recv() => {
                let Some(command) = command else { break; };
                let closing = command["op"] == "close";
                let mut bytes = command.to_string().into_bytes(); bytes.push(b'\n');
                if !tokio::time::timeout(Duration::from_secs(3), input.write_all(&bytes)).await.is_ok_and(|r| r.is_ok()) { break; }
                if closing { break; }
            }
            line = output.next_line() => {
                let Ok(Some(line)) = line else { break; };
                if line.len() > 4 * 1024 * 1024 { break; }
                let Ok(message) = serde_json::from_str::<Value>(&line) else { break; };
                if let Some(reply) = message["reply"].as_str() {
                    let tx = browser.pending.lock().ok().and_then(|mut p| p.remove(reply));
                    if let Some(tx) = tx {
                        let result = if let Some(error) = message["error"].as_str() {
                            Err(match error { "TRAE_BROWSER_REGION_BLOCKED" | "TRAE_BROWSER_PAGE_UNAVAILABLE" | "TRAE_BROWSER_START_FAILED" => error.to_owned(), _ => "TRAE_BROWSER_COMMAND_FAILED".into() })
                        } else { Ok(message["result"].clone()) };
                        let _ = tx.send(result);
                    }
                } else if message["event"] == "grant" {
                    if let (Some(request), Some(query)) = (message["request"].as_str(), message["query"].as_str()) {
                        if query.len() > 64 * 1024 { break; }
                        let (session, request, query, tx) = (id.clone(), request.to_owned(), query.to_owned(), browser.commands.clone());
                        tokio::spawn(async move {
                            let success = login::receive_callback(&session, &query).await.is_ok();
                            let _ = tx.send(json!({"op":"grant_result","request":request,"success":success})).await;
                        });
                    }
                } else if message["event"] == "done" { break; }
            }
        }
    }
    if let Ok(mut browsers) = BROWSERS.lock() {
        browsers.remove(&id);
    }
    if let Ok(mut pending) = browser.pending.lock() {
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err("TRAE_BROWSER_STOPPED".into()));
        }
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        input.write_all(b"{\"op\":\"close\"}\n"),
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(2), node.wait()).await;
    let _ = node.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), chrome.wait()).await;
    let _ = chrome.kill().await;
}

pub async fn snapshot(id: &str) -> Result<Snapshot, String> {
    login::browser_session(id)?;
    let snapshot: Snapshot = serde_json::from_value(rpc(id, json!({"op":"snapshot"})).await?)
        .map_err(|_| "Invalid login browser image")?;
    if !snapshot.image.starts_with("data:image/jpeg;base64,")
        || snapshot.image.len() > 2 * 1024 * 1024
        || snapshot.width != 1000
        || snapshot.height != 720
        || snapshot.location.len() > 240
        || snapshot.location.contains('?')
    {
        return Err("Invalid login browser image".into());
    }
    Ok(snapshot)
}

pub async fn input(id: &str, action: Input) -> Result<(), String> {
    login::browser_session(id)?;
    action.validate()?;
    rpc(id, json!({"op":"input","action":action}))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_input_rejects_unbounded_or_arbitrary_commands() {
        assert!(Input::Click {
            x: 1000.0,
            y: 720.0
        }
        .validate()
        .is_ok());
        assert!(Input::Text {
            text: "中文".into()
        }
        .validate()
        .is_ok());
        assert!(Input::Key {
            key: "SelectAll".into()
        }
        .validate()
        .is_ok());
        assert!(Input::Click {
            x: f64::NAN,
            y: 0.0
        }
        .validate()
        .is_err());
        assert!(Input::Click { x: -1.0, y: 0.0 }.validate().is_err());
        assert!(Input::Text {
            text: "中".repeat(700)
        }
        .validate()
        .is_err());
        assert!(Input::Key { key: "F12".into() }.validate().is_err());
        assert!(Input::Scroll { delta_y: 1201.0 }.validate().is_err());
        assert!(
            serde_json::from_value::<Input>(json!({"type":"evaluate","expression":"1"})).is_err()
        );
    }

    #[tokio::test]
    async fn browser_rpc_matches_replies_and_close_is_nonblocking() {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, mut rx) = mpsc::channel(4);
        let browser = Arc::new(Browser {
            commands: tx,
            pending: Mutex::new(HashMap::new()),
        });
        BROWSERS.lock().unwrap().insert(id.clone(), browser.clone());
        let driver = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            let reply = browser
                .pending
                .lock()
                .unwrap()
                .remove(request["id"].as_str().unwrap())
                .unwrap();
            reply.send(Ok(json!({"ready":true}))).unwrap();
            assert_eq!(rx.recv().await.unwrap()["op"], "close");
        });
        assert_eq!(
            rpc(&id, json!({"op":"snapshot"})).await.unwrap()["ready"],
            true
        );
        close(&id);
        driver.await.unwrap();
        BROWSERS.lock().unwrap().remove(&id);
        assert!(!running(&id));
        assert!(rpc(&id, json!({"op":"snapshot"})).await.is_err());
    }
}
