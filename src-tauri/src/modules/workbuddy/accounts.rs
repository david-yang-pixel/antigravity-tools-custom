//! WorkBuddy's account pool has its own encrypted store, never Google token files.
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, Weak},
};

#[path = "accounts_scheduler.rs"]
pub mod scheduling;

static STORE_LOCK: Mutex<()> = Mutex::new(());
static RUNTIME: LazyLock<Mutex<HashMap<String, Runtime>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REFRESH_LOCKS: LazyLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Realm {
    #[default]
    Cn,
    Global,
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Account {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    pub realm: Realm,
    pub enterprise_id: String,
    pub domain: String,
    pub access_token: String,
    pub refresh_token: String,
    pub device_token: String,
    pub expires_at: i64,
    pub enabled: bool,
    pub models: Vec<Model>,
    /// Successful probes are separate from the upstream candidate catalog.
    #[serde(default)]
    pub verified_models: HashMap<String, i64>,
    #[serde(default)]
    pub unavailable_models: HashSet<String>,
    #[serde(default)]
    pub models_verified_at: i64,
    pub last_error: Option<String>,
    pub revision: String,
    #[serde(default)]
    pub management: super::maintenance::Management,
    /// Stable per-account UA / machine / session; never shared across accounts.
    #[serde(default)]
    pub client: super::identity::ClientIdentity,
}
#[derive(Serialize)]
pub struct AccountView {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    pub realm: Realm,
    pub enabled: bool,
    pub expires_at: i64,
    pub models: Vec<Model>,
    pub candidate_model_count: usize,
    pub models_verified_at: i64,
    pub last_error: Option<String>,
    pub status: String,
    pub in_flight: usize,
    pub management: super::maintenance::Management,
    pub health: Health,
    pub cooldown_until: i64,
    pub cooldown_reason: Option<String>,
}
#[derive(Clone, Default, Serialize)]
pub struct Health {
    pub successes: u64,
    pub failures: u64,
    pub consecutive_failures: u64,
    pub last_success_at: i64,
    pub last_failure_at: i64,
    pub last_error: Option<String>,
}
impl Account {
    pub fn available_models(&self) -> Vec<Model> {
        self.models
            .iter()
            .filter(|model| {
                self.models_verified_at == 0 || self.verified_models.contains_key(&model.id)
            })
            .cloned()
            .collect()
    }
    pub fn supports_model(&self, model: &str) -> bool {
        // Unknown is not unavailable. Upgrades, reauthorization and transient
        // probe failures must not make verification an admission prerequisite.
        !self.unavailable_models.contains(model) && self.models.iter().any(|m| m.id == model)
    }
}
#[derive(Default)]
struct Runtime {
    health: Health,
    cooldown_reason: Option<String>,
    in_flight: usize,
    last_used: u64,
    cooldown_until: i64,
}
#[derive(Deserialize, Serialize)]
struct Envelope {
    version: u8,
    ciphertext: String,
}
fn path() -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join("workbuddy_accounts.json"))
}
fn key_path() -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join("workbuddy_accounts.key"))
}
fn key(create: bool) -> Result<[u8; 32], String> {
    let path = key_path()?;
    if create && !path.exists() {
        let mut value = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut value);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&value)
                    .and_then(|_| file.sync_all())
                    .map_err(|_| "Cannot persist WorkBuddy encryption key")?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("Cannot create WorkBuddy encryption key".into()),
        }
    }
    std::fs::read(path)
        .map_err(|_| "WorkBuddy encryption key is missing; restore it with the account backup")?
        .try_into()
        .map_err(|_| "Invalid WorkBuddy encryption key".into())
}
/// Decrypted accounts and the store/key versions they came from. Every request
/// reads the pool, while the store changes only on maintenance and admin edits.
type StoreStamp = (crate::utils::fs::FileStamp, crate::utils::fs::FileStamp);
static DECRYPTED: Mutex<Option<(StoreStamp, Vec<Account>)>> = Mutex::new(None);

