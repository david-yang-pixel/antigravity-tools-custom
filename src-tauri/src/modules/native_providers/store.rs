//! One encrypted vault per provider; no Google or WorkBuddy token files are read or changed.
use super::{Account, Provider, Settings, SettingsView};
use std::{path::PathBuf, sync::Mutex};
static LOCK: Mutex<()> = Mutex::new(());
/// Pasted refresh-token fingerprints kept per account (oldest dropped first).
const MAX_IMPORTED_FINGERPRINTS: usize = 32;

fn path(provider: Provider) -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join(format!("{}_accounts.enc", provider.key())))
}
fn read(provider: Provider) -> Result<Vec<Account>, String> {
    let raw = match std::fs::read_to_string(path(provider)?) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(format!("Cannot read {} account vault", provider.name())),
    };
    let decoded = crate::utils::crypto::decrypt_string(&raw).map_err(|_| {
        format!(
            "Cannot decrypt {} account vault on this machine",
            provider.name()
        )
    })?;
    let accounts: Vec<Account> =
        serde_json::from_str(&decoded).map_err(|_| "Invalid native account vault")?;
    if accounts.len() > 500 || accounts.iter().any(|a| a.provider != provider) {
        return Err("Invalid native account vault".into());
    }
    Ok(accounts)
}
fn write(provider: Provider, accounts: &[Account]) -> Result<(), String> {
    let path = path(provider)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| "Cannot create account directory")?;
    }
    let encoded = crate::utils::crypto::encrypt_string(
        &serde_json::to_string(accounts).map_err(|_| "Cannot encode account vault")?,
    )?;
    // Owner-only from creation: a post-rename chmod would leave the encrypted vault
    // readable by other local users while it is written.
    crate::utils::fs::write_atomic_private(&path, encoded.as_bytes())
}
pub fn all(provider: Provider) -> Result<Vec<Account>, String> {
    let _guard = LOCK.lock().map_err(|_| "Account vault unavailable")?;
    read(provider)
}
pub fn get(provider: Provider, id: &str) -> Result<Account, String> {
    all(provider)?
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| "Native account no longer exists".into())
}
pub fn update(
    provider: Provider,
    id: &str,
    revision: Option<&str>,
    change: impl FnOnce(&mut Account),
) -> Result<Account, String> {
    let _guard = LOCK.lock().map_err(|_| "Account vault unavailable")?;
    let mut accounts = read(provider)?;
    let account = accounts
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or("Native account no longer exists")?;
    if revision.is_some_and(|r| r != account.revision) {
        return Err("Account was reauthorized; discard the older operation".into());
    }
    change(account);
    let result = account.clone();
    write(provider, &accounts)?;
    Ok(result)
}
/// Applies `change` to every account in one vault write.
pub fn update_all(provider: Provider, mut change: impl FnMut(&mut Account)) -> Result<(), String> {
    let _guard = LOCK.lock().map_err(|_| "Account vault unavailable")?;
    let mut accounts = read(provider)?;
    if accounts.is_empty() {
        return Ok(());
    }
    accounts.iter_mut().for_each(&mut change);
    write(provider, &accounts)
}
/// Appends import fingerprints not yet recorded, keeping the newest 32.
pub fn add_fingerprints(imported: &mut Vec<String>, digests: impl IntoIterator<Item = String>) {
    for digest in digests {
        if !imported.contains(&digest) {
            imported.push(digest);
        }
    }
    let excess = imported.len().saturating_sub(MAX_IMPORTED_FINGERPRINTS);
    imported.drain(..excess);
}
pub fn upsert(mut account: Account, expected: Option<&str>) -> Result<Account, String> {
    if account.uid.trim().is_empty() || account.credentials.access_token.is_empty() {
        return Err("Authorization returned no usable account identity".into());
    }
    let _guard = LOCK.lock().map_err(|_| "Account vault unavailable")?;
    let mut accounts = read(account.provider)?;
    let position = accounts
        .iter()
        .position(|a| a.realm == account.realm && a.uid == account.uid);
    if let Some(id) = expected {
        let previous = accounts
            .iter()
            .find(|a| a.id == id)
            .ok_or("Account was removed while authorizing")?;
        if previous.uid != account.uid || previous.realm != account.realm {
            return Err("Authorization belongs to a different account".into());
        }
    }
    if let Some(index) = position {
        let previous = &accounts[index];
        account.id = previous.id.clone();
        // A new authorization revives an account its dead credential turned off, but never
        // one the administrator turned off.
        account.enabled = previous.enabled || previous.credential_dead;
        let mut imported = previous.credentials.imported_refresh_sha256.clone();
        add_fingerprints(
            &mut imported,
            std::mem::take(&mut account.credentials.imported_refresh_sha256),
        );
        account.credentials.imported_refresh_sha256 = imported;
        account.auto_refresh = previous.auto_refresh;
        account.quota = previous.quota.clone();
        account.models = previous.models.clone();
        account.verified_models = previous.verified_models.clone();
        account.models_updated_at = previous.models_updated_at;
        // Preserve the identity used to obtain these new credentials, never the previous refresh token.
        accounts[index] = account.clone();
    } else {
        if accounts.len() >= 500 {
            return Err("At most 500 accounts per provider".into());
        }
        accounts.push(account.clone());
    }
    write(account.provider, &accounts)?;
    Ok(account)
}
pub fn remove(provider: Provider, id: &str) -> Result<(), String> {
    let _guard = LOCK.lock().map_err(|_| "Account vault unavailable")?;
    let mut accounts = read(provider)?;
    accounts.retain(|a| a.id != id);
    write(provider, &accounts)
}
fn settings_path(provider: Provider) -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join(format!("{}_settings.json", provider.key())))
}
pub fn settings(provider: Provider) -> Result<Settings, String> {
    let mut settings: Settings = match std::fs::read(settings_path(provider)?) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| "Invalid native provider settings")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(_) => return Err("Cannot read native provider settings".into()),
    };
    if let Ok(value) = std::env::var(provider.env("ENABLED")) {
        settings.enabled = match value.as_str() {
            "1" | "true" => true,
            "0" | "false" => false,
            _ => return Err("Native provider ENABLED must be true or false".into()),
        };
    }
    Ok(settings)
}
pub fn settings_view(provider: Provider) -> Result<SettingsView, String> {
    let s = settings(provider)?;
    Ok(SettingsView {
        provider,
        enabled: s.enabled,
        preferred_account_id: s.preferred_account_id,
        env_locked: std::env::var_os(provider.env("ENABLED")).is_some(),
    })
}
pub fn save_settings(provider: Provider, settings: Settings) -> Result<SettingsView, String> {
    if std::env::var_os(provider.env("ENABLED")).is_some() {
        return Err("Provider settings are managed by environment variables".into());
    }
    if let Some(id) = settings.preferred_account_id.as_deref() {
        get(provider, id)?;
    }
    let bytes =
        serde_json::to_vec_pretty(&settings).map_err(|_| "Cannot encode provider settings")?;
    crate::utils::fs::write_atomic(settings_path(provider)?, &bytes)?;
    super::accounts::scheduling::clear_provider_sessions(provider);
    settings_view(provider)
}
