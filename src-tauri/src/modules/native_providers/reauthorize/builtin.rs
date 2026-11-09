//! Official OAuth with PKCE, driven by the project's own short-lived browser worker.
use super::{now, Batch, LoginCredentials, Progress, Service};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Stdio, sync::LazyLock, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
};

static BROWSER: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(1));
const REDIRECT: &str = "http://localhost:1455/auth/callback";
const SCRIPT: &str = include_str!("browser.mjs");
const PROXY_SCRIPT: &str = include_str!("browser-proxy.mjs");

fn nonce() -> String {
    let mut bytes = [0; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn authorization_url(email: &str, verifier: &str, state: &str) -> Result<url::Url, String> {
    let mut url = super::super::chatgpt::endpoints::url(
        &super::super::chatgpt::endpoints::current().auth,
        "/oauth/authorize",
    )
    .map_err(|_| "Invalid official authorization endpoint")?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", super::super::chatgpt::auth::CLIENT_ID),
        ("redirect_uri", REDIRECT),
        ("scope", "openid profile email offline_access"),
        (
            "code_challenge",
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)).as_str(),
        ),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
        ("login_hint", email),
    ]);
    Ok(url)
}
fn callback_code(callback: &str, state: &str) -> Result<String, String> {
    let url = url::Url::parse(callback).map_err(|_| "Invalid OAuth callback")?;
    if url.origin().ascii_serialization() != "http://localhost:1455"
        || url.path() != "/auth/callback"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("Invalid OAuth callback origin".into());
    }
    let params: Vec<_> = url.query_pairs().collect();
    let values = |key: &str| {
        params
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_ref())
            .collect::<Vec<_>>()
    };
    if values("state") != [state] || !values("error").is_empty() {
        return Err("OAuth state mismatch or authorization was rejected".into());
    }
    let codes = values("code");
    if codes.len() != 1 || codes[0].is_empty() || codes[0].len() > 8192 {
        return Err("OAuth callback has no valid authorization code".into());
    }
    Ok(codes[0].into())
}
fn browser() -> String {
    if let Ok(path) = std::env::var("ABV_CHATGPT_BROWSER") {
        return path;
    }
    let paths = [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    if let Some(path) = paths
        .iter()
        .find(|path| std::path::Path::new(path).is_file())
    {
        return (*path).into();
    }
    for directory in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(directory) {
            for suffix in [
                "Google/Chrome/Application/chrome.exe",
                "Microsoft/Edge/Application/msedge.exe",
            ] {
                let path = PathBuf::from(&root).join(suffix);
                if path.is_file() {
                    return path.to_string_lossy().into_owned();
                }
            }
        }
    }
    "chromium".into()
}
fn worker_error(code: &str) -> &'static str {
    match code {
        "BROWSER_UNAVAILABLE" => "Built-in authorization needs Chrome/Chromium; configure ABV_CHATGPT_BROWSER",
        "CHALLENGE" => "Official login requires human verification; use a merchant service or manually import authorized JSON",
        "EMAIL_VERIFICATION" => "Official login requires an email verification code; use a merchant service or manually import authorized JSON",
        "INVALID_CREDENTIALS" => "Official login rejected the password; edit the account's login credentials",
        "ACCOUNT_UNAVAILABLE" => "Official login reports this account is unavailable",
        "INVALID_2FA" => "Built-in authorization needs a valid Base32 2FA secret",
        "UNSUPPORTED_PROXY" => "Built-in authorization needs a valid HTTP, HTTPS or SOCKS5 proxy URL",
        "PROXY_AUTH_FAILED" => "The selected proxy rejected authentication; check the egress proxy credentials",
        "PROXY_CONNECTION_FAILED" => "Cannot reach official login through the selected proxy; check the egress node",
        "LOGIN_TIMEOUT" => "Official login timed out; check the egress node or use another authorization method",
        _ => "Built-in login could not complete; use a merchant service or manually import authorized JSON",
    }
}
struct Profile(PathBuf);
impl Profile {
    fn create() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("abv-chatgpt-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|_| "Cannot create private OAuth browser directory")?;
        Ok(Self(path))
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn run_worker(mut input: Value) -> Result<Value, String> {
    let profile = Profile::create()?;
    input["profile"] = json!(profile.0);
    // Source files contain no secrets. Keeping them in the private temporary directory
    // also avoids platform command-line length limits; credentials still use stdin.
    std::fs::write(profile.0.join("browser.mjs"), SCRIPT)
        .and_then(|_| std::fs::write(profile.0.join("browser-proxy.mjs"), PROXY_SCRIPT))
        .map_err(|_| "Cannot prepare built-in login worker")?;
    let node = std::env::var("ABV_CHATGPT_NODE").unwrap_or_else(|_| "node".into());
    let mut command = Command::new(node);
    command
        .arg(profile.0.join("browser.mjs"))
        .arg("--abv-login-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let mut child = command
        .spawn()
        .map_err(|_| "Built-in authorization needs Node.js 22+; configure ABV_CHATGPT_NODE")?;
    let payload = serde_json::to_vec(&input).map_err(|_| "Cannot prepare built-in login")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Cannot initialize built-in login")?;
    tokio::time::timeout(Duration::from_secs(10), stdin.write_all(&payload))
        .await
        .map_err(|_| "Cannot initialize built-in login; worker input timed out")?
        .map_err(|_| "Cannot initialize built-in login; Node.js 22+ is required")?;
    drop(stdin);
    let stdout = child
        .stdout
        .take()
        .ok_or("Cannot initialize built-in login")?;
    let result = tokio::time::timeout(Duration::from_secs(190), async {
        let mut bytes = Vec::new();
        stdout
            .take(65537)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "Cannot read built-in login result")?;
        let status = child
            .wait()
            .await
            .map_err(|_| "Cannot wait for built-in login")?;
        if !status.success() || bytes.len() > 65536 {
            return Err("Built-in login worker failed".into());
        }
        serde_json::from_slice::<Value>(&bytes).map_err(|_| "Invalid built-in login result".into())
    })
    .await;
    match result {
        Ok(value) => value,
        Err(_) => {
            // The worker has its own shorter deadline and cleans its profile normally.
            // Kill its process group too if a browser/OS fault prevents normal cleanup.
            #[cfg(unix)]
            if let Some(id) = child.id() {
                unsafe {
                    libc::kill(-(id as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            Err("Built-in authorization timed out".into())
        }
    }
}
async fn exchange(client: &reqwest::Client, code: &str, verifier: &str) -> Result<Value, String> {
    let endpoint = super::super::chatgpt::endpoints::url(
        &super::super::chatgpt::endpoints::current().auth,
        "/oauth/token",
    )
    .map_err(|_| "Invalid official token endpoint")?;
    let response = client
        .post(endpoint)
        .timeout(Duration::from_secs(60))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("client_id", super::super::chatgpt::auth::CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|_| "Cannot exchange the official authorization code")?;
    let tokens = super::json_response(response).await?;
    if super::super::text(&tokens, &["access_token"]).is_empty()
        || super::super::text(&tokens, &["refresh_token"]).is_empty()
    {
        return Err("Official authorization did not return complete tokens".into());
    }
    Ok(json!({"platform":"openai", "credentials": tokens}))
}
pub(super) async fn create(
    client: &reqwest::Client,
    login: &LoginCredentials,
    proxy: &str,
    target: &super::Target,
) -> Result<Batch, String> {
    if super::protocol::enabled() {
        return super::protocol::create(login, proxy, target).await;
    }
    // Small servers run one browser at a time, irrespective of merchant job concurrency.
    let _permit = BROWSER
        .acquire()
        .await
        .map_err(|_| "Built-in authorization unavailable")?;
    let verifier = nonce();
    let state = nonce();
    let url = authorization_url(&login.email, &verifier, &state)?;
    let result = run_worker(
        json!({"auth_url":url.as_str(), "browser":browser(), "proxy":proxy,
        "email":login.email, "password":login.password, "two_factor":login.two_factor}),
    )
    .await?;
    if let Some(error) = result["error"].as_str() {
        return Err(worker_error(error).into());
    }
    let code = callback_code(
        result["callback"]
            .as_str()
            .ok_or("No official OAuth callback received")?,
        &state,
    )?;
    let account = exchange(client, &code, &verifier).await?;
    Ok(Batch {
        service: Service::BuiltIn,
        id: uuid::Uuid::new_v4().to_string(),
        token: account.to_string(),
        base: String::new(),
        created_at: now(),
        node: None,
        lease: None,
    })
}
pub(super) fn poll(batch: &Batch) -> Result<Progress, String> {
    let account: Value =
        serde_json::from_str(&batch.token).map_err(|_| "Invalid built-in authorization result")?;
    if account["platform"] != "openai" {
        return Err("Invalid built-in authorization result".into());
    }
    Ok(Progress::Ready(account))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn embedded_worker_uses_stdin_and_cleans_its_profile_when_browser_is_missing() {
        let _sandbox = super::super::super::tests::Sandbox::new();
        if Command::new("node")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let before: std::collections::HashSet<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().starts_with("abv-chatgpt-"))
            .collect();
        let value = run_worker(json!({"auth_url":"https://auth.openai.com/oauth/authorize", "browser":"/missing-abv-test-browser",
            "proxy":"", "email":"a@example.com", "password":"private-password", "two_factor":"JBSWY3DPEHPK3PXP"})).await.unwrap();
        assert_eq!(value["error"], "BROWSER_UNAVAILABLE");
        assert!(!value.to_string().contains("private-password"));
        for entry in std::fs::read_dir(std::env::temp_dir()).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("abv-chatgpt-")
            {
                assert!(before.contains(&entry.file_name()));
            }
        }
    }
    #[test]
    fn pkce_and_callback_are_bound_to_the_original_session() {
        let verifier = nonce();
        let state = nonce();
        let url = authorization_url("a@example.com", &verifier, &state).unwrap();
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier))
        );
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["redirect_uri"], REDIRECT);
        let callback = format!("{REDIRECT}?state={state}&code=private-code");
        assert_eq!(callback_code(&callback, &state).unwrap(), "private-code");
        for invalid in [
            callback.replace("localhost", "evil.example"),
            callback.replace(&state, "wrong"),
            format!("{callback}&state={state}"),
            format!("{callback}&code=other"),
            format!("{callback}&error=denied"),
        ] {
            assert!(callback_code(&invalid, &state).is_err());
        }
    }
    #[tokio::test]
    async fn authorization_code_is_exchanged_directly_with_the_official_endpoint() {
        use axum::{body::Bytes, Json, Router};
        let _sandbox = super::super::super::tests::Sandbox::new();
        let app = Router::new().route("/oauth/token", axum::routing::post(|body: Bytes| async move {
            let fields: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&body).into_owned().collect();
            assert_eq!(fields["grant_type"], "authorization_code");
            assert_eq!(fields["code"], "private-code");
            assert_eq!(fields["code_verifier"], "private-verifier");
            assert_eq!(fields["redirect_uri"], REDIRECT);
            assert_eq!(fields["client_id"], super::super::super::chatgpt::auth::CLIENT_ID);
            Json(json!({"access_token":"private-access", "refresh_token":"private-refresh", "id_token":"private-id"}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        super::super::super::chatgpt::endpoints::set_test(Some(
            super::super::super::chatgpt::endpoints::Endpoints {
                auth: base.clone(),
                backend: base,
            },
        ));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                super::super::super::chatgpt::endpoints::set_test(None);
            }
        }
        let _reset = Reset;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let account = exchange(&client, "private-code", "private-verifier")
            .await
            .unwrap();
        assert_eq!(account["platform"], "openai");
        assert_eq!(account["credentials"]["refresh_token"], "private-refresh");
        server.abort();
    }
}
