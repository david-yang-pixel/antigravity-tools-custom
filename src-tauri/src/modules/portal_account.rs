//! Usage-portal accounts: email registration, password login and recovery.
//!
//! Each account owns exactly one user token (`user_tokens.owner_user_id`). Quota,
//! models and limits stay on that token, so request enforcement and usage
//! accounting are shared with administrator keys. Accounts live in the same
//! SQLite file as the tokens so registration is a single transaction.

use crate::modules::user_token_db::{self as tokens, NewToken, UserToken};
use base64::Engine;
use rand::{rngs::OsRng, Rng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const CODE_TTL_SECS: i64 = 10 * 60;
pub const CODE_RESEND_SECS: i64 = 60;
const CODES_PER_HOUR: i64 = 5;
const CODE_MAX_ATTEMPTS: i64 = 5;
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;
const MAX_FAILED_LOGINS: i64 = 5;
const LOGIN_LOCK_SECS: i64 = 15 * 60;
const PASSWORD_MIN_CHARS: usize = 8;
const PASSWORD_MAX_CHARS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodePurpose {
    Register,
    Reset,
}

impl CodePurpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::Reset => "reset",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "register" => Some(Self::Register),
            "reset" => Some(Self::Reset),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PortalError {
    InvalidEmail,
    WeakPassword,
    InvalidCode,
    CodeCooldown(i64),
    CodeLimit,
    AlreadyRegistered,
    InvalidCredentials,
    Locked(i64),
    AccountDisabled,
    Unavailable(String),
}

impl PortalError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidEmail => "invalid_email",
            Self::WeakPassword => "weak_password",
            Self::InvalidCode => "invalid_code",
            Self::CodeCooldown(_) => "code_cooldown",
            Self::CodeLimit => "code_limit",
            Self::AlreadyRegistered => "already_registered",
            Self::InvalidCredentials => "invalid_credentials",
            Self::Locked(_) => "account_locked",
            Self::AccountDisabled => "account_disabled",
            Self::Unavailable(_) => "unavailable",
        }
    }

    /// Seconds the caller should wait, for cooldown and lockout errors.
    pub fn retry_after(&self) -> Option<i64> {
        match self {
            Self::CodeCooldown(secs) | Self::Locked(secs) => Some(*secs),
            _ => None,
        }
    }
}

impl From<String> for PortalError {
    fn from(error: String) -> Self {
        Self::Unavailable(error)
    }
}

fn db_err(error: rusqlite::Error) -> PortalError {
    PortalError::Unavailable(error.to_string())
}

#[derive(Debug, Clone, Serialize)]
pub struct PortalUser {
    pub id: String,
    pub email: String,
    pub status: String,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
}

impl PortalUser {
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

/// Defaults applied to the key created at registration. Quota is always 0: the
/// administrator activates the account by granting one.
pub struct RegistrationDefaults {
    pub max_concurrency: i32,
    pub allowed_models: Vec<String>,
    pub expires_type: String,
}

/// Called inside the user-token migration transaction.
pub(crate) fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS portal_users (
            id TEXT PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'disabled')),
            failed_logins INTEGER NOT NULL DEFAULT 0,
            locked_until INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            last_login_at INTEGER
        );
        CREATE TABLE IF NOT EXISTS portal_email_codes (
            id TEXT PRIMARY KEY,
            email TEXT NOT NULL,
            purpose TEXT NOT NULL,
            code_hash TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            consumed_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_portal_email_codes_email
            ON portal_email_codes(email, created_at DESC);
        CREATE TABLE IF NOT EXISTS portal_sessions (
            id_hash TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            ip TEXT,
            user_agent TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_portal_sessions_user ON portal_sessions(user_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_user_tokens_owner
            ON user_tokens(owner_user_id) WHERE owner_user_id IS NOT NULL;",
    )
    .map_err(|e| format!("Failed to migrate portal accounts: {e}"))
}

pub fn normalize_email(raw: &str) -> Result<String, PortalError> {
    let email = raw.trim().to_lowercase();
    let valid = email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && match email.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && local.len() <= 64
                    && !domain.contains('@')
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
                    && !domain.contains("..")
            }
            None => false,
        };
    valid.then_some(email).ok_or(PortalError::InvalidEmail)
}

