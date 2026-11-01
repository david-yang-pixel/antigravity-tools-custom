//! Administrator-defined OpenAI-compatible upstreams: a base URL plus an optional API key.
//!
//! Each upstream is its own model-registry source (`openai-compatible:<id>`); models are
//! published, verified and routed through the registry exactly like the built-in sources.
//! Settings live in `custom_upstreams.json` (API keys encrypted at rest; a hand-written
//! plaintext key is accepted and encrypted on the next save). For headless deployments the
//! `ABV_CUSTOM_UPSTREAMS` environment variable (a JSON array of upstreams) replaces the list.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};

pub const SOURCE_PREFIX: &str = "openai-compatible:";
pub const PROVIDER: &str = "openai_compatible";
pub const ENV_VAR: &str = "ABV_CUSTOM_UPSTREAMS";
const MAX_UPSTREAMS: usize = 50;
const MAX_MODELS: usize = 2_000;
const MAX_MODEL_ID_BYTES: usize = 512;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;

static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Upstream {
    /// Stable identity; part of the registry source ID, so it never changes after creation.
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// API root that serves `/chat/completions` and `/models`, e.g. `https://api.example.com/v1`.
    pub base_url: String,
    /// Sent as `Authorization: Bearer <key>`; empty for keyless local servers.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        serialize_with = "crate::utils::crypto::serialize_password",
        deserialize_with = "crate::utils::crypto::deserialize_password"
    )]
    pub api_key: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Native model IDs declared by the administrator, for upstreams without a `/models` list.
    #[serde(default)]
    pub models: Vec<String>,
}

impl Upstream {
    pub fn source_id(&self) -> String {
        source_id(&self.id)
    }
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            self.name.trim()
        }
    }
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }
    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            request
        } else {
            request.bearer_auth(&self.api_key)
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub upstreams: Vec<Upstream>,
}

pub fn source_id(id: &str) -> String {
    format!("{SOURCE_PREFIX}{id}")
}
/// The upstream ID of a well-formed custom source. Registry validation is syntactic, so a
/// removed upstream never invalidates the registry; routing reports it as unavailable.
pub fn upstream_id(source: &str) -> Option<&str> {
    source
        .strip_prefix(SOURCE_PREFIX)
        .filter(|id| valid_upstream_id(id))
}
pub fn is_source(source: &str) -> bool {
    upstream_id(source).is_some()
}
pub fn valid_upstream_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_ID_BYTES
        && !id.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '*' | '?' | '[' | ']' | '{' | '}')
        })
}

/// Accepts the API root with or without a trailing slash or a pasted `/chat/completions`.
pub fn normalize_base_url(raw: &str) -> Result<String, String> {
    let mut value = raw.trim().trim_end_matches('/');
    if let Some(root) = value.strip_suffix("/chat/completions") {
        value = root.trim_end_matches('/');
    }
    let url = url::Url::parse(value).map_err(|_| "Base URL must be an absolute http(s) URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err("Base URL must be an absolute http(s) URL".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Put credentials in the API key field, not in the base URL".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("Base URL cannot contain a query string or fragment".into());
    }
    Ok(value.to_string())
}

fn normalize(mut upstream: Upstream) -> Result<Upstream, String> {
    upstream.id = upstream.id.trim().to_string();
    if !valid_upstream_id(&upstream.id) {
        return Err("Upstream ID must be 1-32 lowercase letters, digits, '-' or '_'".into());
    }
    upstream.name = upstream.name.trim().to_string();
    if upstream.name.chars().count() > 64 || upstream.name.chars().any(char::is_control) {
        return Err("Upstream name must be at most 64 printable characters".into());
    }
    upstream.base_url = normalize_base_url(&upstream.base_url)?;
    upstream.api_key = upstream.api_key.trim().to_string();
    if upstream.api_key.len() > 4096
        || upstream
            .api_key
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("API key must be at most 4096 characters without spaces".into());
    }
    let mut seen = BTreeSet::new();
    upstream.models = upstream
        .models
        .into_iter()
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty() && seen.insert(model.clone()))
        .collect();
    if upstream.models.len() > MAX_MODELS || !upstream.models.iter().all(|m| valid_model_id(m)) {
        return Err(format!(
            "Declared models must be at most {MAX_MODELS} IDs without spaces or wildcards"
        ));
    }
    Ok(upstream)
}