fn read() -> Result<Vec<Account>, String> {
    let path = path()?;
    // Stamp before reading: a rewrite during the read only causes one extra reload.
    let stamp = crate::utils::fs::file_stamp(&path).zip(crate::utils::fs::file_stamp(&key_path()?));
    let cached = stamp.as_ref().and_then(|stamp| {
        let cache = DECRYPTED.lock().ok()?;
        cache
            .as_ref()
            .filter(|(cached, _)| cached == stamp)
            .map(|(_, accounts)| accounts.clone())
    });
    let mut accounts = match cached {
        Some(accounts) => accounts,
        None => {
            let accounts = decrypt_store(&path)?;
            if let (Some(stamp), Ok(mut cache)) = (stamp, DECRYPTED.lock()) {
                *cache = Some((stamp, accounts.clone()));
            }
            accounts
        }
    };
    // Depends on in-process activity state, so it runs on every read.
    for account in &mut accounts {
        super::activity::recover_interrupted(&mut account.management.activity);
    }
    Ok(accounts)
}
fn decrypt_store(path: &std::path::Path) -> Result<Vec<Account>, String> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("Cannot read WorkBuddy accounts".into()),
    };
    let envelope: Envelope =
        serde_json::from_slice(&data).map_err(|_| "Invalid WorkBuddy account store")?;
    if envelope.version != 1 {
        return Err("Unsupported WorkBuddy account store version".into());
    }
    let encrypted = STANDARD
        .decode(envelope.ciphertext)
        .map_err(|_| "Invalid WorkBuddy account store")?;
    if encrypted.len() < 28 {
        return Err("Invalid WorkBuddy account store".into());
    }
    let cipher = Aes256Gcm::new(&key(false)?.into());
    let plain = cipher
        .decrypt(Nonce::from_slice(&encrypted[..12]), &encrypted[12..])
        .map_err(|_| {
            "Cannot decrypt WorkBuddy accounts; restore the matching key and account backup"
        })?;
    serde_json::from_slice(&plain).map_err(|_| "Invalid WorkBuddy accounts".into())
}
fn write(accounts: &[Account]) -> Result<(), String> {
    let cipher = Aes256Gcm::new(&key(true)?.into());
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let plain = serde_json::to_vec(accounts).map_err(|_| "Cannot serialize WorkBuddy accounts")?;
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
        .map_err(|_| "Cannot encrypt WorkBuddy accounts")?;
    let mut bytes = nonce.to_vec();
    bytes.extend(encrypted);
    let envelope = Envelope {
        version: 1,
        ciphertext: STANDARD.encode(bytes),
    };
    let data = serde_json::to_vec(&envelope).map_err(|_| "Cannot encode WorkBuddy accounts")?;
    crate::utils::fs::write_atomic(&path()?, &data)
        .map_err(|_| "Cannot save WorkBuddy accounts".into())
}
pub(super) fn all() -> Result<Vec<Account>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    read()
}
fn runtime_key(id: &str) -> Result<String, String> {
    Ok(format!("{}:{id}", path()?.display()))
}
pub(super) fn view(mut account: Account) -> Result<AccountView, String> {
    // The draw journal is persisted for retry safety, not exposed in the admin UI.
    account.management.activity.pending_draw = None;
    let runtime = RUNTIME.lock().map_err(|_| "WorkBuddy pool unavailable")?;
    let entry = runtime.get(&runtime_key(&account.id)?);
    let status = if !account.enabled {
        "disabled"
    } else if entry.is_some_and(|e| e.cooldown_until > now()) {
        "cooldown"
    } else if account.expires_at <= now() {
        "expired"
    } else if account.last_error.is_some() {
        "error"
    } else if !account
        .models
        .iter()
        .any(|model| account.supports_model(&model.id))
    {
        "needs_models"
    } else {
        "ready"
    };
    let models = account.available_models();
    Ok(AccountView {
        id: account.id,
        uid: account.uid,
        nickname: account.nickname,
        realm: account.realm,
        enabled: account.enabled,
        expires_at: account.expires_at,
        models,
        candidate_model_count: account.models.len(),
        models_verified_at: account.models_verified_at,
        last_error: account.last_error,
        status: status.into(),
        in_flight: entry.map(|e| e.in_flight).unwrap_or(0),
        management: account.management,
        health: entry.map(|e| e.health.clone()).unwrap_or_default(),
        cooldown_until: entry.map(|e| e.cooldown_until).unwrap_or(0),
        cooldown_reason: entry.and_then(|e| e.cooldown_reason.clone()),
    })
}
pub fn list() -> Result<Vec<AccountView>, String> {
    all()?.into_iter().map(view).collect()
}
pub fn get(id: &str) -> Result<Account, String> {
    all()?
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| "WorkBuddy account not found".into())
}
pub fn validate(account: &Account) -> Result<(), String> {
    if account.uid.is_empty() || account.uid.len() > 128 || account.nickname.len() > 256 {
        return Err("Invalid WorkBuddy account identity".into());
    }
    for value in [
        &account.uid,
        &account.enterprise_id,
        &account.domain,
        &account.access_token,
        &account.refresh_token,
        &account.device_token,
    ] {
        if value.len() > 16384
            || value.starts_with("ag_enc_")
            || axum::http::HeaderValue::from_str(value).is_err()
        {
            return Err("Invalid WorkBuddy credential or identity header".into());
        }
    }
    if account.access_token.is_empty() {
        return Err("WorkBuddy accessToken is required".into());
    }
    Ok(())
}
/// Reauthorization replaces only the same realm + uid, preserving enablement and model catalog.
pub fn upsert(mut account: Account) -> Result<AccountView, String> {
    validate(&account)?;
    {
        let _lock = STORE_LOCK
            .lock()
            .map_err(|_| "WorkBuddy account store unavailable")?;
        let mut accounts = read()?;
        if let Some(old) = accounts
            .iter_mut()
            .find(|a| a.uid == account.uid && a.realm == account.realm)
        {
            account.id = old.id.clone();
            account.enabled = old.enabled;
            account.models = old.models.clone();
            account.verified_models = old.verified_models.clone();
            account.unavailable_models = old.unavailable_models.clone();
            account.models_verified_at = old.models_verified_at;
            account.management = old.management.clone();
            account.management.next_sync_at = 0;
            // Stable per-account fingerprints survive reauthorization even when the
            // new payload carries a different non-empty device_token / client block.
            if !old.device_token.is_empty() {
                account.device_token = old.device_token.clone();
            }
            if account.refresh_token.is_empty() {
                account.refresh_token = old.refresh_token.clone();
            }
            if !old.client.machine_id.is_empty() {
                account.client = old.client.clone();
            }
            *old = account.clone();
        } else {
            if accounts.len() >= 500 {
                return Err("WorkBuddy account limit reached (500)".into());
            }
            accounts.push(account.clone());
        }
        if let Some(entry) = accounts.iter_mut().find(|a| a.id == account.id) {
            let _ = super::identity::ensure(entry);
            account = entry.clone();
        }
        write(&accounts)?;
    }
    view(account)
}
/// Backfill stable client fingerprints for accounts created before identity existed.
pub fn ensure_identities() -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    let mut accounts = read()?;
    let mut dirty = false;
    for account in &mut accounts {
        dirty |= super::identity::ensure(account);
    }
    if dirty {
        write(&accounts)?;
    }
    Ok(())
}
pub fn set_enabled(id: &str, enabled: bool) -> Result<AccountView, String> {
    let account = {
        let _lock = STORE_LOCK
            .lock()
            .map_err(|_| "WorkBuddy account store unavailable")?;
        let mut accounts = read()?;
        let a = accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or("WorkBuddy account not found")?;
        a.enabled = enabled;
        let result = a.clone();
        write(&accounts)?;
        result
    };
    view(account)
}
pub fn remove(id: &str) -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    let mut accounts = read()?;
    let count = accounts.len();
    accounts.retain(|a| a.id != id);
    if accounts.len() == count {
        return Err("WorkBuddy account not found".into());
    }
    write(&accounts)
}
fn update_if_current(account: &mut Account) -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    let mut accounts = read()?;
    let old = accounts
        .iter_mut()
        .find(|a| a.id == account.id && a.revision == account.revision)
        .ok_or("WorkBuddy account was removed or reauthorized; retry")?;
    // A refresh must not undo a simultaneous administrator disable action.
    account.enabled = old.enabled;
    account.management = old.management.clone();
    account.models = old.models.clone();
    account.verified_models = old.verified_models.clone();
    account.unavailable_models = old.unavailable_models.clone();
    account.models_verified_at = old.models_verified_at;
    // Keep a fingerprint that another path may have backfilled while this refresh ran.
    if account.client.machine_id.is_empty() {
        account.client = old.client.clone();
    }
    if account.device_token.is_empty() {
        account.device_token = old.device_token.clone();
    }
    let _ = super::identity::ensure(account);
    account.revision = uuid::Uuid::new_v4().to_string();
    *old = account.clone();
    write(&accounts)
}
pub(super) fn refresh_lock(id: &str) -> Result<Arc<tokio::sync::Mutex<()>>, String> {
    let mut locks = REFRESH_LOCKS
        .lock()
        .map_err(|_| "WorkBuddy refresh unavailable")?;
    locks.retain(|_, value| value.strong_count() > 0);
    let key = runtime_key(id)?;
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    Ok(lock)
}
pub async fn fresh(id: &str, force: bool) -> Result<Account, String> {
    let lock = refresh_lock(id)?;
    let _guard = lock.lock().await;
    fresh_locked(id, force).await
}

