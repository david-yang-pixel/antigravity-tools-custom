//! Stable per-account client identity and schedule salt.
//! User-Agent / machine / session / device stay fixed for one account and differ across accounts.
use super::accounts::{Account, Realm};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ClientIdentity {
    pub user_agent: String,
    pub product_version: String,
    pub cli_version: String,
    pub machine_id: String,
    pub session_id: String,
}

fn digest(seed: &str) -> [u8; 32] {
    Sha256::digest(seed.as_bytes()).into()
}

pub fn hash_u64(seed: &str) -> u64 {
    u64::from_le_bytes(digest(seed)[..8].try_into().unwrap_or([0; 8]))
}

fn hex_id(seed: &str) -> String {
    digest(seed)[..18]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Same derivation as `ensure`; headers use this before backfill persists the fields.
pub fn machine_id(account_id: &str) -> String {
    hex_id(&format!("wb2a:machine:{account_id}"))
}
pub fn session_id(account_id: &str) -> String {
    hex_id(&format!("wb2a:session:{account_id}"))
}

fn uuid_from(seed: &str) -> String {
    let d = digest(seed);
    // RFC-ish layout from hash bytes; stable across restarts, unique per account id.
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        d[0],
        d[1],
        d[2],
        d[3],
        d[4],
        d[5],
        d[6] & 0x0f,
        d[7],
        (d[8] & 0x3f) | 0x80,
        d[9],
        d[10],
        d[11],
        d[12],
        d[13],
        d[14],
        d[15]
    )
}

fn versions(account_id: &str) -> (String, String) {
    let n = hash_u64(&format!("wb2a:ua:{account_id}"));
    let patch = 1 + (n % 9) as u32; // 5.5.1 .. 5.5.9
    let cli_minor = 120 + ((n >> 8) % 31) as u32; // 2.120.x .. 2.150.x
    let cli_patch = ((n >> 16) % 20) as u32;
    (format!("5.5.{patch}"), format!("2.{cli_minor}.{cli_patch}"))
}

fn user_agent(realm: Realm, product: &str, cli: &str) -> String {
    match realm {
        Realm::Cn => format!("WorkBuddy/{product} WorkBuddy/{product} CLI/{cli}"),
        Realm::Global => format!("WorkBuddy/{product} WorkBuddy AI/{product} CLI/{cli}"),
    }
}

/// Fill missing identity fields. Never rotates an already-persisted version pair / ids.
pub fn ensure(account: &mut Account) -> bool {
    let mut changed = false;
    if account.client.machine_id.is_empty() {
        account.client.machine_id = machine_id(&account.id);
        changed = true;
    }
    if account.client.session_id.is_empty() {
        account.client.session_id = session_id(&account.id);
        changed = true;
    }
    if account.device_token.is_empty() {
        account.device_token = uuid_from(&format!("wb2a:device:{}", account.id));
        changed = true;
    }
    if account.client.product_version.is_empty() || account.client.cli_version.is_empty() {
        let (product, cli) = versions(&account.id);
        if account.client.product_version.is_empty() {
            account.client.product_version = product;
        }
        if account.client.cli_version.is_empty() {
            account.client.cli_version = cli;
        }
        changed = true;
    }
    let expected = user_agent(
        account.realm,
        &account.client.product_version,
        &account.client.cli_version,
    );
    if account.client.user_agent != expected {
        account.client.user_agent = expected;
        changed = true;
    }
    changed
}

/// Beijing-time preferred check-in minute (09:00–20:59), stable per account.
pub fn checkin_minute(account_id: &str) -> u32 {
    9 * 60 + (hash_u64(&format!("wb2a:checkin:{account_id}")) % (12 * 60)) as u32
}

pub fn sync_extra_secs(account_id: &str) -> i64 {
    (hash_u64(&format!("wb2a:sync:{account_id}")) % 180) as i64
}

pub fn checkin_retry_secs(account_id: &str, attempt_at: i64) -> i64 {
    3600 + (hash_u64(&format!("wb2a:checkin-retry:{account_id}:{attempt_at}")) % 1200) as i64
}

/// Background reward poll: about 2–4 hours, stretched when recent runs found nothing.
pub fn activity_interval_secs(account_id: &str, idle_streak: u32) -> i64 {
    let base = 7200 + (hash_u64(&format!("wb2a:activity:{account_id}")) % 7200) as i64;
    let stretch = 1i64 << idle_streak.min(2);
    (base * stretch).min(12 * 3600)
}

pub const ACTIVITY_OPPORTUNITY_SECS: i64 = 45 * 60;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_is_stable_unique_and_schedule_salts_stay_in_range() {
        let mut a = super::super::accounts::parse_import(json!({
            "uid":"u-a","accessToken":"tok","expiresAt":2000000000
        }))
        .unwrap();
        let mut b = super::super::accounts::parse_import(json!({
            "uid":"u-b","accessToken":"tok","expiresAt":2000000000
        }))
        .unwrap();
        assert!(ensure(&mut a));
        assert!(ensure(&mut b));
        assert!(!ensure(&mut a));
        assert_ne!(a.client.user_agent, b.client.user_agent);
        assert_ne!(a.client.machine_id, b.client.machine_id);
        assert_ne!(a.device_token, b.device_token);
        assert_eq!(a.client.machine_id, machine_id(&a.id));
        assert_eq!(a.client.session_id, session_id(&a.id));
        let ua = a.client.user_agent.clone();
        let machine = a.client.machine_id.clone();
        assert!(!ensure(&mut a));
        assert_eq!(a.client.user_agent, ua);
        assert_eq!(a.client.machine_id, machine);
        assert!(a.client.user_agent.contains("WorkBuddy/"));
        assert!(checkin_minute(&a.id) >= 9 * 60 && checkin_minute(&a.id) < 21 * 60);
        assert!(activity_interval_secs(&a.id, 0) >= 7200);
        assert!(activity_interval_secs(&a.id, 2) <= 12 * 3600);
    }
}