fn validate(settings: Settings) -> Result<Settings, String> {
    if settings.upstreams.len() > MAX_UPSTREAMS {
        return Err(format!(
            "At most {MAX_UPSTREAMS} custom upstreams are supported"
        ));
    }
    let mut ids = BTreeSet::new();
    let mut upstreams = Vec::with_capacity(settings.upstreams.len());
    for upstream in settings.upstreams {
        let upstream = normalize(upstream)?;
        if !ids.insert(upstream.id.clone()) {
            return Err(format!("Duplicate custom upstream ID {:?}", upstream.id));
        }
        upstreams.push(upstream);
    }
    Ok(Settings { upstreams })
}

fn settings_path() -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join("custom_upstreams.json"))
}
fn catalog_path() -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join("custom_upstream_models.json"))
}

/// Parsed settings reused until the file is rewritten; every routed request consults them.
static STORED: Mutex<Option<(PathBuf, crate::utils::fs::FileStamp, Settings)>> = Mutex::new(None);

fn stored() -> Result<Settings, String> {
    let path = settings_path()?;
    // Stamp before reading: a rewrite during the read only causes one extra reload.
    let stamp = crate::utils::fs::file_stamp(&path);
    if let (Some(stamp), Ok(cache)) = (&stamp, STORED.lock()) {
        if let Some((_, _, settings)) = cache
            .as_ref()
            .filter(|(cached_path, cached, _)| cached_path == &path && cached == stamp)
        {
            return Ok(settings.clone());
        }
    }
    let settings: Settings = match std::fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data).map_err(|_| "Invalid custom_upstreams.json")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(_) => return Err("Cannot read custom upstream settings".into()),
    };
    if let (Some(stamp), Ok(mut cache)) = (stamp, STORED.lock()) {
        *cache = Some((path, stamp, settings.clone()));
    }
    Ok(settings)
}

/// `Some` when the environment owns the list; the GUI then shows it read-only.
fn environment() -> Result<Option<Settings>, String> {
    match std::env::var(ENV_VAR) {
        Ok(value) if !value.trim().is_empty() => {
            let upstreams: Vec<Upstream> = serde_json::from_str(&value)
                .map_err(|_| format!("{ENV_VAR} must be a JSON array of upstreams"))?;
            Ok(Some(Settings { upstreams }))
        }
        _ => Ok(None),
    }
}

/// Effective, validated settings. A broken file or variable fails closed for custom sources
/// only; built-in providers never read this.
pub fn load() -> Result<Settings, String> {
    validate(match environment()? {
        Some(settings) => settings,
        None => stored()?,
    })
}

pub fn get(id: &str) -> Result<Option<Upstream>, String> {
    Ok(load()?.upstreams.into_iter().find(|u| u.id == id))
}

/// For the startup gate: whether any custom upstream can serve requests.
pub fn any_enabled() -> bool {
    load().is_ok_and(|settings| settings.upstreams.iter().any(|u| u.enabled))
}

// ---- Administrator view and updates (API keys are never returned) ----

