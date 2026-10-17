//! Usage-portal registration and SMTP settings.
//!
//! Stored in `portal_settings.json` in the data directory and editable from the
//! admin "User Tokens" page. Every field can be overridden by an environment
//! variable for headless servers; overridden fields are reported as locked.

use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "portal_settings.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    /// Implicit TLS, usually port 465.
    #[default]
    Tls,
    /// STARTTLS upgrade, usually port 587.
    Starttls,
    /// Plain SMTP; only for a relay on a trusted network.
    None,
}

impl SmtpSecurity {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tls" | "ssl" => Some(Self::Tls),
            "starttls" => Some(Self::Starttls),
            "none" | "plain" => Some(Self::None),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub username: String,
    /// Encrypted at rest; omitted when empty (the admin view always clears it).
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        serialize_with = "crate::utils::crypto::serialize_password",
        deserialize_with = "crate::utils::crypto::deserialize_password"
    )]
    pub password: String,
    pub from_address: String,
    pub from_name: String,
}

impl Default for SmtpSettings {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 465,
            security: SmtpSecurity::Tls,
            username: String::new(),
            password: String::new(),
            from_address: String::new(),
            from_name: String::new(),
        }
    }
}

impl SmtpSettings {
    pub fn is_ready(&self) -> bool {
        !self.host.trim().is_empty() && !self.from_address.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PortalSettings {
    /// Whether visitors can create accounts from the usage portal.
    pub registration_enabled: bool,
    /// Allowed email domains (subdomains included); empty allows every domain.
    pub allowed_email_domains: Vec<String>,
    /// Limits copied onto each newly registered key. Quota always starts at 0.
    pub default_max_concurrency: i32,
    pub default_allowed_models: Vec<String>,
    pub default_expires_type: String,
    pub smtp: SmtpSettings,
}

impl Default for PortalSettings {
    fn default() -> Self {
        Self {
            registration_enabled: false,
            allowed_email_domains: Vec::new(),
            default_max_concurrency: 0,
            default_allowed_models: Vec::new(),
            default_expires_type: "never".into(),
            smtp: SmtpSettings::default(),
        }
    }
}

impl PortalSettings {
    pub fn email_allowed(&self, email: &str) -> bool {
        let Some((_, domain)) = email.rsplit_once('@') else {
            return false;
        };
        self.allowed_email_domains.is_empty()
            || self
                .allowed_email_domains
                .iter()
                .any(|allowed| domain == allowed || domain.ends_with(&format!(".{allowed}")))
    }

    /// Registration needs both the switch and a working mail configuration.
    pub fn registration_open(&self) -> bool {
        self.registration_enabled && self.smtp.is_ready()
    }

    fn normalize(mut self) -> Result<Self, String> {
        self.allowed_email_domains = normalize_domains(self.allowed_email_domains);
        if self.default_max_concurrency < 0 {
            return Err("Default concurrency must be 0 or more".into());
        }
        if !matches!(
            self.default_expires_type.as_str(),
            "day" | "week" | "month" | "never"
        ) {
            return Err("Default expiry must be day, week, month or never".into());
        }
        self.default_allowed_models =
            crate::modules::user_token_db::normalize_allowed_models(self.default_allowed_models)?;
        let smtp = &mut self.smtp;
        smtp.host = smtp.host.trim().to_string();
        smtp.username = smtp.username.trim().to_string();
        smtp.from_name = smtp.from_name.trim().to_string();
        smtp.from_address = smtp.from_address.trim().to_string();
        if smtp.port == 0 {
            return Err("SMTP port must be between 1 and 65535".into());
        }
        if !smtp.from_address.is_empty()
            && crate::modules::portal_account::normalize_email(&smtp.from_address).is_err()
        {
            return Err("Sender address is not a valid email".into());
        }
        Ok(self)
    }
}

fn normalize_domains(domains: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = domains
        .iter()
        .flat_map(|entry| entry.split([',', '\n', ' ']))
        .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What the admin UI sees: the SMTP password is never returned.
#[derive(Debug, Clone, Serialize)]
pub struct PortalSettingsView {
    #[serde(flatten)]
    pub settings: PortalSettings,
    pub smtp_password_set: bool,
    pub smtp_ready: bool,
    /// Setting names currently forced by environment variables.
    pub env_locked: Vec<&'static str>,
}

/// Admin update. `smtp_password`: absent keeps the stored value, "" clears it.
#[derive(Debug, Clone, Deserialize)]
pub struct PortalSettingsUpdate {
    #[serde(flatten)]
    pub settings: PortalSettings,
    #[serde(default)]
    pub smtp_password: Option<String>,
}

fn settings_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join(SETTINGS_FILE))
}

fn load_stored() -> Result<PortalSettings, String> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok(PortalSettings::default());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {SETTINGS_FILE}: {e}"))?;
    if content.trim().is_empty() {
        return Ok(PortalSettings::default());
    }
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse {SETTINGS_FILE}: {e}"))
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn apply_env(settings: &mut PortalSettings) -> Vec<&'static str> {
    let mut locked = Vec::new();
    if let Some(v) = env("ABV_PORTAL_REGISTRATION").and_then(|v| parse_bool(&v)) {
        settings.registration_enabled = v;
        locked.push("registration_enabled");
    }
    if let Some(v) = env("ABV_PORTAL_EMAIL_DOMAINS") {
        settings.allowed_email_domains = normalize_domains(vec![v]);
        locked.push("allowed_email_domains");
    }
    let smtp = &mut settings.smtp;
    if let Some(v) = env("ABV_SMTP_HOST") {
        smtp.host = v;
        locked.push("smtp.host");
    }
    if let Some(v) = env("ABV_SMTP_PORT").and_then(|v| v.parse().ok()) {
        smtp.port = v;
        locked.push("smtp.port");
    }
    if let Some(v) = env("ABV_SMTP_SECURITY").and_then(|v| SmtpSecurity::parse(&v)) {
        smtp.security = v;
        locked.push("smtp.security");
    }
    if let Some(v) = env("ABV_SMTP_USERNAME") {
        smtp.username = v;
        locked.push("smtp.username");
    }
    if let Ok(v) = std::env::var("ABV_SMTP_PASSWORD") {
        if !v.is_empty() {
            smtp.password = v;
            locked.push("smtp.password");
        }
    }
    if let Some(v) = env("ABV_SMTP_FROM") {
        smtp.from_address = v;
        locked.push("smtp.from_address");
    }
    if let Some(v) = env("ABV_SMTP_FROM_NAME") {
        smtp.from_name = v;
        locked.push("smtp.from_name");
    }
    locked
}

