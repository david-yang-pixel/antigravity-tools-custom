//! Usage-portal registration, SMTP and top-up payment settings.
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

/// Payment methods an EPay-compatible gateway accepts.
pub const PAY_TYPES: [&str; 2] = ["alipay", "wxpay"];
const MAX_TOPUP_CENTS: i64 = 10_000_000;

/// Balance top-ups through an EPay (易支付) compatible gateway such as ZPay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PaymentSettings {
    /// Shows the top-up button in the usage portal.
    pub enabled: bool,
    /// Gateway base URL, e.g. ZPay's `https://zpayz.cn`.
    pub gateway: String,
    /// Merchant ID (PID).
    pub pid: String,
    /// Merchant key; encrypted at rest and never returned to the admin view.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        serialize_with = "crate::utils::crypto::serialize_password",
        deserialize_with = "crate::utils::crypto::deserialize_password"
    )]
    pub key: String,
    /// Offered payment methods, a subset of [`PAY_TYPES`].
    pub pay_types: Vec<String>,
    /// Public address of this site; the gateway calls back and returns users here.
    pub public_url: String,
    pub min_amount_cents: i64,
    pub max_amount_cents: i64,
    /// Product name shown on the payment; it must describe what is sold.
    pub subject: String,
}

impl Default for PaymentSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            gateway: "https://zpayz.cn".into(),
            pid: String::new(),
            key: String::new(),
            pay_types: vec!["alipay".into()],
            public_url: String::new(),
            min_amount_cents: 500,
            max_amount_cents: 100_000,
            subject: "API 调用额度充值".into(),
        }
    }
}

fn valid_limits(min_cents: i64, max_cents: i64) -> bool {
    min_cents >= 1 && min_cents <= max_cents && max_cents <= MAX_TOPUP_CENTS
}