#[derive(Serialize)]
pub struct UpstreamView {
    pub id: String,
    pub source_id: String,
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
    pub models: Vec<String>,
    pub api_key_set: bool,
    /// The last four characters only, to tell keys apart.
    pub api_key_hint: String,
    pub catalog_count: usize,
}
#[derive(Serialize)]
pub struct SettingsView {
    pub upstreams: Vec<UpstreamView>,
    /// Set when `ABV_CUSTOM_UPSTREAMS` owns the list; saving is then refused.
    pub env_locked: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamUpdate {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub base_url: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<String>,
    /// `None` keeps the stored key of an existing upstream; `Some("")` clears it.
    #[serde(default)]
    pub api_key: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    pub upstreams: Vec<UpstreamUpdate>,
}

fn key_hint(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 12 {
        return String::new();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("…{tail}")
}

pub fn view() -> Result<SettingsView, String> {
    let env_locked = environment()?.is_some();
    let settings = load()?;
    let catalog = read_catalog().unwrap_or_default();
    Ok(SettingsView {
        upstreams: settings
            .upstreams
            .into_iter()
            .map(|u| UpstreamView {
                source_id: u.source_id(),
                catalog_count: catalog.get(&u.id).map_or(0, Vec::len),
                api_key_set: !u.api_key.is_empty(),
                api_key_hint: key_hint(&u.api_key),
                id: u.id,
                name: u.name,
                base_url: u.base_url,
                enabled: u.enabled,
                models: u.models,
            })
            .collect(),
        env_locked,
    })
}

/// Replaces the list. Removing an upstream that still backs a publication is refused, so
/// published models never silently lose their route.
pub fn save(update: SettingsUpdate) -> Result<SettingsView, String> {
    {
        let _lock = SETTINGS_LOCK
            .lock()
            .map_err(|_| "Custom upstream settings unavailable")?;
        if environment()?.is_some() {
            return Err(format!(
                "Custom upstreams are managed by the {ENV_VAR} environment variable"
            ));
        }
        let previous = stored()?;
        let mut upstreams = Vec::with_capacity(update.upstreams.len());
        for item in update.upstreams {
            let id = item.id.trim().to_string();
            let old = previous.upstreams.iter().find(|u| u.id == id);
            let api_key = match (item.api_key, old) {
                (Some(key), _) => key,
                (None, Some(old)) => old.api_key.clone(),
                (None, None) => String::new(),
            };
            upstreams.push(Upstream {
                id,
                name: item.name,
                base_url: item.base_url,
                api_key,
                enabled: item.enabled,
                models: item.models,
            });
        }
        let settings = validate(Settings { upstreams })?;
        let kept: BTreeSet<&str> = settings.upstreams.iter().map(|u| u.id.as_str()).collect();
        for removed in previous
            .upstreams
            .iter()
            .filter(|u| !kept.contains(u.id.as_str()))
        {
            let bound = crate::modules::model_registry::publication_count(&removed.source_id())?;
            if bound > 0 {
                return Err(format!(
                    "Upstream {:?} still backs {bound} published model(s); remove those publications first",
                    removed.display_name()
                ));
            }
        }
        let data = serde_json::to_vec_pretty(&settings)
            .map_err(|_| "Cannot serialize custom upstream settings")?;
        crate::utils::fs::write_atomic(settings_path()?, &data)
            .map_err(|_| "Cannot save custom upstream settings".to_string())?;
        prune_catalog(&kept)?;
    }
    view()
}

// ---- Discovered model catalog (metadata only; never grants publication) ----

fn read_catalog() -> Result<BTreeMap<String, Vec<String>>, String> {
    match std::fs::read(catalog_path()?) {
        Ok(data) => {
            serde_json::from_slice(&data).map_err(|_| "Invalid custom_upstream_models.json".into())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(_) => Err("Cannot read custom upstream model catalog".into()),
    }
}
fn write_catalog(catalog: &BTreeMap<String, Vec<String>>) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(catalog).map_err(|_| "Cannot serialize model catalog")?;
    crate::utils::fs::write_atomic(catalog_path()?, &data)
        .map_err(|_| "Cannot save custom upstream model catalog".to_string())
}
fn prune_catalog(kept: &BTreeSet<&str>) -> Result<(), String> {
    let mut catalog = read_catalog().unwrap_or_default();
    let before = catalog.len();
    catalog.retain(|id, _| kept.contains(id.as_str()));
    if catalog.len() != before {
        write_catalog(&catalog)?;
    }
    Ok(())
}
static CATALOG_LOCK: Mutex<()> = Mutex::new(());

pub fn record_catalog(id: &str, models: &[String]) -> Result<(), String> {
    let _lock = CATALOG_LOCK
        .lock()
        .map_err(|_| "Custom upstream catalog unavailable")?;
    let mut catalog = read_catalog().unwrap_or_default();
    catalog.insert(id.to_string(), models.to_vec());
    write_catalog(&catalog)
}

/// Declared models plus the last fetched `/models` list.
pub fn known_models(upstream: &Upstream) -> BTreeSet<String> {
    let mut models: BTreeSet<String> = upstream.models.iter().cloned().collect();
    if let Ok(catalog) = read_catalog() {
        if let Some(fetched) = catalog.get(&upstream.id) {
            models.extend(fetched.iter().cloned());
        }
    }
    models
}

// ---- HTTP ----

/// One client per outbound proxy setting (environment proxies are ignored). No overall timeout: callers bound each request
/// by its own deadline, which also covers long streams. Redirects are never followed so
/// the API key cannot be replayed to another host.
pub fn http(proxy: &crate::proxy::config::UpstreamProxyConfig) -> Result<reqwest::Client, String> {
    static CLIENT: Mutex<Option<(String, reqwest::Client)>> = Mutex::new(None);
    let key = if proxy.enabled { proxy.url.trim() } else { "" }.to_string();
    let mut cache = CLIENT
        .lock()
        .map_err(|_| "Custom upstream connection unavailable")?;
    if let Some((cached, client)) = cache.as_ref() {
        if cached == &key {
            return Ok(client.clone());
        }
    }
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15));
    // The gateway's upstream proxy setting is the only proxy source: never fall back to
    // HTTP(S)_PROXY, which would send the API key through a proxy nobody configured.
    builder = if key.is_empty() {
        builder.no_proxy()
    } else {
        let proxy = reqwest::Proxy::all(&key).map_err(|_| "Invalid upstream proxy URL")?;
        builder.proxy(proxy)
    };
    let client = builder
        .build()
        .map_err(|_| "Cannot initialize custom upstream connection")?;
    *cache = Some((key, client.clone()));
    Ok(client)
}

