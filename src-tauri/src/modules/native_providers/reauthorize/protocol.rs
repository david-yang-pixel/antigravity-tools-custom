//! Private protocol worker, encrypted resumable state and safe public progress.
use super::{now, Batch, LoginCredentials, Service, Target};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Semaphore, SemaphorePermit},
};

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Queued,
    Preparing,
    Connecting,
    Resuming,
    Login,
    Password,
    TwoFactor,
    SecurityCheck,
    Oauth,
    Workspace,
    TokenExchange,
    Verifying,
    Complete,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    pub checkpoint: Option<Value>,
    pub route: String,
    pub node: Option<String>,
    pub stage: Option<Stage>,
    pub started_at: i64,
    pub stage_started_at: i64,
    pub updated_at: i64,
    pub finished_at: i64,
    pub waiting: bool,
    pub retryable: bool,
}
#[derive(Clone, Serialize)]
pub struct View {
    pub stage: Stage,
    pub elapsed_seconds: i64,
    pub stage_seconds: i64,
    pub waiting: bool,
}
impl State {
    pub fn view(&self) -> Option<View> {
        let at = if self.finished_at > 0 {
            self.finished_at
        } else {
            now()
        };
        Some(View {
            stage: self.stage?,
            elapsed_seconds: if self.started_at > 0 {
                (at - self.started_at).max(0)
            } else {
                0
            },
            stage_seconds: if self.stage_started_at > 0 {
                (at - self.stage_started_at).max(0)
            } else {
                0
            },
            waiting: self.waiting,
        })
    }
    pub fn reset_progress(&mut self) {
        self.stage = None;
        self.started_at = 0;
        self.stage_started_at = 0;
        self.updated_at = 0;
        self.finished_at = 0;
        self.waiting = false;
        self.retryable = false;
    }
    pub fn complete(&mut self) {
        if self.stage.is_some() {
            self.stage = Some(Stage::Complete);
            self.stage_started_at = now();
            self.finished_at = now();
            self.updated_at = now();
        }
        self.waiting = false;
        self.retryable = false;
    }
}
pub fn enabled() -> bool {
    std::env::var("ABV_CHATGPT_AUTH_ENGINE").as_deref() != Ok("browser")
}
static WORKERS: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(crate::modules::egress::NATIVE_LOGIN_SLOTS));
static SLOTS: Mutex<[bool; crate::modules::egress::NATIVE_LOGIN_SLOTS]> =
    Mutex::new([false; crate::modules::egress::NATIVE_LOGIN_SLOTS]);
struct Slot {
    index: usize,
    _permit: SemaphorePermit<'static>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut slots) = SLOTS.lock() {
            slots[self.index] = false;
        }
    }
}
async fn reserve(target: &Target) -> Result<Slot, String> {
    let acquire = WORKERS.acquire();
    tokio::pin!(acquire);
    let permit = loop {
        tokio::select! {
            result=&mut acquire=>break result.map_err(|_|"Authorization workers unavailable")?,
            _=tokio::time::sleep(Duration::from_secs(1))=>{target.state()?;}
        }
    };
    let mut slots = SLOTS
        .lock()
        .map_err(|_| "Authorization workers unavailable")?;
    let index = slots
        .iter()
        .position(|used| !*used)
        .ok_or("Authorization workers unavailable")?;
    slots[index] = true;
    Ok(Slot {
        index,
        _permit: permit,
    })
}

