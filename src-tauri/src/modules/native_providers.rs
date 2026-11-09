//! Native provider accounts (currently ChatGPT). Only the provider module knows its wire
//! protocol; authorization, encrypted persistence, refresh, scheduling and account views
//! are shared.
#[path = "native_providers/accounts.rs"]
pub mod accounts;
#[path = "native_providers/chatgpt.rs"]
pub mod chatgpt;
#[path = "native_providers/http.rs"]
pub mod http;
#[path = "native_providers/login.rs"]
pub mod login;
#[path = "native_providers/login_imports.rs"]
pub mod login_imports;
#[path = "native_providers/reauthorize.rs"]
pub mod reauthorize;
#[path = "native_providers/store.rs"]
pub mod store;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_ACCOUNT_CONCURRENCY: usize = 8;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    ChatGpt,
}

impl Provider {
    pub const ALL: [Self; 1] = [Self::ChatGpt];
    pub fn key(self) -> &'static str {
        match self {
            Self::ChatGpt => "chatgpt",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::ChatGpt => "ChatGPT",
        }
    }
    pub fn source(self) -> &'static str {
        match self {
            Self::ChatGpt => "chatgpt:default",
        }
    }
    pub fn from_source(source: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.source() == source)
    }
    pub fn env(self, suffix: &str) -> String {
        format!("ABV_{}_{}", self.key().to_uppercase(), suffix)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
    /// Official metadata marks this model as free of the displayed account credits.
    #[serde(default)]
    pub is_free: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct QuotaPack {
    pub name: String,
    /// None means unlimited only when `unlimited` is true; otherwise it means unknown.
    pub total: Option<f64>,
    pub remaining: Option<f64>,
    pub used: Option<f64>,
    pub unlimited: bool,
    pub expires_at: Option<i64>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Quota {
    pub unit: String,
    pub total: Option<f64>,
    pub remaining: Option<f64>,
    pub used: Option<f64>,
    pub unlimited: bool,
    pub updated_at: i64,
    pub packs: Vec<QuotaPack>,
}
impl Quota {
    #[cfg(test)]
    pub fn from_packs(unit: &str, packs: Vec<QuotaPack>) -> Self {
        let unlimited = packs.iter().any(|p| p.unlimited);
        let sum = |f: fn(&QuotaPack) -> Option<f64>| -> Option<f64> {
            packs
                .iter()
                .map(f)
                .try_fold(0.0, |sum, n| n.map(|n| sum + n))
        };
        Self {
            unit: unit.into(),
            total: if unlimited { None } else { sum(|p| p.total) },
            remaining: if unlimited {
                None
            } else {
                sum(|p| p.remaining)
            },
            used: sum(|p| p.used),
            unlimited,
            packs,
            updated_at: now(),
        }
    }
    pub fn exhausted(&self) -> bool {
        if self.unlimited || !self.remaining.is_some_and(|n| n <= 0.0) {
            return false;
        }
        // Percent windows reset at a known time (`expires_at`): a full window stays
        // exhausted until then, however old the snapshot. Balance quotas have no such
        // reset, so their snapshot is trusted only while fresh.
        let at = now();
        if self.unit == "%"
            && self.packs.iter().any(|p| {
                p.remaining.is_some_and(|n| n <= 0.0) && p.expires_at.is_some_and(|r| r > at)
            })
        {
            return true;
        }
        at - self.updated_at < 900
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Subscription plan (ChatGPT `plan_type`).
    #[serde(default)]
    pub user_type: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub chatgpt_account_id: String,
    /// SHA-256 (hex) of every refresh token pasted into an import for this account, so a
    /// token that was rotated away cannot be imported (and replayed) again.
    #[serde(default)]
    pub imported_refresh_sha256: Vec<String>,
}

/// This value is persisted only inside an authenticated, encrypted vault. Never serialize
/// an Account into an API response; `AccountView` is the credential-free public projection.
#[derive(Clone, Deserialize, Serialize)]
pub struct Account {
    pub id: String,
    pub provider: Provider,
    pub realm: Realm,
    pub uid: String,
    pub nickname: String,
    pub enabled: bool,
    pub auto_refresh: bool,
    pub expires_at: i64,
    pub credentials: Credentials,
    #[serde(default)]
    pub login_credentials: Option<reauthorize::LoginCredentials>,
    #[serde(default)]
    pub reauthorization: reauthorize::State,
    pub models: Vec<Model>,
    pub verified_models: BTreeMap<String, i64>,
    pub models_updated_at: i64,
    pub quota: Option<Quota>,
    pub quota_error: Option<String>,
    pub models_error: Option<String>,
    pub auth_error: Option<String>,
    /// The provider declared the credential dead and the account was turned off for it;
    /// a new authorization turns it back on.
    #[serde(default)]
    pub credential_dead: bool,
    pub last_sync_at: i64,
    pub next_sync_at: i64,
    /// New authorizations replace this value, invalidating an older refresh in flight.
    pub revision: String,
}
impl Account {
    pub fn new(provider: Provider, realm: Realm) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            provider,
            realm,
            uid: String::new(),
            nickname: String::new(),
            enabled: true,
            auto_refresh: true,
            expires_at: 0,
            credentials: Credentials::default(),
            login_credentials: None,
            reauthorization: reauthorize::State::default(),
            models: Vec::new(),
            verified_models: BTreeMap::new(),
            models_updated_at: 0,
            quota: None,
            quota_error: None,
            models_error: None,
            auth_error: None,
            credential_dead: false,
            last_sync_at: 0,
            next_sync_at: 0,
            revision: uuid::Uuid::new_v4().to_string(),
        }
    }
}

#[derive(Clone, Serialize)]
pub struct AccountView {
    pub id: String,
    pub provider: Provider,
    pub realm: Realm,
    pub uid: String,
    pub nickname: String,
    pub enabled: bool,
    pub auto_refresh: bool,
    pub expires_at: i64,
    pub can_refresh: bool,
    pub has_login_credentials: bool,
    pub reauthorization_service: Option<reauthorize::Service>,
    pub auto_reauthorize: bool,
    pub reauthorizing: bool,
    pub reauthorization_error: Option<String>,
    pub authorization_progress: Option<reauthorize::protocol::View>,
    pub credential_dead: bool,
    pub models: Vec<Model>,
    pub models_updated_at: i64,
    pub quota: Option<Quota>,
    pub quota_error: Option<String>,
    pub models_error: Option<String>,
    pub auth_error: Option<String>,
    pub last_sync_at: i64,
    pub next_sync_at: i64,
    pub in_flight: usize,
    pub cooldown_until: i64,
    pub last_error: Option<String>,
    pub successes: u64,
    pub failures: u64,
}
impl From<&Account> for AccountView {
    fn from(a: &Account) -> Self {
        let runtime = accounts::runtime(&a.id);
        Self {
            id: a.id.clone(),
            provider: a.provider,
            realm: a.realm,
            uid: a.uid.clone(),
            nickname: a.nickname.clone(),
            enabled: a.enabled,
            auto_refresh: a.auto_refresh,
            expires_at: a.expires_at,
            can_refresh: !a.credentials.refresh_token.is_empty(),
            has_login_credentials: a.login_credentials.is_some(),
            // Every pre-selection account came from the original merchant, including
            // token-only imports that do not yet have password/2FA material bound.
            reauthorization_service: Some(reauthorize::service_for(a).unwrap_or_default().service),
            auto_reauthorize: a.reauthorization.auto,
            reauthorizing: a.reauthorization.requested
                || a.reauthorization.batch.is_some()
                || (a.reauthorization.protocol.started_at > 0
                    && a.reauthorization.protocol.finished_at == 0),
            reauthorization_error: a.reauthorization.error.clone(),
            authorization_progress: a.reauthorization.protocol.view(),
            credential_dead: a.credential_dead,
            models: a.models.clone(),
            models_updated_at: a.models_updated_at,
            quota: a.quota.clone(),
            quota_error: a.quota_error.clone(),
            models_error: a.models_error.clone(),
            auth_error: a.auth_error.clone(),
            last_sync_at: a.last_sync_at,
            // Healthy idle accounts pause polls; zero-balance recovery stays scheduled.
            next_sync_at: if accounts::sync_paused(a, now()) {
                0
            } else {
                a.next_sync_at
            },
            in_flight: runtime.in_flight,
            cooldown_until: runtime.cooldown_until,
            last_error: runtime.last_error,
            successes: runtime.successes,
            failures: runtime.failures,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Settings {
    pub enabled: bool,
    pub preferred_account_id: Option<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            preferred_account_id: None,
        }
    }
}
#[derive(Serialize)]
pub struct SettingsView {
    pub provider: Provider,
    pub enabled: bool,
    pub preferred_account_id: Option<String>,
    pub env_locked: bool,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value.get(*key).and_then(|v| match v {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
        })
        .unwrap_or_default()
}
pub fn number(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
            .filter(|n| n.is_finite())
    })
}
pub fn epoch(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64().or_else(|| value.as_str()?.parse().ok()) {
        return Some(if n > 10_000_000_000 { n / 1000 } else { n });
    }
    value
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp())
}

#[cfg(test)]
#[path = "native_providers/tests.rs"]
pub(crate) mod tests;