/// `https://host[:port][/path]` without query, fragment or trailing slash.
/// The gateway must use HTTPS because order queries carry the merchant key.
fn clean_url(value: &str, require_https: bool) -> Option<String> {
    let value = value.trim().trim_end_matches('/');
    let url = url::Url::parse(value).ok()?;
    let secure = url.scheme() == "https"
        || (!require_https && url.scheme() == "http")
        || (url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost")));
    (secure && url.host_str().is_some() && url.query().is_none() && url.fragment().is_none())
        .then(|| value.to_string())
}

impl PaymentSettings {
    /// Gateway credentials, when complete. Pending orders are reconciled with
    /// these even while new top-ups are switched off. A gateway that is not
    /// HTTPS (e.g. a hand-edited settings file) counts as unconfigured, because
    /// order queries carry the merchant key.
    pub fn merchant(&self) -> Option<crate::modules::epay::Merchant> {
        let secure_gateway = clean_url(&self.gateway, true).is_some();
        (secure_gateway && !self.pid.is_empty() && !self.key.is_empty()).then(|| {
            crate::modules::epay::Merchant {
                gateway: self.gateway.clone(),
                pid: self.pid.clone(),
                key: self.key.clone(),
            }
        })
    }

    /// Top-ups need the switch, credentials, a callback address and a method.
    pub fn is_ready(&self) -> bool {
        self.enabled
            && self.merchant().is_some()
            && !self.public_url.is_empty()
            && !self.pay_types.is_empty()
    }

    pub fn notify_url(&self) -> String {
        format!("{}/api/pay/epay/notify", self.public_url)
    }

    /// The gateway appends its own query string, so this must not carry one.
    pub fn return_url(&self) -> String {
        format!("{}/usage", self.public_url)
    }

    fn normalize(&mut self) -> Result<(), String> {
        self.gateway = if self.gateway.trim().is_empty() {
            String::new()
        } else {
            clean_url(&self.gateway, true)
                .ok_or("Payment gateway must be an HTTPS address without a query string")?
        };
        self.public_url = if self.public_url.trim().is_empty() {
            String::new()
        } else {
            clean_url(&self.public_url, false)
                .ok_or("Site address must be an http(s) address without a query string")?
        };
        self.pid = self.pid.trim().to_string();
        if !self.pid.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err("Merchant ID may only contain letters and digits".into());
        }
        let mut types = Vec::new();
        for kind in &self.pay_types {
            let kind = kind.trim().to_ascii_lowercase();
            if !PAY_TYPES.contains(&kind.as_str()) {
                return Err(format!("Unsupported payment method: {kind}"));
            }
            if !types.contains(&kind) {
                types.push(kind);
            }
        }
        self.pay_types = types;
        if !valid_limits(self.min_amount_cents, self.max_amount_cents) {
            return Err("Top-up limits must satisfy 0.01 <= minimum <= maximum <= 100000".into());
        }
        self.subject = self.subject.trim().to_string();
        // `&` and `=` would make the signed string ambiguous.
        if self.subject.is_empty()
            || self.subject.chars().count() > 64
            || self.subject.contains(['&', '='])
        {
            return Err("Product name must be 1-64 characters without & or =".into());
        }
        Ok(())
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
    pub payment: PaymentSettings,
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
            payment: PaymentSettings::default(),
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
        self.payment.normalize()?;
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

/// What the admin UI sees: the SMTP password and merchant key are never returned.
#[derive(Debug, Clone, Serialize)]
pub struct PortalSettingsView {
    #[serde(flatten)]
    pub settings: PortalSettings,
    pub smtp_password_set: bool,
    pub smtp_ready: bool,
    pub payment_key_set: bool,
    pub payment_ready: bool,
    /// Setting names currently forced by environment variables.
    pub env_locked: Vec<&'static str>,
}

/// Admin update. `smtp_password` / `payment_key`: absent keeps the stored value,
/// "" clears it.
#[derive(Debug, Clone, Deserialize)]
pub struct PortalSettingsUpdate {
    #[serde(flatten)]
    pub settings: PortalSettings,
    #[serde(default)]
    pub smtp_password: Option<String>,
    #[serde(default)]
    pub payment_key: Option<String>,
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
    let payment = &mut settings.payment;
    if let Some(v) = env("ABV_ZPAY_ENABLED").and_then(|v| parse_bool(&v)) {
        payment.enabled = v;
        locked.push("payment.enabled");
    }
    if let Some(v) = env("ABV_ZPAY_GATEWAY").and_then(|v| clean_url(&v, true)) {
        payment.gateway = v;
        locked.push("payment.gateway");
    }
    if let Some(v) = env("ABV_ZPAY_PID").filter(|v| v.bytes().all(|b| b.is_ascii_alphanumeric())) {
        payment.pid = v;
        locked.push("payment.pid");
    }
    if let Some(v) = env("ABV_ZPAY_KEY") {
        payment.key = v;
        locked.push("payment.key");
    }
    if let Some(v) = env("ABV_ZPAY_PAY_TYPES") {
        payment.pay_types = v
            .split([',', ' '])
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| PAY_TYPES.contains(&t.as_str()))
            .collect();
        locked.push("payment.pay_types");
    }
    if let Some(v) = env("ABV_ZPAY_PUBLIC_URL").and_then(|v| clean_url(&v, false)) {
        payment.public_url = v;
        locked.push("payment.public_url");
    }
    // Limits are checked together, as normalize() does; an invalid pair is
    // ignored so a bad variable cannot lock the admin form into failing saves.
    let env_min = env("ABV_ZPAY_MIN_AMOUNT").and_then(|v| crate::modules::epay::parse_money(&v));
    let env_max = env("ABV_ZPAY_MAX_AMOUNT").and_then(|v| crate::modules::epay::parse_money(&v));
    if env_min.is_some() || env_max.is_some() {
        let min = env_min.unwrap_or(payment.min_amount_cents);
        let max = env_max.unwrap_or(payment.max_amount_cents);
        if valid_limits(min, max) {
            payment.min_amount_cents = min;
            payment.max_amount_cents = max;
            if env_min.is_some() {
                locked.push("payment.min_amount_cents");
            }
            if env_max.is_some() {
                locked.push("payment.max_amount_cents");
            }
        } else {
            tracing::warn!("Ignoring ABV_ZPAY_MIN_AMOUNT/ABV_ZPAY_MAX_AMOUNT: need 0.01 <= min <= max <= 100000");
        }
    }
    if let Some(v) = env("ABV_ZPAY_SUBJECT").filter(|v| !v.contains(['&', '='])) {
        payment.subject = v;
        locked.push("payment.subject");
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
    let payment_key_set = !settings.payment.key.is_empty();
    let payment_ready = settings.payment.is_ready();
    settings.smtp.password.clear();
    settings.payment.key.clear();
    Ok(PortalSettingsView {
        settings,
        smtp_password_set,
        smtp_ready,
        payment_key_set,
        payment_ready,
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
    next.payment.key = match update.payment_key {
        Some(key) => key.trim().to_string(),
        None => stored.payment.key,
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

    #[test]
    fn portal_settings_payment_key_is_encrypted_and_readiness_needs_everything() {
        let mut settings = PortalSettings::default();
        settings.payment.key = "merchant-secret".into();
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("merchant-secret"));
        let parsed: PortalSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.payment.key, "merchant-secret");
        // Files written before payments existed still load with safe defaults.
        let legacy: PortalSettings =
            serde_json::from_str(r#"{"registration_enabled":true}"#).unwrap();
        assert!(!legacy.payment.enabled && legacy.payment.min_amount_cents == 500);

        let mut payment = PaymentSettings {
            enabled: true,
            pid: "2026040101252972".into(),
            key: "k".into(),
            ..Default::default()
        };
        assert!(!payment.is_ready(), "needs the public site address");
        payment.public_url = "https://example.com".into();
        assert!(payment.is_ready());
        assert_eq!(
            payment.notify_url(),
            "https://example.com/api/pay/epay/notify"
        );
        assert_eq!(payment.return_url(), "https://example.com/usage");
        payment.pay_types.clear();
        assert!(!payment.is_ready());
        // A hand-edited plain-HTTP gateway never receives the key.
        payment.pay_types = vec!["alipay".into()];
        payment.gateway = "http://zpayz.cn".into();
        assert!(payment.merchant().is_none() && !payment.is_ready());
        payment.gateway = "http://127.0.0.1:18999".into();
        assert!(
            payment.merchant().is_some(),
            "loopback stays usable for local testing"
        );
    }

    #[test]
    fn portal_settings_payment_normalization() {
        let mut payment = PaymentSettings {
            gateway: " https://zpayz.cn/ ".into(),
            public_url: "https://antigravity.example.com/".into(),
            pay_types: vec!["Alipay".into(), "alipay".into(), "wxpay".into()],
            subject: "  API 额度充值 ".into(),
            ..Default::default()
        };
        payment.normalize().unwrap();
        assert_eq!(payment.gateway, "https://zpayz.cn");
        assert_eq!(payment.public_url, "https://antigravity.example.com");
        assert_eq!(payment.pay_types, vec!["alipay", "wxpay"]);
        assert_eq!(payment.subject, "API 额度充值");
        for bad in [
            PaymentSettings {
                gateway: "http://zpayz.cn".into(),
                ..Default::default()
            },
            PaymentSettings {
                public_url: "https://x.com/usage?a=1".into(),
                ..Default::default()
            },
            PaymentSettings {
                pid: "12&34".into(),
                ..Default::default()
            },
            PaymentSettings {
                pay_types: vec!["qqpay".into()],
                ..Default::default()
            },
            PaymentSettings {
                min_amount_cents: 0,
                ..Default::default()
            },
            PaymentSettings {
                min_amount_cents: 600,
                max_amount_cents: 500,
                ..Default::default()
            },
            PaymentSettings {
                subject: "a=b".into(),
                ..Default::default()
            },
        ] {
            assert!(bad.clone().normalize().is_err(), "{bad:?}");
        }
    }
}