/// Effective settings: the stored file with environment overrides applied.
pub fn load() -> Result<PortalSettings, String> {
    let mut settings = load_stored()?;
    apply_env(&mut settings);
    Ok(settings)
}

pub fn view() -> Result<PortalSettingsView, String> {
    let mut settings = load_stored()?;
    let env_locked = apply_env(&mut settings);
    let smtp_password_set = !settings.smtp.password.is_empty();
    let smtp_ready = settings.smtp.is_ready();
    settings.smtp.password.clear();
    Ok(PortalSettingsView {
        settings,
        smtp_password_set,
        smtp_ready,
        env_locked,
    })
}

pub fn save(update: PortalSettingsUpdate) -> Result<PortalSettingsView, String> {
    let stored = load_stored()?;
    let mut next = update.settings.normalize()?;
    next.smtp.password = match update.smtp_password {
        Some(password) => password,
        None => stored.smtp.password,
    };
    let content = serde_json::to_string_pretty(&next)
        .map_err(|e| format!("Failed to serialize portal settings: {e}"))?;
    crate::utils::fs::write_atomic(&settings_path()?, content.as_bytes())
        .map_err(|e| format!("Failed to save portal settings: {e}"))?;
    view()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_settings_domain_matching_and_normalization() {
        let settings = PortalSettings {
            allowed_email_domains: normalize_domains(vec!["@Example.com, corp.io".into()]),
            ..Default::default()
        };
        assert_eq!(
            settings.allowed_email_domains,
            vec!["corp.io", "example.com"]
        );
        assert!(settings.email_allowed("a@example.com"));
        assert!(settings.email_allowed("a@mail.corp.io"));
        assert!(!settings.email_allowed("a@notexample.com"));
        assert!(PortalSettings::default().email_allowed("a@any.org"));
    }

    #[test]
    fn portal_settings_registration_needs_mail() {
        let mut settings = PortalSettings {
            registration_enabled: true,
            ..Default::default()
        };
        assert!(!settings.registration_open());
        settings.smtp.host = "smtp.example.com".into();
        settings.smtp.from_address = "noreply@example.com".into();
        assert!(settings.registration_open());
    }

    #[test]
    fn portal_settings_reject_invalid_values() {
        for bad in [
            PortalSettings {
                default_expires_type: "custom".into(),
                ..Default::default()
            },
            PortalSettings {
                default_max_concurrency: -1,
                ..Default::default()
            },
        ] {
            assert!(bad.normalize().is_err());
        }
        let mut bad_sender = PortalSettings::default();
        bad_sender.smtp.from_address = "not-an-email".into();
        assert!(bad_sender.normalize().is_err());
    }

    #[test]
    fn portal_settings_password_is_encrypted_at_rest_and_hidden() {
        let mut settings = PortalSettings::default();
        settings.smtp.password = "smtp-secret".into();
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("smtp-secret"));
        let parsed: PortalSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.smtp.password, "smtp-secret");
        let empty = serde_json::to_value(PortalSettings::default()).unwrap();
        assert!(empty["smtp"].get("password").is_none());
    }
}