/// Limit request-side waiting without cancelling an already started token rotation.
/// An outer timeout means no account was prepared before the request deadline.
pub async fn fresh_before(
    id: &str,
    deadline: tokio::time::Instant,
) -> Result<Result<Account, String>, tokio::time::error::Elapsed> {
    tokio::time::timeout_at(deadline, async {
        let lock = refresh_lock(id)?;
        // A timed-out waiter must not start another refresh after the lock is released.
        let guard = lock.lock_owned().await;
        let id = id.to_owned();
        tokio::spawn(async move {
            let _guard = guard;
            // Retain the lock through persistence if the caller times out or disconnects.
            // The refresh HTTP request remains bounded by its own 25-second timeout.
            fresh_locked(&id, false).await
        })
        .await
        .map_err(|_| "WorkBuddy credential preparation failed".to_owned())?
    })
    .await
}

async fn fresh_locked(id: &str, force: bool) -> Result<Account, String> {
    let mut account = get(id)?;
    if force || account.expires_at <= now() + 600 {
        edit_management(id, |m| m.keepalive_attempt_at = now())?;
        match super::client::refresh(&mut account).await {
            Ok(()) => {
                account.last_error = None;
                update_if_current(&mut account)?;
                edit_management(id, |m| {
                    m.last_keepalive_at = now();
                    m.keepalive_error = None;
                })?;
                account = get(id)?;
            }
            Err(error) => {
                account.last_error = Some(error.clone());
                update_if_current(&mut account)?;
                edit_management(id, |m| m.keepalive_error = Some(error.clone()))?;
                return Err(error);
            }
        }
    }
    Ok(account)
}
pub async fn sync(id: &str, force_refresh: bool) -> Result<AccountView, String> {
    super::maintenance::run(id, force_refresh, false, false).await
}
pub(super) fn edit_management(
    id: &str,
    edit: impl FnOnce(&mut super::maintenance::Management),
) -> Result<AccountView, String> {
    let result = {
        let _lock = STORE_LOCK
            .lock()
            .map_err(|_| "WorkBuddy account store unavailable")?;
        let mut accounts = read()?;
        let a = accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or("WorkBuddy account not found")?;
        edit(&mut a.management);
        let result = a.clone();
        write(&accounts)?;
        result
    };
    view(result)
}
pub fn configure(
    id: &str,
    managed: Option<bool>,
    auto_checkin: Option<bool>,
    keepalive: Option<bool>,
    auto_rewards: Option<bool>,
    auto_lottery: Option<bool>,
) -> Result<AccountView, String> {
    if (auto_rewards == Some(true) || auto_lottery == Some(true)) && get(id)?.realm != Realm::Cn {
        return Err("WorkBuddy activity rewards are only supported for domestic accounts".into());
    }
    edit_management(id, |m| {
        if let Some(value) = managed {
            m.managed = value;
        }
        if let Some(value) = auto_checkin {
            m.auto_checkin = value;
        }
        if let Some(value) = keepalive {
            m.keepalive = value;
        }
        if let Some(value) = auto_rewards {
            m.auto_rewards = value;
        }
        if let Some(value) = auto_lottery {
            m.auto_lottery = value;
        }
        m.next_sync_at = 0;
    })
}
pub(super) fn save_observation(account: &mut Account) -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    let mut accounts = read()?;
    let old = accounts
        .iter_mut()
        .find(|a| a.id == account.id && a.revision == account.revision)
        .ok_or("WorkBuddy credentials changed during sync; retry")?;
    // Preserve concurrent enablement/preferences and a newer keepalive observation.
    account.enabled = old.enabled;
    account.management.managed = old.management.managed;
    account.management.auto_checkin = old.management.auto_checkin;
    account.management.keepalive = old.management.keepalive;
    account.management.auto_rewards = old.management.auto_rewards;
    account.management.auto_lottery = old.management.auto_lottery;
    account.management.activity = old.management.activity.clone();
    if account.client.machine_id.is_empty() {
        account.client = old.client.clone();
    }
    if account.device_token.is_empty() {
        account.device_token = old.device_token.clone();
    }
    let _ = super::identity::ensure(account);
    old.client = account.client.clone();
    old.device_token = account.device_token.clone();
    old.models = account.models.clone();
    // Maintenance does not turn catalog entries into successful verification results.
    old.verified_models
        .retain(|id, _| old.models.iter().any(|model| &model.id == id));
    // A partial catalog sync must not erase a conclusive rejection. Only a
    // successful explicit probe can restore access for the same account identity.
    account.verified_models = old.verified_models.clone();
    account.unavailable_models = old.unavailable_models.clone();
    account.models_verified_at = old.models_verified_at;
    old.last_error = account.last_error.clone();
    old.management = account.management.clone();
    write(&accounts)
}
/// Activity writes only its own observation and never stale credentials/preferences.
pub(super) fn edit_activity(
    account: &Account,
    edit: impl FnOnce(&mut super::maintenance::Management),
) -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    let mut accounts = read()?;
    let current = accounts
        .iter_mut()
        .find(|a| a.id == account.id && a.revision == account.revision)
        .ok_or("WorkBuddy credentials changed during activity; query again")?;
    edit(&mut current.management);
    write(&accounts)
}
/// Finish this job's journal even if a concurrent credential refresh changed the
/// revision. The run ID prevents an old job from replacing a later job's result.
pub(super) fn save_activity(
    account: &Account,
    state: super::activity::State,
) -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    let mut accounts = read()?;
    let current = accounts
        .iter_mut()
        .find(|a| {
            a.id == account.id
                && a.uid == account.uid
                && a.realm == account.realm
                && a.management.activity.run_id == state.run_id
        })
        .ok_or("WorkBuddy activity run was superseded or removed")?;
    current.management.activity = state;
    write(&accounts)
}
pub(super) fn save_verified_models(
    account: &Account,
    models: &[Model],
    unavailable: &[String],
) -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    let mut accounts = read()?;
    let current = accounts
        .iter_mut()
        .find(|a| a.id == account.id && a.revision == account.revision && a.enabled)
        .ok_or("WorkBuddy account changed during verification; fetch models again")?;
    current.models_verified_at = now();
    current
        .unavailable_models
        .extend(unavailable.iter().cloned());
    for model in models {
        current.unavailable_models.remove(&model.id);
    }
    current.verified_models = models
        .iter()
        .filter(|model| {
            current
                .models
                .iter()
                .any(|candidate| candidate.id == model.id)
        })
        .map(|model| (model.id.clone(), current.models_verified_at))
        .collect();
    write(&accounts)
}