/// For admin actions without a running server's hot configuration.
pub fn admin_http() -> Result<reqwest::Client, String> {
    let proxy = crate::modules::config::load_app_config()
        .map(|config| config.proxy.upstream_proxy)
        .unwrap_or_default();
    http(&proxy)
}

async fn bounded_json(response: reqwest::Response, limit: usize) -> Result<Value, String> {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Upstream response was interrupted")?;
        if data.len() + chunk.len() > limit {
            return Err("Upstream response exceeds size limit".into());
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|_| "Upstream returned invalid JSON".into())
}

/// `GET {base}/models`, metadata only. Accepts the OpenAI `{"data":[{"id"}]}` shape and the
/// bare-array variant some gateways return. Invalid IDs are skipped, not fatal.
pub async fn fetch_models(upstream: &Upstream) -> Result<Vec<String>, String> {
    let response = upstream
        .authorize(admin_http()?.get(upstream.endpoint("models")))
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| "Cannot reach the upstream; check the base URL and network")?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 | 403 => "The upstream rejected the API key".to_string(),
            404 => "The upstream has no /models list; declare model IDs manually".to_string(),
            code => format!("The upstream returned HTTP {code} for /models"),
        });
    }
    let value = bounded_json(response, MAX_CATALOG_BYTES).await?;
    let items = value
        .get("data")
        .or_else(|| value.get("models"))
        .unwrap_or(&value)
        .as_array()
        .ok_or("The upstream /models response has no model list")?;
    let mut models = BTreeSet::new();
    for item in items {
        let id = item
            .get("id")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .or_else(|| item.as_str())
            .unwrap_or_default();
        if valid_model_id(id) && models.len() < MAX_MODELS {
            models.insert(id.to_string());
        }
    }
    Ok(models.into_iter().collect())
}

pub enum Probe {
    Verified,
    Unavailable,
    Failed(String),
}