pub fn validate_password(password: &str) -> Result<(), PortalError> {
    let chars = password.chars().count();
    if (PASSWORD_MIN_CHARS..=PASSWORD_MAX_CHARS).contains(&chars) && !password.trim().is_empty() {
        Ok(())
    } else {
        Err(PortalError::WeakPassword)
    }
}

fn hash_password(password: &str) -> Result<String, PortalError> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| PortalError::Unavailable(format!("password hashing failed: {e}")))
}

fn verify_password(hash: &str, password: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    PasswordHash::new(hash)
        .map(|parsed| {
            argon2::Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// Spends the same work as a real check so unknown emails are not distinguishable
/// by response time.
fn burn_password_check(password: &str) {
    static DUMMY: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| hash_password("dummy-password").unwrap_or_default());
    let _ = verify_password(&DUMMY, password);
}

fn sha256_hex(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

fn code_hash(email: &str, purpose: CodePurpose, code: &str) -> String {
    sha256_hex(&[email, purpose.as_str(), code])
}

fn new_code() -> String {
    format!("{:06}", OsRng.gen_range(0..1_000_000))
}

fn new_session_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn find_user(conn: &Connection, email: &str) -> Result<Option<(PortalUser, String)>, PortalError> {
    conn.query_row(
        "SELECT id, email, status, created_at, last_login_at, password_hash
         FROM portal_users WHERE email = ?1",
        params![email],
        |row| {
            Ok((
                PortalUser {
                    id: row.get(0)?,
                    email: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                    last_login_at: row.get(4)?,
                },
                row.get::<_, String>(5)?,
            ))
        },
    )
    .optional()
    .map_err(db_err)
}

/// What the caller should email after a code request. Every request is recorded
/// and rate limited the same way whether or not the email is registered, so the
/// response never reveals which addresses have accounts.
#[derive(Debug, PartialEq, Eq)]
pub enum CodeDelivery {
    /// Send this verification code.
    Code { id: String, code: String },
    /// Registration for an address that already has an account: tell its owner.
    AlreadyRegistered,
    /// Nothing to send (reset for an unknown or disabled address).
    Nothing,
}

pub fn request_code(
    email: &str,
    purpose: CodePurpose,
    now: i64,
) -> Result<CodeDelivery, PortalError> {
    let conn = tokens::connect_db()?;
    conn.execute(
        "DELETE FROM portal_email_codes WHERE created_at < ?1",
        params![now - 24 * 3600],
    )
    .map_err(db_err)?;
    let (latest, hourly): (Option<i64>, i64) = conn
        .query_row(
            "SELECT MAX(created_at), COUNT(*) FROM portal_email_codes
             WHERE email = ?1 AND created_at > ?2",
            params![email, now - 3600],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(db_err)?;
    if let Some(latest) = latest.filter(|at| now - at < CODE_RESEND_SECS) {
        return Err(PortalError::CodeCooldown(CODE_RESEND_SECS - (now - latest)));
    }
    if hourly >= CODES_PER_HOUR {
        return Err(PortalError::CodeLimit);
    }

    let user = find_user(&conn, email)?.map(|(user, _)| user);
    let id = uuid::Uuid::new_v4().to_string();
    let code = new_code();
    conn.execute(
        "INSERT INTO portal_email_codes (id, email, purpose, code_hash, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            email,
            purpose.as_str(),
            code_hash(email, purpose, &code),
            now,
            now + CODE_TTL_SECS
        ],
    )
    .map_err(db_err)?;

    Ok(match (purpose, user) {
        (CodePurpose::Register, None) => CodeDelivery::Code { id, code },
        (CodePurpose::Register, Some(_)) => CodeDelivery::AlreadyRegistered,
        (CodePurpose::Reset, Some(user)) if user.is_active() => CodeDelivery::Code { id, code },
        (CodePurpose::Reset, _) => CodeDelivery::Nothing,
    })
}

/// Lets the address request again right away when the email could not be sent.
pub fn discard_code(id: &str) -> Result<(), PortalError> {
    tokens::connect_db()?
        .execute("DELETE FROM portal_email_codes WHERE id = ?1", params![id])
        .map_err(db_err)?;
    Ok(())
}

/// Checks the newest unused code. Failed attempts are committed immediately so a
/// later failure in the caller cannot roll back the attempt counter.
fn consume_code(
    conn: &Connection,
    email: &str,
    purpose: CodePurpose,
    code: &str,
    now: i64,
) -> Result<(), PortalError> {
    let row: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT id, code_hash, attempts FROM portal_email_codes
             WHERE email = ?1 AND purpose = ?2 AND consumed_at IS NULL AND expires_at > ?3
             ORDER BY created_at DESC LIMIT 1",
            params![email, purpose.as_str(), now],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(db_err)?;
    let Some((id, expected, attempts)) = row else {
        return Err(PortalError::InvalidCode);
    };
    let code = code.trim();
    if attempts >= CODE_MAX_ATTEMPTS
        || code.len() != 6
        || code_hash(email, purpose, code) != expected
    {
        conn.execute(
            "UPDATE portal_email_codes SET attempts = attempts + 1 WHERE id = ?1",
            params![id],
        )
        .map_err(db_err)?;
        return Err(PortalError::InvalidCode);
    }
    conn.execute(
        "UPDATE portal_email_codes SET consumed_at = ?1 WHERE id = ?2",
        params![now, id],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Creates the account and its single key (disabled, quota 0) in one transaction.
pub fn register(
    email: &str,
    code: &str,
    password: &str,
    defaults: RegistrationDefaults,
    now: i64,
) -> Result<(PortalUser, UserToken), PortalError> {
    validate_password(password)?;
    let mut conn = tokens::connect_db()?;
    // The code is checked first so this endpoint cannot be used to probe which
    // addresses are registered (registered addresses are never sent a code).
    consume_code(&conn, email, CodePurpose::Register, code, now)?;
    let password_hash = hash_password(password)?;

    let tx = conn.transaction().map_err(db_err)?;
    let user = PortalUser {
        id: uuid::Uuid::new_v4().to_string(),
        email: email.to_string(),
        status: "active".into(),
        created_at: now,
        last_login_at: None,
    };
    let inserted = tx.execute(
        "INSERT INTO portal_users (id, email, password_hash, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'active', ?4, ?4)",
        params![user.id, user.email, password_hash, now],
    );
    match inserted {
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            return Err(PortalError::AlreadyRegistered)
        }
        other => {
            other.map_err(db_err)?;
        }
    }
    let username: String = email
        .split('@')
        .next()
        .unwrap_or(email)
        .chars()
        .take(64)
        .collect();
    let token = tokens::insert_token(
        &tx,
        NewToken {
            username,
            description: Some(email.to_string()),
            expires_type: defaults.expires_type,
            custom_expires_at: None,
            max_ips: 0,
            curfew_start: None,
            curfew_end: None,
            max_concurrency: defaults.max_concurrency,
            token_quota: 0,
            allowed_models: defaults.allowed_models,
            owner_user_id: Some(user.id.clone()),
            enabled: false,
        },
    )?;
    tx.commit().map_err(db_err)?;
    Ok((user, token))
}

pub fn login(email: &str, password: &str, now: i64) -> Result<PortalUser, PortalError> {
    let conn = tokens::connect_db()?;
    let Some((mut user, hash)) = find_user(&conn, email)? else {
        burn_password_check(password);
        return Err(PortalError::InvalidCredentials);
    };
    let locked_until: Option<i64> = conn
        .query_row(
            "SELECT locked_until FROM portal_users WHERE id = ?1",
            params![user.id],
            |row| row.get(0),
        )
        .map_err(db_err)?;
    if let Some(until) = locked_until.filter(|until| *until > now) {
        return Err(PortalError::Locked(until - now));
    }
    if !verify_password(&hash, password) {
        // Lock after repeated failures; the counter restarts once the lock is set.
        conn.execute(
            "UPDATE portal_users SET
                failed_logins = CASE WHEN failed_logins + 1 >= ?2 THEN 0 ELSE failed_logins + 1 END,
                locked_until = CASE WHEN failed_logins + 1 >= ?2 THEN ?3 ELSE locked_until END
             WHERE id = ?1",
            params![user.id, MAX_FAILED_LOGINS, now + LOGIN_LOCK_SECS],
        )
        .map_err(db_err)?;
        return Err(PortalError::InvalidCredentials);
    }
    if !user.is_active() {
        return Err(PortalError::AccountDisabled);
    }
    conn.execute(
        "UPDATE portal_users SET failed_logins = 0, locked_until = NULL, last_login_at = ?2
         WHERE id = ?1",
        params![user.id, now],
    )
    .map_err(db_err)?;
    user.last_login_at = Some(now);
    Ok(user)
}

pub fn reset_password(
    email: &str,
    code: &str,
    password: &str,
    now: i64,
) -> Result<(), PortalError> {
    validate_password(password)?;
    let mut conn = tokens::connect_db()?;
    consume_code(&conn, email, CodePurpose::Reset, code, now)?;
    let Some((user, _)) = find_user(&conn, email)? else {
        return Err(PortalError::InvalidCode);
    };
    if !user.is_active() {
        return Err(PortalError::AccountDisabled);
    }
    let password_hash = hash_password(password)?;
    let tx = conn.transaction().map_err(db_err)?;
    tx.execute(
        "UPDATE portal_users SET password_hash = ?2, failed_logins = 0, locked_until = NULL,
            updated_at = ?3 WHERE id = ?1",
        params![user.id, password_hash, now],
    )
    .map_err(db_err)?;
    tx.execute(
        "DELETE FROM portal_sessions WHERE user_id = ?1",
        params![user.id],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)
}

/// Returns the raw session secret for the cookie; only its hash is stored.
pub fn create_session(
    user_id: &str,
    ip: &str,
    user_agent: Option<&str>,
    now: i64,
) -> Result<String, PortalError> {
    let conn = tokens::connect_db()?;
    conn.execute(
        "DELETE FROM portal_sessions WHERE expires_at <= ?1",
        params![now],
    )
    .map_err(db_err)?;
    let secret = new_session_secret();
    let user_agent: Option<String> = user_agent.map(|ua| ua.chars().take(256).collect());
    conn.execute(
        "INSERT INTO portal_sessions (id_hash, user_id, created_at, expires_at, last_seen_at, ip, user_agent)
         VALUES (?1, ?2, ?3, ?4, ?3, ?5, ?6)",
        params![sha256_hex(&[&secret]), user_id, now, now + SESSION_TTL_SECS, ip, user_agent],
    )
    .map_err(db_err)?;
    Ok(secret)
}

/// The active account behind a session cookie, if the session is still valid.
pub fn session_user(secret: &str, now: i64) -> Result<Option<PortalUser>, PortalError> {
    if secret.is_empty() || secret.len() > 128 {
        return Ok(None);
    }
    let conn = tokens::connect_db()?;
    let id_hash = sha256_hex(&[secret]);
    let user = conn
        .query_row(
            "SELECT u.id, u.email, u.status, u.created_at, u.last_login_at
             FROM portal_sessions s JOIN portal_users u ON u.id = s.user_id
             WHERE s.id_hash = ?1 AND s.expires_at > ?2 AND u.status = 'active'",
            params![id_hash, now],
            |row| {
                Ok(PortalUser {
                    id: row.get(0)?,
                    email: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                    last_login_at: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(db_err)?;
    if user.is_some() {
        conn.execute(
            "UPDATE portal_sessions SET last_seen_at = ?2 WHERE id_hash = ?1",
            params![id_hash, now],
        )
        .map_err(db_err)?;
    }
    Ok(user)
}

pub fn delete_session(secret: &str) -> Result<(), PortalError> {
    tokens::connect_db()?
        .execute(
            "DELETE FROM portal_sessions WHERE id_hash = ?1",
            params![sha256_hex(&[secret])],
        )
        .map_err(db_err)?;
    Ok(())
}

/// Replaces the account's API key; the old key stops working immediately while
/// usage history stays attached to the same token row.
pub fn regenerate_key(user_id: &str, now: i64) -> Result<String, PortalError> {
    let conn = tokens::connect_db()?;
    let value = tokens::generate_token_value();
    let changed = conn
        .execute(
            "UPDATE user_tokens SET token = ?2, updated_at = ?3 WHERE owner_user_id = ?1",
            params![user_id, value, now],
        )
        .map_err(db_err)?;
    if changed == 1 {
        Ok(value)
    } else {
        Err(PortalError::Unavailable("account has no key".into()))
    }
}

// --- Administrator operations ---

pub fn accounts_by_id() -> Result<HashMap<String, PortalUser>, String> {
    let conn = tokens::connect_db()?;
    let mut stmt = conn
        .prepare("SELECT id, email, status, created_at, last_login_at FROM portal_users")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PortalUser {
                id: row.get(0)?,
                email: row.get(1)?,
                status: row.get(2)?,
                created_at: row.get(3)?,
                last_login_at: row.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.map(|row| row.map(|user| (user.id.clone(), user)))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())
}

pub fn get_account(user_id: &str) -> Result<Option<PortalUser>, String> {
    Ok(accounts_by_id()?.remove(user_id))
}

/// Disabling stops the key and signs the user out everywhere. Enabling restores
/// the key only when it already has a quota.
pub fn set_account_enabled(user_id: &str, enabled: bool) -> Result<(), String> {
    let mut conn = tokens::connect_db()?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp();
    let changed = if enabled {
        let changed = tx
            .execute(
                "UPDATE portal_users SET status = 'active', updated_at = ?2 WHERE id = ?1",
                params![user_id, now],
            )
            .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE user_tokens SET enabled = 1, updated_at = ?2
             WHERE owner_user_id = ?1 AND token_quota > 0",
            params![user_id, now],
        )
        .map_err(|e| e.to_string())?;
        changed
    } else {
        tx.execute(
            "UPDATE user_tokens SET enabled = 0, updated_at = ?2 WHERE owner_user_id = ?1",
            params![user_id, now],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM portal_sessions WHERE user_id = ?1",
            params![user_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE portal_users SET status = 'disabled', updated_at = ?2 WHERE id = ?1",
            params![user_id, now],
        )
        .map_err(|e| e.to_string())?
    };
    if changed != 1 {
        return Err("Account not found".into());
    }
    tx.commit().map_err(|e| e.to_string())
}

pub fn revoke_sessions(user_id: &str) -> Result<usize, String> {
    tokens::connect_db()?
        .execute(
            "DELETE FROM portal_sessions WHERE user_id = ?1",
            params![user_id],
        )
        .map_err(|e| e.to_string())
}

/// Removes everything belonging to an account; called when its key is deleted.
pub(crate) fn purge_account(conn: &Connection, user_id: &str) -> Result<(), String> {
    let email: Option<String> = conn
        .query_row(
            "SELECT email FROM portal_users WHERE id = ?1",
            params![user_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM portal_sessions WHERE user_id = ?1",
        params![user_id],
    )
    .map_err(|e| e.to_string())?;
    if let Some(email) = email {
        conn.execute(
            "DELETE FROM portal_email_codes WHERE email = ?1",
            params![email],
        )
        .map_err(|e| e.to_string())?;
    }
    conn.execute("DELETE FROM portal_users WHERE id = ?1", params![user_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> RegistrationDefaults {
        RegistrationDefaults {
            max_concurrency: 2,
            allowed_models: vec![],
            expires_type: "never".into(),
        }
    }

    fn unique_email(tag: &str) -> String {
        format!("{tag}-{}@example.com", uuid::Uuid::new_v4().simple())
    }

    fn issue(email: &str, purpose: CodePurpose, now: i64) -> String {
        match request_code(email, purpose, now).unwrap() {
            CodeDelivery::Code { code, .. } => code,
            other => panic!("expected a code, got {other:?}"),
        }
    }

    fn registered(tag: &str, now: i64) -> (String, PortalUser, UserToken) {
        tokens::init_db().unwrap();
        let email = unique_email(tag);
        let code = issue(&email, CodePurpose::Register, now);
        let (user, token) = register(&email, &code, "correct horse", defaults(), now).unwrap();
        (email, user, token)
    }

    #[test]
    fn portal_email_normalization() {
        assert_eq!(
            normalize_email("  A.B@Example.COM ").unwrap(),
            "a.b@example.com"
        );
        for bad in [
            "",
            "a",
            "a@b",
            "@b.com",
            "a@.com",
            "a@b..com",
            "a b@c.com",
            "a@b@c.com",
        ] {
            assert_eq!(
                normalize_email(bad),
                Err(PortalError::InvalidEmail),
                "{bad}"
            );
        }
    }

    #[test]
    fn portal_registration_creates_one_disabled_zero_quota_key() {
        let now = 1_000_000;
        let (email, user, token) = registered("reg", now);
        assert_eq!(token.owner_user_id.as_deref(), Some(user.id.as_str()));
        assert!(!token.enabled);
        assert_eq!(token.token_quota, 0);
        assert!(
            token.quota_exhausted(),
            "a registered key at quota 0 has no usage"
        );
        assert_eq!(token.remaining_tokens(), Some(0));
        assert_eq!(token.max_concurrency, 2);

        // The code is single use and a second registration is rejected.
        assert_eq!(
            register(&email, "000000", "correct horse", defaults(), now).unwrap_err(),
            PortalError::InvalidCode
        );
        assert_eq!(
            request_code(&email, CodePurpose::Register, now + CODE_RESEND_SECS),
            Ok(CodeDelivery::AlreadyRegistered)
        );
    }

    #[test]
    fn portal_registered_key_cannot_be_enabled_without_quota() {
        let (_, user, token) = registered("guard", 1_000_000);
        let enable = |quota: Option<i64>| {
            tokens::update_token(
                &token.id,
                None,
                None,
                Some(true),
                None,
                None,
                None,
                None,
                quota,
                None,
                None,
            )
        };
        let error = enable(None).unwrap_err();
        assert!(error.contains(tokens::OWNED_KEY_REQUIRES_QUOTA), "{error}");
        // Renewal extends a pending key without enabling it.
        tokens::renew_token(&token.id, "month").unwrap();
        assert!(!tokens::get_token_by_id(&token.id).unwrap().unwrap().enabled);

        enable(Some(500)).unwrap();
        let active = tokens::get_token_by_id(&token.id).unwrap().unwrap();
        assert!(active.enabled && !active.quota_exhausted());
        // Taking the quota back to 0 while enabled is refused.
        assert!(tokens::update_token(
            &token.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(0),
            None,
            None
        )
        .is_err());

        set_account_enabled(&user.id, false).unwrap();
        assert!(!tokens::get_token_by_id(&token.id).unwrap().unwrap().enabled);
        assert!(
            enable(None).is_err(),
            "a disabled account keeps its key off"
        );
        set_account_enabled(&user.id, true).unwrap();
        assert!(tokens::get_token_by_id(&token.id).unwrap().unwrap().enabled);
    }

    #[test]
    fn portal_code_rules() {
        tokens::init_db().unwrap();
        let now = 2_000_000;
        let email = unique_email("code");
        let code = issue(&email, CodePurpose::Register, now);
        assert!(matches!(
            request_code(&email, CodePurpose::Register, now + 10),
            Err(PortalError::CodeCooldown(50))
        ));
        let wrong = if code == "123456" { "654321" } else { "123456" };
        for _ in 0..CODE_MAX_ATTEMPTS {
            assert_eq!(
                register(&email, wrong, "correct horse", defaults(), now).unwrap_err(),
                PortalError::InvalidCode
            );
        }
        // Too many wrong guesses burn the code even when the right one arrives later.
        assert_eq!(
            register(&email, &code, "correct horse", defaults(), now).unwrap_err(),
            PortalError::InvalidCode
        );
        let fresh = issue(&email, CodePurpose::Register, now + CODE_RESEND_SECS);
        assert_eq!(
            register(
                &email,
                &fresh,
                "correct horse",
                defaults(),
                now + CODE_TTL_SECS * 2
            )
            .unwrap_err(),
            PortalError::InvalidCode,
            "expired"
        );
        // Unknown addresses get the same rate limiting but nothing to send.
        let unknown = unique_email("unknown");
        assert_eq!(
            request_code(&unknown, CodePurpose::Reset, now),
            Ok(CodeDelivery::Nothing)
        );
        assert!(matches!(
            request_code(&unknown, CodePurpose::Reset, now + 1),
            Err(PortalError::CodeCooldown(_))
        ));
    }

    #[test]
    fn portal_login_lockout_reset_and_sessions() {
        let now = 3_000_000;
        let (email, user, token) = registered("login", now);
        assert_eq!(login(&email, "correct horse", now).unwrap().id, user.id);
        assert_eq!(
            login(&unique_email("nobody"), "correct horse", now).unwrap_err(),
            PortalError::InvalidCredentials
        );

        let secret = create_session(&user.id, "1.2.3.4", Some("ua"), now).unwrap();
        assert_eq!(session_user(&secret, now + 1).unwrap().unwrap().id, user.id);
        assert!(session_user(&secret, now + SESSION_TTL_SECS)
            .unwrap()
            .is_none());

        for _ in 0..MAX_FAILED_LOGINS {
            assert_eq!(
                login(&email, "wrong password", now).unwrap_err(),
                PortalError::InvalidCredentials
            );
        }
        assert!(matches!(
            login(&email, "correct horse", now),
            Err(PortalError::Locked(_))
        ));

        let code = issue(&email, CodePurpose::Reset, now + CODE_RESEND_SECS);
        reset_password(&email, &code, "new password!", now).unwrap();
        assert!(
            session_user(&secret, now + 1).unwrap().is_none(),
            "reset signs out"
        );
        assert!(
            login(&email, "new password!", now).is_ok(),
            "reset clears the lock"
        );

        let old_value = token.token.clone();
        let new_value = regenerate_key(&user.id, now).unwrap();
        assert!(tokens::get_token_by_value(&old_value).unwrap().is_none());
        assert_eq!(
            tokens::get_token_by_value(&new_value).unwrap().unwrap().id,
            token.id
        );

        let secret = create_session(&user.id, "1.2.3.4", None, now).unwrap();
        set_account_enabled(&user.id, false).unwrap();
        assert!(session_user(&secret, now).unwrap().is_none());
        assert_eq!(
            login(&email, "new password!", now).unwrap_err(),
            PortalError::AccountDisabled
        );
    }

    #[test]
    fn portal_deleting_the_key_deletes_the_account() {
        let now = 4_000_000;
        let (email, user, token) = registered("delete", now);
        tokens::delete_token(&token.id).unwrap();
        assert!(get_account(&user.id).unwrap().is_none());
        assert!(tokens::get_token_by_owner(&user.id).unwrap().is_none());
        // The address can register again.
        let code = issue(&email, CodePurpose::Register, now + CODE_RESEND_SECS);
        assert!(register(&email, &code, "correct horse", defaults(), now).is_ok());
    }
}
