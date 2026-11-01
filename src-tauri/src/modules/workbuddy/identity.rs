//! Stable per-account client identity and schedule salt.
//! Machine, session and device ids are random values stored with the account.
//! They are not derived from a published salt, and every account uses one pinned
//! product version instead of a hash-spread version range.
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

fn legacy_machine(account_id: &str) -> String {
    hex_id(&format!("wb2a:machine:{account_id}"))
}
fn legacy_session(account_id: &str) -> String {
    hex_id(&format!("wb2a:session:{account_id}"))
}
fn legacy_device(account_id: &str) -> String {
    uuid_from(&format!("wb2a:device:{account_id}"))
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

pub const PRODUCT_VERSION: &str = "5.5.4";
pub const CLI_VERSION: &str = "2.137.1";

pub fn pinned_user_agent(realm: Realm) -> String {
    user_agent(realm, PRODUCT_VERSION, CLI_VERSION)
}

fn user_agent(realm: Realm, product: &str, cli: &str) -> String {
    match realm {
        Realm::Cn => format!("WorkBuddy/{product} WorkBuddy/{product} CLI/{cli}"),
        Realm::Global => format!("WorkBuddy/{product} WorkBuddy AI/{product} CLI/{cli}"),
    }
}

fn random_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Fill missing identity fields. Replaces ids that still match the old published
/// derivation. A random id, once stored, is kept across restarts and reauthorization.
pub fn ensure(account: &mut Account) -> bool {
    let mut changed = false;
    if account.client.machine_id.is_empty()
        || account.client.machine_id == legacy_machine(&account.id)
    {
        account.client.machine_id = random_id();
        changed = true;
    }
    if account.client.session_id.is_empty()
        || account.client.session_id == legacy_session(&account.id)
    {
        account.client.session_id = random_id();
        changed = true;
    }
    if account.device_token.is_empty() || account.device_token == legacy_device(&account.id) {
        account.device_token = uuid::Uuid::new_v4().to_string();
        changed = true;
    }
    if account.client.product_version != PRODUCT_VERSION {
        account.client.product_version = PRODUCT_VERSION.into();
        changed = true;
    }
    if account.client.cli_version != CLI_VERSION {
        account.client.cli_version = CLI_VERSION.into();
        changed = true;
    }
    let expected = pinned_user_agent(account.realm);
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
        assert_eq!(a.client.user_agent, b.client.user_agent);
        assert_eq!(a.client.user_agent, pinned_user_agent(a.realm));
        assert_ne!(a.client.machine_id, b.client.machine_id);
        assert_ne!(a.device_token, b.device_token);
        assert_eq!(a.client.machine_id.len(), 32);
        assert_ne!(a.client.machine_id, legacy_machine(&a.id));
        assert_ne!(a.device_token, legacy_device(&a.id));
        let ua = a.client.user_agent.clone();
        a.client.machine_id = legacy_machine(&a.id);
        a.client.session_id = legacy_session(&a.id);
        a.device_token = legacy_device(&a.id);
        assert!(ensure(&mut a));
        assert_ne!(a.client.machine_id, legacy_machine(&a.id));
        let machine = a.client.machine_id.clone();
        assert!(!ensure(&mut a));
        assert_eq!(a.client.machine_id, machine);
        assert_eq!(a.client.user_agent, ua);
        assert!(checkin_minute(&a.id) >= 9 * 60 && checkin_minute(&a.id) < 21 * 60);
        assert!(activity_interval_secs(&a.id, 0) >= 7200);
        assert!(activity_interval_secs(&a.id, 2) <= 12 * 3600);
    }
}