/// One non-streaming basic-text request. Only an explicit "model not found" answer marks the
/// model unavailable; any other failure stays inconclusive.
pub async fn probe(upstream: &Upstream, model: &str) -> Result<Probe, String> {
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with only OK."}],
        "stream": false,
    });
    let response = upstream
        .authorize(admin_http()?.post(upstream.endpoint("chat/completions")))
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| "Cannot reach the upstream")?;
    let status = response.status();
    let value = bounded_json(response, 1024 * 1024)
        .await
        .unwrap_or(Value::Null);
    if !status.is_success() {
        let code = value
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if code == "model_not_found" {
            return Ok(Probe::Unavailable);
        }
        let detail = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Ok(Probe::Failed(format!(
            "HTTP {}: {}",
            status.as_u16(),
            crate::proxy::upstream::client::sanitize_error_for_log(detail)
        )));
    }
    let message = value.pointer("/choices/0/message");
    let answered = ["content", "reasoning_content"].iter().any(|key| {
        message
            .and_then(|m| m.get(*key))
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
    });
    Ok(if answered {
        Probe::Verified
    } else {
        Probe::Failed("no answer text".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(id: &str) -> UpstreamUpdate {
        UpstreamUpdate {
            id: id.into(),
            name: "Relay".into(),
            base_url: "https://relay.example/v1/".into(),
            enabled: true,
            models: vec![
                "gpt-4o".into(),
                " gpt-4o ".into(),
                "deepseek/chat:free".into(),
            ],
            api_key: Some("sk-test-1234567890abcd".into()),
        }
    }

    #[test]
    fn base_url_normalization_accepts_pasted_endpoints_and_rejects_credentials() {
        assert_eq!(
            normalize_base_url(" https://api.example.com/v1/chat/completions/ ").unwrap(),
            "https://api.example.com/v1"
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:11434/v1").unwrap(),
            "http://127.0.0.1:11434/v1"
        );
        for bad in [
            "api.example.com/v1",
            "ftp://example.com",
            "https://user:pass@example.com/v1",
            "https://example.com/v1?key=1",
            "",
        ] {
            assert!(normalize_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn source_ids_round_trip_and_reject_foreign_prefixes() {
        assert_eq!(upstream_id(&source_id("relay-1")), Some("relay-1"));
        assert!(upstream_id("openai-compatible:Bad Id").is_none());
        assert!(upstream_id("workbuddy:default").is_none());
        assert!(!is_source("openai-compatible:"));
    }

    #[test]
    fn keys_are_encrypted_at_rest_kept_on_edit_and_never_viewed() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let view = save(SettingsUpdate {
            upstreams: vec![upstream("relay")],
        })
        .unwrap();
        let item = &view.upstreams[0];
        assert_eq!(item.base_url, "https://relay.example/v1");
        assert_eq!(item.models, ["gpt-4o", "deepseek/chat:free"]);
        assert!(item.api_key_set);
        assert_eq!(item.api_key_hint, "…abcd");
        assert_eq!(item.source_id, "openai-compatible:relay");
        let raw = std::fs::read_to_string(settings_path().unwrap()).unwrap();
        assert!(!raw.contains("sk-test-1234567890abcd"));
        // Omitting the key keeps it; renaming does not change the identity.
        let mut edit = upstream("relay");
        edit.api_key = None;
        edit.name = "Renamed".into();
        save(SettingsUpdate {
            upstreams: vec![edit],
        })
        .unwrap();
        let stored = get("relay").unwrap().unwrap();
        assert_eq!(stored.api_key, "sk-test-1234567890abcd");
        assert_eq!(stored.name, "Renamed");
        // A plaintext key written by hand is accepted.
        std::fs::write(
            settings_path().unwrap(),
            r#"{"upstreams":[{"id":"local","base_url":"http://127.0.0.1:8000/v1","api_key":"plain"}]}"#,
        )
        .unwrap();
        let local = get("local").unwrap().unwrap();
        assert_eq!(local.api_key, "plain");
        assert!(local.enabled);
    }

    #[test]
    fn invalid_or_duplicate_upstreams_are_rejected() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        assert!(save(SettingsUpdate {
            upstreams: vec![upstream("relay"), upstream("relay")],
        })
        .is_err());
        assert!(save(SettingsUpdate {
            upstreams: vec![upstream("Relay")],
        })
        .is_err());
        let mut wildcard = upstream("relay");
        wildcard.models = vec!["gpt-*".into()];
        assert!(save(SettingsUpdate {
            upstreams: vec![wildcard],
        })
        .is_err());
        assert!(load().unwrap().upstreams.is_empty());
    }

    #[test]
    fn key_hint_hides_short_keys() {
        assert_eq!(key_hint("short"), "");
        assert_eq!(key_hint("sk-0123456789wxyz"), "…wxyz");
    }
}