/// Update one explicit probe without refreshing or replacing other evidence.
pub(crate) fn record_model_verification(
    account: &Account,
    model: &str,
    available: bool,
) -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy account store unavailable")?;
    let mut accounts = read()?;
    let current = accounts
        .iter_mut()
        .find(|a| a.id == account.id && a.revision == account.revision && a.enabled)
        .ok_or("WorkBuddy account changed during verification")?;
    if !current.models.iter().any(|candidate| candidate.id == model) {
        return Err("WorkBuddy model changed during verification".into());
    }
    current.models_verified_at = now();
    if available {
        current
            .verified_models
            .insert(model.to_string(), current.models_verified_at);
        current.unavailable_models.remove(model);
    } else {
        current.verified_models.remove(model);
        current.unavailable_models.insert(model.to_string());
    }
    write(&accounts)
}
pub fn parse_import(value: Value) -> Result<Account, String> {
    let identity = value.get("account").unwrap_or(&value);
    let auth = value.get("auth").unwrap_or(&value);
    let text = |v: &Value, keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| v.get(k).and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    let domain = text(auth, &["domain"]);
    let realm = match text(auth, &["realm"]).as_str() {
        "global" => Realm::Global,
        "cn" => Realm::Cn,
        "" if domain == "workbuddy.ai" || domain.ends_with(".workbuddy.ai") => Realm::Global,
        "" => Realm::Cn,
        _ => return Err("WorkBuddy realm must be cn or global".into()),
    };
    let account = Account {
        id: uuid::Uuid::new_v4().to_string(),
        uid: text(identity, &["uid"]),
        nickname: text(identity, &["nickname"]),
        enterprise_id: text(identity, &["enterpriseId", "enterprise_id"]),
        domain,
        realm,
        access_token: text(auth, &["accessToken", "access_token"]),
        refresh_token: text(auth, &["refreshToken", "refresh_token"]),
        device_token: text(&value, &["device_token"]),
        expires_at: auth
            .get("expiresAt")
            .or_else(|| auth.get("expires_at"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        enabled: true,
        models: Vec::new(),
        verified_models: HashMap::new(),
        unavailable_models: HashSet::new(),
        models_verified_at: 0,
        last_error: None,
        revision: uuid::Uuid::new_v4().to_string(),
        management: super::maintenance::Management::default(),
        client: super::identity::ClientIdentity::default(),
    };
    validate(&account)?;
    Ok(account)
}

pub struct Lease {
    pub account: Account,
    key: String,
    finished: std::sync::atomic::AtomicBool,
    breaker: crate::models::CircuitBreakerConfig,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut pool) = RUNTIME.lock() {
            if let Some(entry) = pool.get_mut(&self.key) {
                entry.in_flight = entry.in_flight.saturating_sub(1);
                scheduling::wake();
            }
        }
    }
}
impl Lease {
    pub fn success(&self) {
        if self
            .finished
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        if let Ok(mut pool) = RUNTIME.lock() {
            if let Some(entry) = pool.get_mut(&self.key) {
                entry.health.successes += 1;
                entry.health.consecutive_failures = 0;
                entry.health.last_success_at = now();
                entry.health.last_error = None;
            }
        }
        // Piggyback domestic reward collection on real usage so it is not a lonely cron.
        let id = self.account.id.clone();
        tauri::async_runtime::spawn(async move {
            let Ok(account) = get(&id) else {
                return;
            };
            if super::activity::due_opportunistic(&account, now()) {
                let _ = super::activity::start(&id, super::activity::Action::Automatic);
            }
        });
    }
    pub fn failure(&self, reason: &str, seconds: i64) {
        if self
            .finished
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        if let Ok(mut pool) = RUNTIME.lock() {
            if let Some(entry) = pool.get_mut(&self.key) {
                entry.health.failures += 1;
                entry.health.consecutive_failures += 1;
                entry.health.last_failure_at = now();
                entry.health.last_error = Some(reason.into());
                if self.breaker.enabled {
                    let step = self
                        .breaker
                        .backoff_steps
                        .get(
                            (entry.health.consecutive_failures.saturating_sub(1) as usize)
                                .min(self.breaker.backoff_steps.len().saturating_sub(1)),
                        )
                        .copied()
                        .unwrap_or(60)
                        .min(86400) as i64;
                    entry.cooldown_until = now().saturating_add(seconds.max(step).clamp(1, 86400));
                    entry.cooldown_reason = Some(reason.into());
                }
            }
        }
    }
    #[cfg(test)]
    pub fn cooldown(&self, seconds: i64) {
        if let Ok(mut pool) = RUNTIME.lock() {
            if let Some(entry) = pool.get_mut(&self.key) {
                entry.cooldown_until = now() + seconds.clamp(1, 300);
            }
        }
    }
}
#[cfg(test)]
pub fn acquire(model: &str) -> Result<Lease, String> {
    scheduling::try_acquire(
        model,
        "",
        &scheduling::Policy {
            sticky: crate::proxy::sticky_config::StickySessionConfig {
                mode: crate::proxy::sticky_config::SchedulingMode::PerformanceFirst,
                max_wait_seconds: 0,
            },
            ..Default::default()
        },
    )?
    .ok_or_else(|| "No available WorkBuddy account".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn workbuddy_credentials_encrypted_and_reauthorization_preserves_settings() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let value = json!({"account":{"uid":"u1","nickname":"test"},"auth":{"accessToken":"secret_access","refreshToken":"secret_refresh","expiresAt":now()+3600}});
        let mut account = parse_import(value.clone()).unwrap();
        account.models.push(Model {
            id: "native-model".into(),
            name: "Model".into(),
        });
        let view = upsert(account).unwrap();
        let raw = std::fs::read_to_string(path().unwrap()).unwrap();
        assert!(!raw.contains("secret_access"));
        assert!(!raw.contains("\"uid\""));
        let public = serde_json::to_string(&view).unwrap();
        assert!(!public.contains("secret"));
        assert!(!public.contains("token"));
        assert_eq!(get(&view.id).unwrap().access_token, "secret_access");
        let fingerprint = get(&view.id).unwrap();
        assert!(!fingerprint.device_token.is_empty());
        assert!(!fingerprint.client.machine_id.is_empty());
        set_enabled(&view.id, false).unwrap();
        let mut reauth = parse_import(value).unwrap();
        reauth.device_token = "replacement-device-token".into();
        reauth.client.user_agent = "WorkBuddy/9.9.9".into();
        reauth.client.machine_id = "replacement-machine".into();
        let updated = upsert(reauth).unwrap();
        assert_eq!(updated.id, view.id);
        assert!(!updated.enabled);
        assert_eq!(updated.models.len(), 1);
        assert_eq!(updated.candidate_model_count, 1);
        let kept = get(&view.id).unwrap();
        assert_eq!(kept.device_token, fingerprint.device_token);
        assert_eq!(kept.client.machine_id, fingerprint.client.machine_id);
        assert_ne!(kept.device_token, "replacement-device-token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key = crate::modules::account::get_data_dir()
                .unwrap()
                .join("workbuddy_accounts.key");
            assert_eq!(
                std::fs::metadata(key).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn workbuddy_store_cache_follows_writes_and_external_restores() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let value = json!({"account":{"uid":"u1","nickname":"test"},"auth":{"accessToken":"a","refreshToken":"r","expiresAt":now()+3600}});
        let id = upsert(parse_import(value).unwrap()).unwrap().id;
        assert!(get(&id).unwrap().enabled);
        let backup = std::fs::read(path().unwrap()).unwrap();
        set_enabled(&id, false).unwrap();
        assert!(!get(&id).unwrap().enabled);
        // An in-place restore (same inode) must still be observed.
        std::fs::write(path().unwrap(), &backup).unwrap();
        assert!(get(&id).unwrap().enabled);
        std::fs::remove_file(path().unwrap()).unwrap();
        assert!(get(&id).is_err());
    }
    #[test]
    fn workbuddy_stale_writes_cannot_resurrect_removed_or_reauthorized_accounts() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let value = json!({"uid":"u1","accessToken":"secret","refreshToken":"refresh","expiresAt":now()+3600});
        let view = upsert(parse_import(value.clone()).unwrap()).unwrap();
        let mut stale = get(&view.id).unwrap();
        upsert(parse_import(value).unwrap()).unwrap();
        assert!(update_if_current(&mut stale).is_err());
        let mut fresh = get(&view.id).unwrap();
        let mut observation = fresh.clone();
        observation.models.push(Model {
            id: "new-model".into(),
            name: "New model".into(),
        });
        observation.management.credits = Some(super::super::resources::Credits {
            available: 12.0,
            ..Default::default()
        });
        save_observation(&mut observation).unwrap();
        set_enabled(&view.id, false).unwrap();
        update_if_current(&mut fresh).unwrap();
        let stored = get(&view.id).unwrap();
        assert!(!stored.enabled);
        assert_eq!(stored.models[0].id, "new-model");
        assert_eq!(stored.management.credits.unwrap().available, 12.0);
        remove(&view.id).unwrap();
        assert!(update_if_current(&mut fresh).is_err());
        assert!(list().unwrap().is_empty());
    }
    #[test]
    fn workbuddy_pool_capacity_release_and_disabled_accounts() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let mut a = parse_import(json!({"uid":"a","accessToken":"secret"})).unwrap();
        a.models.push(Model {
            id: "test".into(),
            name: "Test".into(),
        });
        let id = upsert(a).unwrap().id;
        let first = acquire("test").unwrap();
        let second = acquire("test").unwrap();
        assert!(acquire("test").is_err());
        drop(first);
        assert!(acquire("test").is_ok());
        second.cooldown(20);
        drop(second);
        assert!(acquire("test").is_err());
        set_enabled(&id, false).unwrap();
        assert!(acquire("test").is_err());
    }
}