const ASSETS: &[(&str, &str)] = &[
    ("worker.mjs", include_str!("protocol/worker.mjs")),
    (
        "vendor/tosub2/src/protocol-login.mjs",
        include_str!("protocol/vendor/tosub2/src/protocol-login.mjs"),
    ),
    (
        "vendor/tosub2/src/tls-transport.mjs",
        include_str!("protocol/vendor/tosub2/src/tls-transport.mjs"),
    ),
    (
        "vendor/tosub2/src/tls_transport.py",
        include_str!("protocol/vendor/tosub2/src/tls_transport.py"),
    ),
    (
        "vendor/tosub2/src/cloudflare-ctf/cf_runtime.cjs",
        include_str!("protocol/vendor/tosub2/src/cloudflare-ctf/cf_runtime.cjs"),
    ),
    (
        "vendor/tosub2/src/cloudflare-ctf/cloudflare_solver.py",
        include_str!("protocol/vendor/tosub2/src/cloudflare-ctf/cloudflare_solver.py"),
    ),
    (
        "vendor/tosub2/src/cloudflare-ctf/sentinel_client.py",
        include_str!("protocol/vendor/tosub2/src/cloudflare-ctf/sentinel_client.py"),
    ),
    (
        "vendor/tosub2/src/cloudflare-ctf/sentinel_dynamic.py",
        include_str!("protocol/vendor/tosub2/src/cloudflare-ctf/sentinel_dynamic.py"),
    ),
    (
        "vendor/tosub2/src/cloudflare-ctf/sentinel_runtime.cjs",
        include_str!("protocol/vendor/tosub2/src/cloudflare-ctf/sentinel_runtime.cjs"),
    ),
    (
        "vendor/tosub2/src/cloudflare-ctf/runtime_support.py",
        include_str!("protocol/vendor/tosub2/src/cloudflare-ctf/runtime_support.py"),
    ),
    (
        "vendor/tosub2/src/cloudflare-ctf/request_policy.py",
        include_str!("protocol/vendor/tosub2/src/cloudflare-ctf/request_policy.py"),
    ),
    (
        "vendor/tosub2/src/cloudflare-ctf/turnstile_runtime.cjs",
        include_str!("protocol/vendor/tosub2/src/cloudflare-ctf/turnstile_runtime.cjs"),
    ),
];
struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Result<Self, String> {
        let root =
            std::env::temp_dir().join(format!("abv-chatgpt-protocol-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&root)
            .map_err(|_| "Cannot create private protocol directory")?;
        let value = Self(root);
        for (name, source) in ASSETS {
            let path = value.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap())
                .and_then(|_| std::fs::write(path, source))
                .map_err(|_| "Cannot prepare protocol worker")?;
        }
        Ok(value)
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct ProcessTree {
    id: u32,
    armed: bool,
}
impl ProcessTree {
    fn kill(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.id as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            let _ = std::process::Command::new(PathBuf::from(root).join("System32/taskkill.exe"))
                .args(["/PID", &self.id.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.kill();
    }
}
fn dependencies() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ABV_CHATGPT_AUTH_NODE_MODULES") {
        let path = PathBuf::from(path);
        if path.join("jsdom/package.json").is_file() {
            return Ok(path);
        }
        return Err(error("DEPENDENCIES_MISSING").into());
    }
    let mut paths = vec![
        PathBuf::from("/app/chatgpt-auth/node_modules"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/modules/native_providers/reauthorize/protocol/node_modules"),
    ];
    if let Some(parent) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
    {
        paths.push(parent.join("chatgpt-auth/node_modules"));
        paths.push(parent.join("../Resources/chatgpt-auth/node_modules"));
    }
    if let Some(parent) = std::path::Path::new(file!()).parent() {
        paths.push(parent.join("protocol/node_modules"));
    }
    paths
        .into_iter()
        .find(|p| p.join("jsdom/package.json").is_file())
        .ok_or_else(|| error("DEPENDENCIES_MISSING").into())
}
fn error(code: &str) -> &'static str {
    match code{
    "DEPENDENCIES_MISSING"=>"Protocol authorization needs Node.js 22+, Python curl_cffi and jsdom; check the worker installation",
    "LOGIN_REJECTED"=>"Official login rejected the password; edit the saved login credentials",
    "TOTP_REJECTED"=>"Official login rejected the 2FA code; check the 2FA secret and server clock",
    "EMAIL_REQUIRED"=>"Official login requires an email verification code; authorization is waiting for verification",
    "PHONE_REQUIRED"=>"Official authorization requires phone verification; complete it before retrying",
    "PROFILE_REQUIRED"=>"This account needs profile setup before authorization can continue",
    "SECURITY_CHECK"=>"Official security verification could not complete on the selected route",
    "NETWORK_FAILED"=>"Protocol authorization connection failed on the selected route",
    "RATE_LIMITED"=>"Official login is rate limited; retry is delayed",
    "ACCOUNT_UNAVAILABLE"=>"Official login reports that this account is unavailable",
    "STEP_TIMEOUT"=>"The current protocol authorization step timed out",
    "TASK_TIMEOUT"=>"Protocol authorization exceeded its time limit",
    "IDENTITY_MISMATCH"=>"Authorization returned a different identity or invalid OAuth callback",
    "INVALID_CHECKPOINT"=>"The saved authorization checkpoint is invalid; start a new login",
    _=>"Protocol authorization could not complete; check the reported stage and account status",
}
}
pub fn retry_delay(state: &super::State) -> i64 {
    if state.protocol.retryable {
        30 * i64::from(state.attempts.max(1))
    } else {
        3600
    }
}

async fn next_record<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> Result<Option<Value>, String> {
    loop {
        let bytes = reader
            .fill_buf()
            .await
            .map_err(|_| "Cannot read protocol worker")?;
        if bytes.is_empty() {
            return if buffer.is_empty() {
                Ok(None)
            } else {
                Err("Incomplete protocol worker response".into())
            };
        }
        let end = bytes.iter().position(|b| *b == b'\n');
        let count = end.map_or(bytes.len(), |p| p + 1);
        if buffer.len() + count > 524288 {
            return Err("Protocol worker response exceeds size limit".into());
        }
        buffer.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if end.is_some() {
            let line = std::mem::take(buffer);
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|_| "Invalid protocol worker response".into());
        }
    }
}
async fn run(target: &Target, login: &LoginCredentials, proxy: &str) -> Result<Value, String> {
    let state = target.state()?;
    let dir = Workspace::new()?;
    let modules = dependencies()?;
    let timeout = std::env::var("ABV_CHATGPT_AUTH_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120)
        .clamp(30, 300);
    let mut command =
        Command::new(std::env::var("ABV_CHATGPT_NODE").unwrap_or_else(|_| "node".into()));
    command
        .arg(dir.0.join("worker.mjs"))
        .arg("--abv-protocol-worker")
        .env("NODE_PATH", modules)
        .env("NODE_ENV", "production")
        .env_remove("TOSUB2_TEST_SENTINEL_TOKEN")
        .env("TMPDIR", &dir.0)
        .env("TMP", &dir.0)
        .env("TEMP", &dir.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(python) = std::env::var_os("ABV_CHATGPT_AUTH_PYTHON") {
        command.env("TOSUB2_PYTHON", python);
    } else if std::path::Path::new("/app/chatgpt-auth/venv/bin/python").is_file() {
        command.env("TOSUB2_PYTHON", "/app/chatgpt-auth/venv/bin/python");
    }
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000200);
    let mut child = command.spawn().map_err(|_| error("DEPENDENCIES_MISSING"))?;
    let mut tree = ProcessTree {
        id: child.id().ok_or("Cannot start protocol worker")?,
        armed: true,
    };
    let input = json!({"email":login.email,"password":login.password,"two_factor":login.two_factor,"proxy":proxy,
        "checkpoint":state.protocol.checkpoint,"timeout_ms":timeout*1000});
    let bytes = serde_json::to_vec(&input).map_err(|_| "Cannot prepare protocol login")?;
    let mut stdin = child.stdin.take().ok_or("Cannot prepare protocol login")?;
    tokio::time::timeout(Duration::from_secs(10), stdin.write_all(&bytes))
        .await
        .map_err(|_| "Protocol worker input timed out")?
        .map_err(|_| "Cannot send protocol login")?;
    drop(stdin);
    let mut reader = BufReader::new(child.stdout.take().ok_or("Cannot read protocol worker")?);
    let mut buffer = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout + 15);
    let result = async {
        loop {
            let record = tokio::select! {
                _=tokio::time::sleep_until(deadline)=>return Err(error("TASK_TIMEOUT").into()),
                _=tokio::time::sleep(Duration::from_secs(1))=>{target.state()?;continue;},
                record=next_record(&mut reader,&mut buffer)=>record?,
            };
            let record = record.ok_or("Protocol worker exited without a result")?;
            match record["type"].as_str() {
                Some("progress") => {
                    let stage: Stage = serde_json::from_value(record["stage"].clone())
                        .map_err(|_| "Invalid authorization stage")?;
                    target.change(|state| {
                        let s = &mut state.protocol;
                        if s.stage != Some(stage) {
                            s.stage_started_at = now();
                        }
                        s.stage = Some(stage);
                        s.updated_at = now();
                    })?;
                }
                Some("checkpoint") => {
                    let checkpoint = record.get("checkpoint").filter(|v| !v.is_null()).cloned();
                    if checkpoint.as_ref().is_some_and(|cp| {
                        cp["email"].as_str() != Some(login.email.as_str())
                            || cp["stage"] != "email_verified"
                    }) {
                        return Err("Invalid authorization checkpoint identity".into());
                    }
                    target.change(|s| s.protocol.checkpoint = checkpoint)?;
                }
                Some("error") => {
                    let code = record["code"].as_str().unwrap_or("PROTOCOL_FAILED");
                    target.change(|s| {
                        s.protocol.waiting = matches!(
                            code,
                            "EMAIL_REQUIRED"
                                | "PHONE_REQUIRED"
                                | "PROFILE_REQUIRED"
                                | "SECURITY_CHECK"
                        );
                        s.protocol.retryable = matches!(code, "NETWORK_FAILED" | "RATE_LIMITED")
                            && s.protocol.stage != Some(Stage::TokenExchange);
                        s.protocol.finished_at = now();
                        s.protocol.updated_at = now();
                    })?;
                    return Err(error(code).into());
                }
                Some("result") => {
                    let account = record
                        .get("account")
                        .filter(|v| v.is_object())
                        .cloned()
                        .ok_or("Invalid protocol authorization result")?;
                    target.change(|s| {
                        s.protocol.stage = Some(Stage::Verifying);
                        s.protocol.stage_started_at = now();
                        s.protocol.updated_at = now();
                    })?;
                    return Ok(account);
                }
                _ => return Err("Invalid protocol worker response".into()),
            }
        }
    }
    .await;
    tree.kill();
    let _ = child.kill().await;
    if result.is_err() {
        let _ = target.change(|s| {
            if s.protocol.finished_at == 0 {
                s.protocol.finished_at = now();
                s.protocol.updated_at = now();
            }
        });
    }
    result
}
pub async fn create(
    login: &LoginCredentials,
    proxy: &str,
    target: &Target,
) -> Result<Batch, String> {
    target.change(|s| {
        s.protocol.reset_progress();
        s.protocol.stage = Some(Stage::Queued);
        s.protocol.started_at = now();
        s.protocol.stage_started_at = now();
        s.protocol.updated_at = now();
    })?;
    let slot = reserve(target).await?;
    let saved = target.state()?.protocol;
    let mut route = if target.pending {
        match crate::modules::egress::native_login_route(
            target.provider,
            slot.index,
            target.node.as_deref().or(saved.node.as_deref()),
        )
        .await
        {
            Ok(route) => Some(route),
            Err(_) if target.node.is_none() && saved.node.is_some() => {
                target.change(|s| {
                    s.protocol.checkpoint = None;
                    s.protocol.node = None;
                })?;
                Some(
                    crate::modules::egress::native_login_route(target.provider, slot.index, None)
                        .await?,
                )
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let proxy = route
        .as_ref()
        .map(|r| super::super::http::proxy_url(&r.exit))
        .unwrap_or_else(|| proxy.into());
    let node = route
        .as_ref()
        .and_then(|r| r.node.clone())
        .or_else(|| target.node.clone());
    let fingerprint = node
        .as_ref()
        .map(|n| format!("node:{n}"))
        .unwrap_or_else(|| format!("proxy:{:x}", Sha256::digest(proxy.as_bytes())));
    target.change(|s| {
        let p = &mut s.protocol;
        if p.route != fingerprint {
            p.checkpoint = None;
        }
        p.route = fingerprint;
        p.node = node.clone();
        p.stage = Some(Stage::Preparing);
        p.stage_started_at = now();
        p.updated_at = now();
        p.finished_at = 0;
        p.waiting = false;
        p.retryable = false;
    })?;
    let account = run(target, login, &proxy).await?;
    let lease = route
        .as_mut()
        .and_then(|route| route.take_hold())
        .map(Arc::new);
    Ok(Batch {
        service: Service::BuiltIn,
        id: uuid::Uuid::new_v4().to_string(),
        token: account.to_string(),
        base: String::new(),
        created_at: now(),
        node,
        lease,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::native_providers::{login_imports, tests::Sandbox, Provider};
    fn pending() -> Target {
        let report = login_imports::enqueue(
            Provider::ChatGpt,
            "worker@example.com----private-password----JBSWY3DPEHPK3PXP",
            true,
            Some(super::super::ServiceConfig {
                service: Service::BuiltIn,
                cdk: String::new(),
            }),
        )
        .unwrap();
        let id = report.queued[0].id.clone();
        let job = super::super::super::store::imports(Provider::ChatGpt)
            .unwrap()
            .into_iter()
            .find(|j| j.id == id)
            .unwrap();
        Target {
            provider: Provider::ChatGpt,
            id,
            revision: job.revision,
            pending: true,
            node: None,
        }
    }
    #[tokio::test]
    async fn fragmented_record_survives_cancellation_between_reads() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(reader);
        let mut buffer = Vec::new();
        writer.write_all(b"{\"type\":\"progress\",").await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            next_record(&mut reader, &mut buffer)
        )
        .await
        .is_err());
        writer.write_all(b"\"stage\":\"login\"}\n").await.unwrap();
        assert_eq!(
            next_record(&mut reader, &mut buffer)
                .await
                .unwrap()
                .unwrap()["stage"],
            "login"
        );
        assert!(buffer.is_empty());
    }
    #[test]
    fn checkpoints_are_encrypted_and_removed_targets_reject_late_writes() {
        let _sandbox = Sandbox::new();
        let target = pending();
        target
            .change(|s| {
                s.protocol.checkpoint =
                    Some(json!({"email":"worker@example.com","cookies":["private-cookie"]}));
                s.protocol.stage = Some(Stage::Password);
                s.protocol.started_at = now();
                s.protocol.stage_started_at = now();
            })
            .unwrap();
        let public = serde_json::to_string(&login_imports::list(target.provider).unwrap()).unwrap();
        assert!(public.contains("password"));
        assert!(!public.contains("private-cookie"));
        assert!(!public.contains("checkpoint"));
        let vault = std::fs::read_to_string(
            crate::modules::account::get_data_dir()
                .unwrap()
                .join("chatgpt_login_imports.enc"),
        )
        .unwrap();
        assert!(!vault.contains("private-cookie"));
        assert!(!vault.contains("private-password"));
        login_imports::remove(target.provider, &target.id).unwrap();
        assert!(target.state().is_err());
        assert!(target.change(|s| s.protocol.complete()).is_err());
    }
    #[tokio::test]
    async fn two_slots_are_independent_and_a_removed_queued_task_cancels() {
        let _sandbox = Sandbox::new();
        let target = pending();
        let one = reserve(&target).await.unwrap();
        let two = reserve(&target).await.unwrap();
        assert_ne!(one.index, two.index);
        let waiting = reserve(&target);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        login_imports::remove(target.provider, &target.id).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), &mut waiting)
            .await
            .unwrap()
            .is_err());
        drop(one);
        drop(two);
    }
    #[test]
    fn private_worker_assets_are_removed_on_drop() {
        let workspace = Workspace::new().unwrap();
        let path = workspace.0.clone();
        assert!(path.join("vendor/tosub2/src/tls_transport.py").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        drop(workspace);
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn missing_python_is_reported_without_credentials_and_cleans_the_worker() {
        let _sandbox = Sandbox::new();
        if Command::new("node")
            .arg("--version")
            .output()
            .await
            .is_err()
            || dependencies().is_err()
        {
            return;
        }
        std::env::set_var("ABV_CHATGPT_AUTH_PYTHON", "/missing-abv-python");
        let target = pending();
        let login =
            super::super::parse("worker@example.com----private-password----JBSWY3DPEHPK3PXP")
                .unwrap()
                .remove(0);
        let before: std::collections::HashSet<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        let error = run(&target, &login, "").await.unwrap_err();
        assert_eq!(error, super::error("DEPENDENCIES_MISSING"));
        assert!(!error.contains("private-password"));
        for entry in std::fs::read_dir(std::env::temp_dir()).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("abv-chatgpt-protocol-")
            {
                assert!(before.contains(&entry.file_name()));
            }
        }
    }
}
