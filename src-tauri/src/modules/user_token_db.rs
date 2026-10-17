//! User Token Database Module
//! UserToken 数据库操作模块

#![allow(dead_code)]
// 用户令牌存储，部分接口留作后续扩展

use chrono::{FixedOffset, Timelike, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// 用户令牌结构体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserToken {
    pub id: String,
    pub token: String,
    pub username: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub expires_type: String, // "day", "week", "month", "never"
    pub expires_at: Option<i64>,
    pub max_concurrency: i32,         // 0 = unlimited
    pub allowed_models: Vec<String>,  // empty = all public model IDs
    pub token_quota: i64,             // lifetime input + output tokens; 0 = unlimited
    pub max_ips: i32,                 // 0 = unlimited
    pub curfew_start: Option<String>, // "HH:MM" 宵禁开始时间
    pub curfew_end: Option<String>,   // "HH:MM" 宵禁结束时间
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub total_requests: i64,
    pub total_tokens_used: i64,
    /// Usage-portal account that registered this key; `None` for administrator keys.
    #[serde(default)]
    pub owner_user_id: Option<String>,
}

impl UserToken {
    /// Administrator keys use 0 as "unlimited"; a registered account's key never
    /// gets that shortcut, so its quota of 0 means no usage at all.
    pub fn has_quota_limit(&self) -> bool {
        self.token_quota > 0 || self.owner_user_id.is_some()
    }

    pub fn quota_exhausted(&self) -> bool {
        self.has_quota_limit() && self.total_tokens_used >= self.token_quota
    }

    pub fn remaining_tokens(&self) -> Option<i64> {
        self.has_quota_limit()
            .then(|| (self.token_quota - self.total_tokens_used).max(0))
    }
}

/// Inputs for a new key; administrators and portal registration share this path.
pub(crate) struct NewToken {
    pub username: String,
    pub description: Option<String>,
    pub expires_type: String,
    pub custom_expires_at: Option<i64>,
    pub max_ips: i32,
    pub curfew_start: Option<String>,
    pub curfew_end: Option<String>,
    pub max_concurrency: i32,
    pub token_quota: i64,
    pub allowed_models: Vec<String>,
    pub owner_user_id: Option<String>,
    pub enabled: bool,
}

/// Database-level rule: a registered account's key can only be enabled while it has
/// a positive quota and its account is active. Enforced by triggers so every write
/// path (admin edit, renewal, older binaries after a rollback) fails closed.
const OWNED_KEY_ENABLE_BLOCKED: &str = "NEW.owner_user_id IS NOT NULL AND NEW.enabled <> 0 AND \
    (NEW.token_quota <= 0 OR NOT EXISTS (SELECT 1 FROM portal_users p \
    WHERE p.id = NEW.owner_user_id AND p.status = 'active'))";
pub const OWNED_KEY_REQUIRES_QUOTA: &str = "registered_token_requires_quota";

fn map_write_error(context: &str, error: rusqlite::Error) -> String {
    let text = error.to_string();
    if text.contains(OWNED_KEY_REQUIRES_QUOTA) {
        format!("{OWNED_KEY_REQUIRES_QUOTA}: a registered user's token needs a quota above 0 and an active account before it can be enabled")
    } else {
        format!("{context}: {text}")
    }
}

pub(crate) fn generate_token_value() -> String {
    format!("sk-{}", Uuid::new_v4().simple())
}

/// 令牌 IP 绑定结构体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenIpBinding {
    pub id: String,
    pub token_id: String,
    pub ip_address: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub request_count: i64,
    pub user_agent: Option<String>,
}

/// 令牌使用日志结构体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsageLog {
    pub id: String,
    pub token_id: String,
    pub ip_address: String,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub request_time: i64,
    pub status: u16,
}

/// 获取数据库路径
pub fn get_db_path() -> Result<PathBuf, String> {
    let mut path = crate::modules::account::get_data_dir()?;
    path.push("user_tokens.db");
    Ok(path)
}

/// 连接数据库
pub fn connect_db() -> Result<Connection, String> {
    let path = get_db_path()?;
    let conn = Connection::open(&path).map_err(|e| format!("Failed to open database: {}", e))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| format!("Failed to configure database: {e}"))?;
    Ok(conn)
}

/// 初始化数据库
pub fn init_db() -> Result<(), String> {
    let mut conn = connect_db()?;
    init_db_connection(&mut conn)
}

fn init_db_connection(conn: &mut Connection) -> Result<(), String> {
    let conn = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("Failed to begin migration: {e}"))?;

    // 创建 user_tokens 表
    conn.execute(
        "CREATE TABLE IF NOT EXISTS user_tokens (
            id TEXT PRIMARY KEY,
            token TEXT UNIQUE NOT NULL,
            username TEXT NOT NULL,
            description TEXT,
            enabled BOOLEAN NOT NULL DEFAULT 1,
            expires_type TEXT NOT NULL,
            expires_at INTEGER,
            max_ips INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            last_used_at INTEGER,
            total_requests INTEGER NOT NULL DEFAULT 0,
            total_tokens_used INTEGER NOT NULL DEFAULT 0,
            curfew_start TEXT,
            curfew_end TEXT
        )",
        [],
    )
    .map_err(|e| format!("Failed to create user_tokens table: {}", e))?;

    // 尝试添加新列 (用于旧数据库迁移，忽略已存在的错误)
    let _ = conn.execute("ALTER TABLE user_tokens ADD COLUMN expires_type TEXT", []);
    let _ = conn.execute("ALTER TABLE user_tokens ADD COLUMN expires_at INTEGER", []);
    let _ = conn.execute(
        "ALTER TABLE user_tokens ADD COLUMN max_ips INTEGER DEFAULT 0",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE user_tokens ADD COLUMN total_requests INTEGER DEFAULT 0",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE user_tokens ADD COLUMN total_tokens_used INTEGER DEFAULT 0",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE user_tokens ADD COLUMN last_used_at INTEGER",
        [],
    );
    let _ = conn.execute("ALTER TABLE user_tokens ADD COLUMN curfew_start TEXT", []);
    let _ = conn.execute("ALTER TABLE user_tokens ADD COLUMN curfew_end TEXT", []);

    // Additive migration: preserve keys and usage; report errors instead of silently
    // treating a missing quota column as unlimited.
    for (column, definition) in [
        ("allowed_models", "TEXT NOT NULL DEFAULT '[]'"),
        (
            "max_concurrency",
            "INTEGER NOT NULL DEFAULT 0 CHECK(max_concurrency >= 0)",
        ),
        (
            "token_quota",
            "INTEGER NOT NULL DEFAULT 0 CHECK(token_quota >= 0)",
        ),
        ("owner_user_id", "TEXT"),
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('user_tokens') WHERE name = ?1)",
                [column],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to inspect token schema: {e}"))?;
        if !exists {
            conn.execute(
                &format!("ALTER TABLE user_tokens ADD COLUMN {column} {definition}"),
                [],
            )
            .map_err(|e| format!("Failed to migrate {column}: {e}"))?;
        }
    }

    crate::modules::portal_account::migrate(&conn)?;
    conn.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS user_tokens_owned_key_insert;
         DROP TRIGGER IF EXISTS user_tokens_owned_key_update;
         CREATE TRIGGER user_tokens_owned_key_insert BEFORE INSERT ON user_tokens
         WHEN {OWNED_KEY_ENABLE_BLOCKED}
         BEGIN SELECT RAISE(ABORT, '{OWNED_KEY_REQUIRES_QUOTA}'); END;
         CREATE TRIGGER user_tokens_owned_key_update BEFORE UPDATE ON user_tokens
         WHEN {OWNED_KEY_ENABLE_BLOCKED}
         BEGIN SELECT RAISE(ABORT, '{OWNED_KEY_REQUIRES_QUOTA}'); END;"
    ))
    .map_err(|e| format!("Failed to install registered token guard: {e}"))?;

    // 创建 token_ip_bindings 表
    conn.execute(
        "CREATE TABLE IF NOT EXISTS token_ip_bindings (
            id TEXT PRIMARY KEY,
            token_id TEXT NOT NULL,
            ip_address TEXT NOT NULL,
            first_seen_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            request_count INTEGER NOT NULL DEFAULT 0,
            user_agent TEXT,
            FOREIGN KEY(token_id) REFERENCES user_tokens(id) ON DELETE CASCADE,
            UNIQUE(token_id, ip_address)
        )",
        [],
    )
    .map_err(|e| format!("Failed to create token_ip_bindings table: {}", e))?;

    // 创建 token_usage_logs 表
    conn.execute(
        "CREATE TABLE IF NOT EXISTS token_usage_logs (
            id TEXT PRIMARY KEY,
            token_id TEXT NOT NULL,
            ip_address TEXT,
            model TEXT,
            input_tokens INTEGER,
            output_tokens INTEGER,
            request_time INTEGER NOT NULL,
            status INTEGER,
            FOREIGN KEY(token_id) REFERENCES user_tokens(id) ON DELETE CASCADE
        )",
        [],
    )
    .map_err(|e| format!("Failed to create token_usage_logs table: {}", e))?;

    // Historical rows remain NULL: missing cache telemetry is not a zero hit count.
    let has_cache: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('token_usage_logs') WHERE name = 'cached_tokens')",
        [], |row| row.get(0),
    ).map_err(|e| e.to_string())?;
    if !has_cache {
        conn.execute("ALTER TABLE token_usage_logs ADD COLUMN cached_tokens INTEGER CHECK(cached_tokens >= 0)", [])
            .map_err(|e| format!("Failed to migrate cached tokens: {e}"))?;
    }

    // 创建索引
    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_token_usage_logs_token_id ON token_usage_logs(token_id)",
        [],
    );
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_token_usage_logs_request_time ON token_usage_logs(request_time)", []);
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_token_usage_logs_token_time ON token_usage_logs(token_id, request_time DESC)", []);

    // [FIX Issue #1719] 数据清洗：修复旧版本升级导致的 NULL 字段
    // 这些字段在旧版本中可能不存在，ALTER TABLE 添加后默认为 NULL，导致反序列化失败
    let _ = conn.execute("UPDATE user_tokens SET expires_type = 'never' WHERE expires_type IS NULL OR expires_type = ''", []);
    let _ = conn.execute(
        "UPDATE user_tokens SET max_ips = 0 WHERE max_ips IS NULL",
        [],
    );
    let _ = conn.execute(
        "UPDATE user_tokens SET total_requests = 0 WHERE total_requests IS NULL",
        [],
    );
    let _ = conn.execute(
        "UPDATE user_tokens SET total_tokens_used = 0 WHERE total_tokens_used IS NULL",
        [],
    );
    let _ = conn.execute(
        "UPDATE user_tokens SET enabled = 1 WHERE enabled IS NULL",
        [],
    );

    conn.commit()
        .map_err(|e| format!("Failed to commit token migration: {e}"))
}

/// Resolve an expiration type to an absolute timestamp (seconds); relative types count
/// from `now`. Every write path uses this, so an invalid choice is rejected instead of
/// silently producing a token that never expires.
pub fn resolve_expires_at(
    expires_type: &str,
    custom_expires_at: Option<i64>,
    now: i64,
) -> Result<Option<i64>, String> {
    const DAY: i64 = 86_400;
    match expires_type {
        "day" => Ok(Some(now + DAY)),
        "week" => Ok(Some(now + 7 * DAY)),
        "month" => Ok(Some(now + 30 * DAY)),
        "never" => Ok(None),
        "custom" => match custom_expires_at {
            Some(at) if at > now => Ok(Some(at)),
            Some(_) => Err("Custom expiration time must be in the future".into()),
            None => Err("Custom expiration time is required".into()),
        },
        other => Err(format!("Unsupported expiration type: {other}")),
    }
}

pub fn validate_limits(max_concurrency: i32, token_quota: i64) -> Result<(), String> {
    if max_concurrency < 0 || !(0..=9_007_199_254_740_991).contains(&token_quota) {
        return Err("Concurrency and quota must be non-negative integers; quota must not exceed 9007199254740991".into());
    }
    Ok(())
}

/// Exact client-facing model IDs; aliases remain subject to the administrator's routing.
pub fn normalize_allowed_models(models: Vec<String>) -> Result<Vec<String>, String> {
    if models.len() > 256 {
        return Err("Select no more than 256 models".into());
    }
    let mut normalized = Vec::new();
    for model in models {
        let model = model.trim().strip_prefix("models/").unwrap_or(model.trim());
        if model.is_empty()
            || model.len() > 200
            || !model
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
        {
            return Err("Invalid model ID; use an exact model name without wildcards".into());
        }
        normalized.push(model.to_string());
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn read_allowed_models(row: &rusqlite::Row<'_>) -> rusqlite::Result<Vec<String>> {
    let raw: String = row.get("allowed_models")?;
    serde_json::from_str(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            row.as_ref().column_index("allowed_models").unwrap_or(0),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

#[derive(Debug, Serialize)]
pub struct ModelUsage {
    pub model: String,
    pub requests: i64,
    pub failed_requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub last_used_at: i64,
    pub cached_tokens: Option<i64>,
    pub cache_recorded_requests: i64,
}

pub fn model_usage_for_token(token_id: &str) -> Result<Vec<ModelUsage>, String> {
    let conn = connect_db()?;
    let mut statement = conn.prepare(
        "SELECT COALESCE(NULLIF(model, ''), 'unknown') AS model_name, COUNT(*),
         SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END),
         SUM(MAX(COALESCE(input_tokens, 0), 0)), SUM(MAX(COALESCE(output_tokens, 0), 0)), MAX(request_time), SUM(cached_tokens), COUNT(cached_tokens)
         FROM token_usage_logs WHERE token_id = ?1 GROUP BY model_name
         ORDER BY SUM(MAX(COALESCE(input_tokens, 0), 0) + MAX(COALESCE(output_tokens, 0), 0)) DESC, model_name"
    ).map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([token_id], |r| {
            Ok(ModelUsage {
                model: r.get(0)?,
                requests: r.get(1)?,
                failed_requests: r.get(2)?,
                input_tokens: r.get(3)?,
                output_tokens: r.get(4)?,
                last_used_at: r.get(5)?,
                cached_tokens: r.get(6)?,
                cache_recorded_requests: r.get(7)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Public DTOs intentionally omit token IDs, IP addresses and request/response content.
#[derive(Debug, Serialize)]
pub struct PublicUsageLog {
    pub id: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: Option<i64>,
    pub request_time: i64,
    pub status: i32,
}
#[derive(Debug, Serialize)]
pub struct UsageLogPage {
    pub items: Vec<PublicUsageLog>,
    pub page: u32,
    pub page_size: u32,
    pub total: i64,
}
pub fn usage_logs_for_token(token_id: &str, page: u32) -> Result<UsageLogPage, String> {
    filtered_usage_logs_for_token(token_id, page, None, None)
}
pub fn filtered_usage_logs_for_token(
    token_id: &str,
    page: u32,
    start_time: Option<i64>,
    end_time: Option<i64>,
) -> Result<UsageLogPage, String> {
    let conn = connect_db()?;
    let total = conn
        .query_row(
            "SELECT COUNT(*) FROM token_usage_logs WHERE token_id = ?1 AND request_time >= ?2 AND request_time <= ?3",
            params![token_id, start_time.unwrap_or(i64::MIN), end_time.unwrap_or(i64::MAX)],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let page_size = 20;
    let mut stmt = conn.prepare("SELECT id, COALESCE(NULLIF(model, ''), 'unknown'), MAX(COALESCE(input_tokens, 0), 0), MAX(COALESCE(output_tokens, 0), 0), cached_tokens, request_time, COALESCE(status, 0) FROM token_usage_logs WHERE token_id = ?1 AND request_time >= ?4 AND request_time <= ?5 ORDER BY request_time DESC, id DESC LIMIT ?2 OFFSET ?3").map_err(|e| e.to_string())?;
    let items = stmt
        .query_map(
            params![
                token_id,
                page_size,
                i64::from(page.saturating_sub(1)) * i64::from(page_size),
                start_time.unwrap_or(i64::MIN),
                end_time.unwrap_or(i64::MAX)
            ],
            |r| {
                Ok(PublicUsageLog {
                    id: r.get(0)?,
                    model: r.get(1)?,
                    input_tokens: r.get(2)?,
                    output_tokens: r.get(3)?,
                    cached_tokens: r.get(4)?,
                    request_time: r.get(5)?,
                    status: r.get(6)?,
                })
            },
        )
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(UsageLogPage {
        items,
        page,
        page_size,
        total,
    })
}

#[derive(Debug, Serialize)]
pub struct UsagePeriod {
    pub start_date: String,
    pub end_date: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: Option<i64>,
    pub cache_recorded_requests: i64,
}
/// UTC+8 calendar days and ISO weeks (Monday start), including empty periods.
pub fn period_usage_for_token(
    token_id: &str,
    weekly: bool,
    now: i64,
) -> Result<Vec<UsagePeriod>, String> {
    use chrono::{Datelike, Duration, TimeZone};
    let timezone = FixedOffset::east_opt(8 * 3600).unwrap();
    let today = timezone
        .timestamp_opt(now, 0)
        .single()
        .ok_or("Invalid timestamp")?
        .date_naive();
    let current = if weekly {
        today - Duration::days(i64::from(today.weekday().num_days_from_monday()))
    } else {
        today
    };
    let count = if weekly { 12 } else { 30 };
    let span = if weekly { 7 } else { 1 };
    let oldest = current - Duration::days((count - 1) * span);
    let from = timezone
        .from_local_datetime(&oldest.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp();
    let until = timezone
        .from_local_datetime(
            &(current + Duration::days(span))
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .single()
        .unwrap()
        .timestamp();
    let conn = connect_db()?;
    let mut stmt = conn.prepare("SELECT (request_time - ?2) / ?3 AS bucket, COUNT(*), SUM(MAX(COALESCE(input_tokens, 0), 0)), SUM(MAX(COALESCE(output_tokens, 0), 0)), SUM(cached_tokens), COUNT(cached_tokens) FROM token_usage_logs WHERE token_id = ?1 AND request_time >= ?2 AND request_time < ?4 GROUP BY bucket").map_err(|e| e.to_string())?;
    let mut periods: Vec<_> = (0..count)
        .map(|i| {
            let start = oldest + Duration::days(i * span);
            UsagePeriod {
                start_date: start.to_string(),
                end_date: (start + Duration::days(span - 1)).to_string(),
                requests: 0,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: Some(0),
                cache_recorded_requests: 0,
            }
        })
        .collect();
    let rows = stmt
        .query_map(params![token_id, from, span * 86400, until], |r| {
            Ok((
                r.get::<_, usize>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (i, requests, input, output, cached, recorded) = row.map_err(|e| e.to_string())?;
        if let Some(p) = periods.get_mut(i) {
            p.requests = requests;
            p.input_tokens = input;
            p.output_tokens = output;
            p.cached_tokens = cached;
            p.cache_recorded_requests = recorded;
        }
    }
    periods.reverse();
    Ok(periods)
}

/// 创建新令牌
pub fn create_token(
    username: String,
    expires_type: String,
    description: Option<String>,
    max_ips: i32,
    curfew_start: Option<String>,
    curfew_end: Option<String>,
    custom_expires_at: Option<i64>, // 自定义过期时间戳 (秒)
    max_concurrency: i32,
    token_quota: i64,
    allowed_models: Vec<String>,
) -> Result<UserToken, String> {
    let conn = connect_db()?;
    insert_token(
        &conn,
        NewToken {
            username,
            description,
            expires_type,
            custom_expires_at,
            max_ips,
            curfew_start,
            curfew_end,
            max_concurrency,
            token_quota,
            allowed_models,
            owner_user_id: None,
            enabled: true,
        },
    )
}

pub(crate) fn insert_token(conn: &Connection, spec: NewToken) -> Result<UserToken, String> {
    validate_limits(spec.max_concurrency, spec.token_quota)?;
    let allowed_models = normalize_allowed_models(spec.allowed_models)?;
    let now = Utc::now().timestamp();
    let expires_at = resolve_expires_at(&spec.expires_type, spec.custom_expires_at, now)?;

    let user_token = UserToken {
        id: Uuid::new_v4().to_string(),
        token: generate_token_value(),
        username: spec.username,
        description: spec.description,
        enabled: spec.enabled,
        expires_type: spec.expires_type,
        expires_at,
        max_ips: spec.max_ips,
        max_concurrency: spec.max_concurrency,
        token_quota: spec.token_quota,
        allowed_models,
        curfew_start: spec.curfew_start,
        curfew_end: spec.curfew_end,
        created_at: now,
        updated_at: now,
        last_used_at: None,
        total_requests: 0,
        total_tokens_used: 0,
        owner_user_id: spec.owner_user_id,
    };

    conn.execute(
        "INSERT INTO user_tokens (
            id, token, username, description, enabled, expires_type, expires_at, max_ips,
            curfew_start, curfew_end,
            created_at, updated_at, total_requests, total_tokens_used, max_concurrency, token_quota, allowed_models,
            owner_user_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            user_token.id,
            user_token.token,
            user_token.username,
            user_token.description,
            user_token.enabled,
            user_token.expires_type,
            user_token.expires_at,
            user_token.max_ips,
            user_token.curfew_start,
            user_token.curfew_end,
            user_token.created_at,
            user_token.updated_at,
            user_token.total_requests,
            user_token.total_tokens_used,
            user_token.max_concurrency,
            user_token.token_quota,
            serde_json::to_string(&user_token.allowed_models).map_err(|e| e.to_string())?,
            user_token.owner_user_id,
        ],
    )
    .map_err(|e| map_write_error("Failed to insert user token", e))?;

    Ok(user_token)
}

/// Lenient row mapping: columns added by migrations may still be NULL on old rows.
fn token_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserToken> {
    Ok(UserToken {
        id: row.get("id")?,
        token: row.get("token")?,
        username: row.get("username")?,
        description: row.get("description")?,
        enabled: row.get("enabled").unwrap_or(true),
        expires_type: row
            .get("expires_type")
            .unwrap_or_else(|_| "never".to_string()),
        expires_at: row.get("expires_at").unwrap_or(None),
        max_ips: row.get("max_ips").unwrap_or(0),
        max_concurrency: row.get("max_concurrency")?,
        token_quota: row.get("token_quota")?,
        allowed_models: read_allowed_models(row)?,
        curfew_start: row.get("curfew_start").unwrap_or(None),
        curfew_end: row.get("curfew_end").unwrap_or(None),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_used_at: row.get("last_used_at").unwrap_or(None),
        total_requests: row.get("total_requests").unwrap_or(0),
        total_tokens_used: row.get("total_tokens_used").unwrap_or(0),
        owner_user_id: row.get("owner_user_id")?,
    })
}

fn query_token(sql: &str, value: &str) -> Result<Option<UserToken>, String> {
    let conn = connect_db()?;
    conn.query_row(sql, params![value], token_from_row)
        .optional()
        .map_err(|e| format!("Failed to query token: {}", e))
}

/// 列出所有令牌
pub fn list_tokens() -> Result<Vec<UserToken>, String> {
    let conn = connect_db()?;
    let mut stmt = conn
        .prepare("SELECT * FROM user_tokens ORDER BY created_at DESC")
        .map_err(|e| format!("Failed to prepare query: {}", e))?;

    let token_iter = stmt
        .query_map([], token_from_row)
        .map_err(|e| format!("Failed to query tokens: {}", e))?;

    let mut tokens = Vec::new();
    for token in token_iter {
        tokens.push(token.map_err(|e| format!("Failed to parse token row: {}", e))?);
    }

    Ok(tokens)
}

/// 获取单个令牌信息
pub fn get_token_by_id(id: &str) -> Result<Option<UserToken>, String> {
    query_token("SELECT * FROM user_tokens WHERE id = ?1", id)
}

/// 根据 Token 值获取令牌信息
pub fn get_token_by_value(token: &str) -> Result<Option<UserToken>, String> {
    query_token("SELECT * FROM user_tokens WHERE token = ?1", token)
}

/// The single key owned by a usage-portal account.
pub fn get_token_by_owner(user_id: &str) -> Result<Option<UserToken>, String> {
    query_token(
        "SELECT * FROM user_tokens WHERE owner_user_id = ?1",
        user_id,
    )
}

/// 更新令牌状态/备注等
pub fn update_token(
    id: &str,
    username: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
    max_ips: Option<i32>,
    curfew_start: Option<Option<String>>,
    curfew_end: Option<Option<String>>,
    max_concurrency: Option<i32>,
    token_quota: Option<i64>,
    allowed_models: Option<Vec<String>>,
    expires: Option<(String, Option<i64>)>, // (expires_type, custom_expires_at)
) -> Result<(), String> {
    validate_limits(max_concurrency.unwrap_or(0), token_quota.unwrap_or(0))?;
    let allowed_models = allowed_models.map(normalize_allowed_models).transpose()?;
    let now = Utc::now().timestamp();
    let expires = expires
        .map(|(kind, custom)| resolve_expires_at(&kind, custom, now).map(|at| (kind, at)))
        .transpose()?;
    let conn = connect_db()?;

    let mut query = "UPDATE user_tokens SET updated_at = ?1".to_string();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now)];
    let mut param_idx = 2;

    if let Some(user) = username {
        query.push_str(&format!(", username = ?{}", param_idx));
        params_vec.push(Box::new(user));
        param_idx += 1;
    }

    if let Some(desc) = description {
        query.push_str(&format!(", description = ?{}", param_idx));
        params_vec.push(Box::new(desc));
        param_idx += 1;
    }

    if let Some(en) = enabled {
        query.push_str(&format!(", enabled = ?{}", param_idx));
        params_vec.push(Box::new(en));
        param_idx += 1;
    }

    if let Some(ips) = max_ips {
        query.push_str(&format!(", max_ips = ?{}", param_idx));
        params_vec.push(Box::new(ips));
        param_idx += 1;
    }

    if let Some(limit) = max_concurrency {
        query.push_str(&format!(", max_concurrency = ?{}", param_idx));
        params_vec.push(Box::new(limit));
        param_idx += 1;
    }
    if let Some(quota) = token_quota {
        query.push_str(&format!(", token_quota = ?{}", param_idx));
        params_vec.push(Box::new(quota));
        param_idx += 1;
    }

    if let Some(models) = allowed_models {
        query.push_str(&format!(", allowed_models = ?{}", param_idx));
        params_vec.push(Box::new(
            serde_json::to_string(&models).map_err(|e| e.to_string())?,
        ));
        param_idx += 1;
    }

    if let Some(start) = curfew_start {
        query.push_str(&format!(", curfew_start = ?{}", param_idx));
        params_vec.push(Box::new(start));
        param_idx += 1;
    }

    if let Some(end) = curfew_end {
        query.push_str(&format!(", curfew_end = ?{}", param_idx));
        params_vec.push(Box::new(end));
        param_idx += 1;
    }

    if let Some((kind, expires_at)) = expires {
        query.push_str(&format!(
            ", expires_type = ?{}, expires_at = ?{}",
            param_idx,
            param_idx + 1
        ));
        params_vec.push(Box::new(kind));
        params_vec.push(Box::new(expires_at));
        param_idx += 2;
    }

    query.push_str(&format!(" WHERE id = ?{}", param_idx));
    params_vec.push(Box::new(id.to_string()));

    // 将 Vec<Box<dyn ToSql>> 转换为 &[&dyn ToSql]
    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

    conn.execute(&query, params_refs.as_slice())
        .map_err(|e| map_write_error("Failed to update user token", e))?;

    Ok(())
}

/// 续期令牌
pub fn renew_token(id: &str, expires_type: &str) -> Result<(), String> {
    let now = Utc::now().timestamp();
    // Renewal carries no timestamp, so "custom" is rejected here instead of clearing the expiry.
    let expires_at = resolve_expires_at(expires_type, None, now)?;
    let conn = connect_db()?;

    // Renewal re-enables administrator keys; a registered key is re-enabled only when
    // the guard trigger would allow it, so renewing a pending key just extends it.
    conn.execute(
        "UPDATE user_tokens SET expires_type = ?1, expires_at = ?2, updated_at = ?3,
            enabled = CASE WHEN owner_user_id IS NULL OR (token_quota > 0 AND EXISTS (
                SELECT 1 FROM portal_users p WHERE p.id = owner_user_id AND p.status = 'active'))
            THEN 1 ELSE enabled END
         WHERE id = ?4",
        params![expires_type, expires_at, now, id],
    )
    .map_err(|e| format!("Failed to renew token: {}", e))?;

    Ok(())
}

/// 删除令牌
/// Deleting a registered user's key also deletes the account, since each account
/// owns exactly one key; the email can then register again.
pub fn delete_token(id: &str) -> Result<(), String> {
    let mut conn = connect_db()?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to delete token: {}", e))?;
    let owner: Option<String> = tx
        .query_row(
            "SELECT owner_user_id FROM user_tokens WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("Failed to delete token: {}", e))?
        .flatten();
    tx.execute("DELETE FROM user_tokens WHERE id = ?1", params![id])
        .map_err(|e| format!("Failed to delete token: {}", e))?;
    if let Some(user_id) = owner {
        crate::modules::portal_account::purge_account(&tx, &user_id)?;
    }
    tx.commit()
        .map_err(|e| format!("Failed to delete token: {}", e))
}

/// 获取令牌的所有 IP 绑定
pub fn get_token_ips(token_id: &str) -> Result<Vec<TokenIpBinding>, String> {
    let conn = connect_db()?;
    let mut stmt = conn
        .prepare("SELECT * FROM token_ip_bindings WHERE token_id = ?1 ORDER BY last_seen_at DESC")
        .map_err(|e| format!("Failed to prepare query: {}", e))?;

    let iter = stmt
        .query_map(params![token_id], |row| {
            Ok(TokenIpBinding {
                id: row.get("id")?,
                token_id: row.get("token_id")?,
                ip_address: row.get("ip_address")?,
                first_seen_at: row.get("first_seen_at")?,
                last_seen_at: row.get("last_seen_at")?,
                request_count: row.get("request_count")?,
                user_agent: row.get("user_agent")?,
            })
        })
        .map_err(|e| format!("Failed to query token IPs: {}", e))?;

    let mut bindings = Vec::new();
    for b in iter {
        bindings.push(b.map_err(|e| format!("Failed to parse binding row: {}", e))?);
    }

    Ok(bindings)
}

/// 记录/更新令牌使用情况 (同时处理 user_tokens 和 token_ip_bindings)
pub fn record_token_usage_and_ip(
    token_id: &str,
    ip: &str,
    model: &str,
    input_tokens: i32,
    output_tokens: i32,
    status: u16,
    user_agent: Option<String>,
) -> Result<(), String> {
    record_token_usage_with_cache(
        token_id,
        ip,
        model,
        input_tokens,
        output_tokens,
        None,
        status,
        user_agent,
    )
}

pub fn record_token_usage_with_cache(
    token_id: &str,
    ip: &str,
    model: &str,
    input_tokens: i32,
    output_tokens: i32,
    cached_tokens: Option<u32>,
    status: u16,
    user_agent: Option<String>,
) -> Result<(), String> {
    let mut conn = connect_db()?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("Failed to create transaction: {}", e))?;
    let now = Utc::now().timestamp();

    // 1. 更新 user_tokens 主表
    tx.execute(
        "UPDATE user_tokens SET 
            last_used_at = ?1, 
            total_requests = total_requests + 1, 
            total_tokens_used = total_tokens_used + ?2 
        WHERE id = ?3",
        params![
            now,
            i64::from(input_tokens.max(0)) + i64::from(output_tokens.max(0)),
            token_id
        ],
    )
    .map_err(|e| format!("Failed to update user_tokens stats: {}", e))?;

    // 2. 更新或插入 token_ip_bindings 表
    let binding_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM token_ip_bindings WHERE token_id = ?1 AND ip_address = ?2)",
        params![token_id, ip],
        |row| row.get(0),
    ).unwrap_or(false);

    if binding_exists {
        tx.execute(
            "UPDATE token_ip_bindings SET 
                last_seen_at = ?1, 
                request_count = request_count + 1,
                user_agent = COALESCE(?2, user_agent)
            WHERE token_id = ?3 AND ip_address = ?4",
            params![now, user_agent, token_id, ip],
        )
        .map_err(|e| format!("Failed to update ip binding: {}", e))?;
    } else {
        let binding_id = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO token_ip_bindings (
                id, token_id, ip_address, first_seen_at, last_seen_at, request_count, user_agent
            ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
            params![binding_id, token_id, ip, now, now, user_agent],
        )
        .map_err(|e| format!("Failed to insert ip binding: {}", e))?;
    }

    // 3. 插入 token_usage_logs 表
    let log_id = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO token_usage_logs (
            id, token_id, ip_address, model, input_tokens, output_tokens, request_time, status, cached_tokens
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            log_id,
            token_id,
            ip,
            model,
            input_tokens,
            output_tokens,
            now,
            status,
            cached_tokens.map(i64::from)
        ],
    )
    .map_err(|e| format!("Failed to insert usage log: {}", e))?;

    tx.commit()
        .map_err(|e| format!("Failed to commit transaction: {}", e))?;

    Ok(())
}

/// 检查 Token 是否有效 (包含过期时间检查和 IP 限制检查)
/// 返回: (是否有效, 拒绝原因)
pub fn validate_token(token_str: &str, ip: &str) -> Result<(bool, Option<String>), String> {
    let token_opt = get_token_by_value(token_str)?;

    if let Some(token) = token_opt {
        if !token.enabled {
            return Ok((false, Some("This token is disabled".into())));
        }
        // 1. 检查过期时间
        if token.expires_type != "never" {
            if let Some(expires_at) = token.expires_at {
                if expires_at < Utc::now().timestamp() {
                    return Ok((
                        false,
                        Some(
                            "Your token has expired. Please contact the administrator to renew it."
                                .to_string(),
                        ),
                    ));
                }
            }
        }

        // 2. 检查 IP 限制
        if token.max_ips > 0 {
            let conn = connect_db()?;

            // 检查当前 IP 是否已绑定
            let is_bound: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM token_ip_bindings WHERE token_id = ?1 AND ip_address = ?2)",
                params![token.id, ip],
                |row| row.get(0)
            ).unwrap_or(false);

            if !is_bound {
                // 如果未绑定，检查是否达到上限
                let current_ip_count: i32 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM token_ip_bindings WHERE token_id = ?1",
                        params![token.id],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);

                if current_ip_count >= token.max_ips {
                    return Ok((false, Some(format!("IP limit reached ({}/{}). Please contact the administrator to increase the limit.", current_ip_count, token.max_ips))));
                }
            }
        }

        // 3. 检查宵禁时间 (Curfew)
        // 逻辑：如果当前北京时间在 start 和 end 之间，则拒绝
        // 格式：HH:MM
        // 使用固定 UTC+8 (北京时间)，不依赖服务器本地时区
        if let (Some(start_str), Some(end_str)) = (&token.curfew_start, &token.curfew_end) {
            if !start_str.is_empty() && !end_str.is_empty() {
                let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap();
                let now_beijing = Utc::now().with_timezone(&beijing_offset);
                let current_time_str =
                    format!("{:02}:{:02}", now_beijing.hour(), now_beijing.minute());

                // 跨午夜处理: start > end (e.g. 23:00 to 06:00)
                // 正常: start < end (e.g. 09:00 to 18:00)
                let is_curfew = if start_str > end_str {
                    current_time_str >= *start_str || current_time_str < *end_str
                } else {
                    current_time_str >= *start_str && current_time_str < *end_str
                };

                if is_curfew {
                    return Ok((false, Some(format!("Service is not available between {} and {} Beijing Time (Curfew enabled). Current Beijing time: {}", start_str, end_str, current_time_str))));
                }
            }
        }

        // 一切正常，Token 有效
        Ok((true, None))
    } else {
        Ok((
            false,
            Some("Invalid token. Please check your API key.".to_string()),
        ))
    }
}

/// 获取 IP 关联的用户名 (用于 IP 管理页面)
/// 返回最近一次使用该 IP 的 Token 所属的用户名
pub fn get_username_for_ip(ip: &str) -> Result<Option<String>, String> {
    let conn = connect_db()?;
    let result: Option<String> = conn
        .query_row(
            "SELECT t.username
         FROM token_ip_bindings b 
         JOIN user_tokens t ON b.token_id = t.id 
         WHERE b.ip_address = ?1 
         ORDER BY b.last_seen_at DESC 
         LIMIT 1",
            params![ip],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("Failed to query username by ip: {}", e))?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_token_usage_logs_cache_pagination_and_calendar_boundaries() {
        init_db().unwrap();
        let a = create_token(
            "calendar-a".into(),
            "never".into(),
            None,
            0,
            None,
            None,
            None,
            0,
            0,
            vec![],
        )
        .unwrap();
        let b = create_token(
            "calendar-b".into(),
            "never".into(),
            None,
            0,
            None,
            None,
            None,
            0,
            0,
            vec![],
        )
        .unwrap();
        let conn = connect_db().unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-14T12:00:00+08:00")
            .unwrap()
            .timestamp();
        // Sunday 23:59:59 and Monday 00:00:00 belong to different UTC+8 weeks.
        for (stamp, cached) in [
            ("2026-10-11T23:59:59+08:00", None),
            ("2026-10-12T00:00:00+08:00", Some(0)),
            ("2026-10-14T00:00:00+08:00", Some(30)),
        ] {
            record_token_usage_with_cache(
                &a.id,
                "private-ip",
                "model-a",
                100,
                20,
                cached,
                200,
                None,
            )
            .unwrap();
            conn.execute("UPDATE token_usage_logs SET request_time = ?1 WHERE token_id = ?2 AND cached_tokens IS ?3", params![chrono::DateTime::parse_from_rfc3339(stamp).unwrap().timestamp(), &a.id, cached.map(i64::from)]).unwrap();
        }
        record_token_usage_with_cache(
            &b.id,
            "other-ip",
            "other-model",
            99999,
            99999,
            Some(99999),
            200,
            None,
        )
        .unwrap();
        let logs = usage_logs_for_token(&a.id, 1).unwrap();
        assert_eq!(logs.total, 3);
        assert_eq!(logs.items.len(), 3);
        assert_eq!(logs.items[0].cached_tokens, Some(30));
        assert_eq!(logs.items[1].cached_tokens, Some(0));
        assert_eq!(logs.items[2].cached_tokens, None);
        assert!(usage_logs_for_token(&a.id, 2).unwrap().items.is_empty());
        let start = logs.items[1].request_time;
        let end = logs.items[0].request_time;
        let filtered = filtered_usage_logs_for_token(&a.id, 1, Some(start), Some(end)).unwrap();
        assert_eq!(filtered.total, 2);
        assert_eq!(filtered.items.len(), 2);
        assert_eq!(
            filtered_usage_logs_for_token(&a.id, 1, Some(end), Some(end))
                .unwrap()
                .total,
            1
        );
        assert_eq!(
            filtered_usage_logs_for_token(&a.id, 1, None, Some(start - 1))
                .unwrap()
                .total,
            1
        );
        assert_eq!(
            filtered_usage_logs_for_token(&a.id, 1, Some(end + 1), None)
                .unwrap()
                .total,
            0
        );
        assert!(
            filtered_usage_logs_for_token(&a.id, 2, Some(start), Some(end))
                .unwrap()
                .items
                .is_empty()
        );
        let public = serde_json::to_string(&logs).unwrap();
        assert!(
            !public.contains("private-ip")
                && !public.contains(&a.id)
                && !public.contains("other-model")
        );
        let daily = period_usage_for_token(&a.id, false, now).unwrap();
        assert_eq!(daily.len(), 30);
        assert_eq!(daily[0].start_date, "2026-10-14");
        assert_eq!(daily[0].input_tokens, 100);
        assert_eq!(daily[1].requests, 0);
        let weekly = period_usage_for_token(&a.id, true, now).unwrap();
        assert_eq!(weekly.len(), 12);
        assert_eq!(weekly[0].start_date, "2026-10-12");
        assert_eq!(weekly[0].end_date, "2026-10-18");
        assert_eq!(weekly[0].requests, 2);
        assert_eq!(weekly[0].cached_tokens, Some(30));
        assert_eq!(weekly[1].requests, 1);
        assert_eq!(weekly[1].cached_tokens, None);
        assert_eq!(
            get_token_by_id(&a.id).unwrap().unwrap().total_tokens_used,
            360
        ); // cache is already included in input
        for _ in 0..20 {
            record_token_usage_and_ip(&a.id, "private-ip", "model-a", 1, 0, 200, None).unwrap();
        }
        let first = usage_logs_for_token(&a.id, 1).unwrap();
        let second = usage_logs_for_token(&a.id, 2).unwrap();
        assert_eq!(first.items.len(), 20);
        assert_eq!(second.items.len(), 3);
        assert!(!first
            .items
            .iter()
            .any(|r| second.items.iter().any(|s| s.id == r.id)));
    }

    #[test]
    fn test_create_and_query_token() {
        let _ = init_db(); // Ensure DB is initialized

        // Use a random username to avoid collisions in existing DB runs during dev
        let username = format!("TestUser_{}", Uuid::new_v4());
        let token_res = create_token(
            username.clone(),
            "day".to_string(),
            Some("Test token".to_string()),
            0,
            None,
            None,
            None,
            0,
            0,
            Vec::new(),
        );
        assert!(token_res.is_ok());

        let token = token_res.unwrap();
        assert_eq!(token.username, username);
        assert!(token.token.starts_with("sk-"));

        let fetched = get_token_by_id(&token.id);
        assert!(fetched.is_ok());
        assert_eq!(fetched.unwrap().unwrap().username, username);
    }

    #[test]
    fn test_never_expire_token_validation() {
        let _ = init_db();
        let username = format!("NeverExpireUser_{}", Uuid::new_v4());
        let token_res = create_token(
            username.clone(),
            "never".to_string(),
            Some("Never expire test token".to_string()),
            0,
            None,
            None,
            None,
            0,
            0,
            Vec::new(),
        );
        assert!(token_res.is_ok());
        let token = token_res.unwrap();
        let (valid, reason) =
            validate_token(&token.token, "127.0.0.1").expect("validation must succeed");
        assert!(
            valid,
            "Token with expires_type never must be valid, reason: {:?}",
            reason
        );
    }
    #[test]
    fn user_token_migration_preserves_legacy_usage_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE user_tokens (
            id TEXT PRIMARY KEY, token TEXT UNIQUE NOT NULL, username TEXT NOT NULL,
            description TEXT, enabled BOOLEAN NOT NULL DEFAULT 1, expires_type TEXT NOT NULL,
            expires_at INTEGER, max_ips INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL, last_used_at INTEGER, total_requests INTEGER NOT NULL DEFAULT 0,
            total_tokens_used INTEGER NOT NULL DEFAULT 0, curfew_start TEXT, curfew_end TEXT);
            INSERT INTO user_tokens (id, token, username, expires_type, created_at, updated_at, total_tokens_used)
            VALUES ('legacy-id', 'test-key', 'legacy', 'never', 1, 1, 12345);").unwrap();
        init_db_connection(&mut conn).unwrap();
        init_db_connection(&mut conn).unwrap();
        let values: (String, i64, i32, i64) = conn.query_row(
            "SELECT token, total_tokens_used, max_concurrency, token_quota FROM user_tokens WHERE id = 'legacy-id'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
        assert_eq!(values, ("test-key".into(), 12345, 0, 0));
    }

    #[test]
    fn user_token_limits_persist_edit_clear_and_validate() {
        init_db().unwrap();
        let t = create_token(
            "settings-test".into(),
            "never".into(),
            None,
            0,
            None,
            None,
            None,
            2,
            100,
            Vec::new(),
        )
        .unwrap();
        record_token_usage_and_ip(&t.id, "127.0.0.1", "test", 20, 30, 200, None).unwrap();
        update_token(
            &t.id,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(1),
            Some(40),
            None,
            None,
        )
        .unwrap();
        let edited = get_token_by_value(&t.token).unwrap().unwrap();
        assert_eq!(
            (
                edited.max_concurrency,
                edited.token_quota,
                edited.total_tokens_used
            ),
            (1, 40, 50)
        );
        update_token(
            &t.id,
            Some("renamed".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(get_token_by_id(&t.id).unwrap().unwrap().token_quota, 40);
        assert!(update_token(
            &t.id,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(-1),
            None,
            None,
            None,
        )
        .is_err());
        assert!(validate_limits(0, -1).is_err());
        assert!(validate_limits(0, 9_007_199_254_740_992).is_err());
        update_token(
            &t.id,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(0),
            Some(0),
            None,
            None,
        )
        .unwrap();
        renew_token(&t.id, "day").unwrap();
        let cleared = get_token_by_id(&t.id).unwrap().unwrap();
        assert_eq!(
            (
                cleared.max_concurrency,
                cleared.token_quota,
                cleared.total_tokens_used
            ),
            (0, 0, 50)
        );
    }

    #[test]
    fn user_token_concurrent_usage_is_not_lost() {
        init_db().unwrap();
        let t = create_token(
            "settlement-test".into(),
            "never".into(),
            None,
            0,
            None,
            None,
            None,
            0,
            100,
            Vec::new(),
        )
        .unwrap();
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let id = t.id.clone();
                std::thread::spawn(move || {
                    record_token_usage_and_ip(&id, "127.0.0.1", "test", 10, 5, 200, None).unwrap()
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let t = get_token_by_id(&t.id).unwrap().unwrap();
        assert_eq!((t.total_requests, t.total_tokens_used), (16, 240));
    }
    #[test]
    fn user_token_model_policy_persists_edits_and_preserves_usage() {
        init_db().unwrap();
        let t = create_token(
            "models-test".into(),
            "never".into(),
            None,
            0,
            None,
            None,
            None,
            2,
            100,
            vec![
                " model-b ".into(),
                "models/model-a".into(),
                "model-a".into(),
            ],
        )
        .unwrap();
        assert_eq!(t.allowed_models, vec!["model-a", "model-b"]);
        record_token_usage_and_ip(&t.id, "local", "model-a", 10, 5, 200, None).unwrap();
        update_token(
            &t.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(vec!["model-c".into()]),
            None,
        )
        .unwrap();
        let read = get_token_by_value(&t.token).unwrap().unwrap();
        assert_eq!(read.allowed_models, vec!["model-c"]);
        assert_eq!(read.total_tokens_used, 15);
        update_token(
            &t.id,
            Some("renamed".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            get_token_by_id(&t.id).unwrap().unwrap().allowed_models,
            vec!["model-c"]
        );
        update_token(
            &t.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(vec![]),
            None,
        )
        .unwrap();
        assert!(get_token_by_id(&t.id)
            .unwrap()
            .unwrap()
            .allowed_models
            .is_empty());
        assert!(normalize_allowed_models(vec!["*".into()]).is_err());
        assert!(normalize_allowed_models(vec!["  ".into()]).is_err());
    }

    #[test]
    fn user_token_expiry_resolution_rejects_ambiguous_choices() {
        let now = 1_000_000;
        assert_eq!(resolve_expires_at("day", None, now), Ok(Some(now + 86_400)));
        assert_eq!(
            resolve_expires_at("week", None, now),
            Ok(Some(now + 7 * 86_400))
        );
        assert_eq!(
            resolve_expires_at("month", None, now),
            Ok(Some(now + 30 * 86_400))
        );
        assert_eq!(resolve_expires_at("never", Some(now + 5), now), Ok(None));
        assert_eq!(
            resolve_expires_at("custom", Some(now + 5), now),
            Ok(Some(now + 5))
        );
        assert!(resolve_expires_at("custom", Some(now), now).is_err());
        assert!(resolve_expires_at("custom", None, now).is_err());
        assert!(resolve_expires_at("year", None, now).is_err());
    }

    #[test]
    fn user_token_expiry_edit_keeps_or_replaces_expiry() {
        init_db().unwrap();
        let t = create_token(
            format!("expiry-edit-{}", Uuid::new_v4()),
            "day".into(),
            None,
            0,
            None,
            None,
            None,
            0,
            0,
            vec![],
        )
        .unwrap();
        let original = t.expires_at.unwrap();
        let edit = |expires| {
            update_token(
                &t.id, None, None, None, None, None, None, None, None, None, expires,
            )
        };

        // Editing other fields leaves the expiry untouched.
        update_token(
            &t.id,
            Some("renamed".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let kept = get_token_by_id(&t.id).unwrap().unwrap();
        assert_eq!(
            (kept.expires_type.as_str(), kept.expires_at),
            ("day", Some(original))
        );

        let custom = Utc::now().timestamp() + 3_600;
        edit(Some(("custom".into(), Some(custom)))).unwrap();
        let edited = get_token_by_id(&t.id).unwrap().unwrap();
        assert_eq!(
            (edited.expires_type.as_str(), edited.expires_at),
            ("custom", Some(custom))
        );

        // Rejected edits change nothing, including the other fields in the same request.
        assert!(update_token(
            &t.id,
            Some("ignored".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(("custom".into(), None))
        )
        .is_err());
        assert!(edit(Some(("custom".into(), Some(1)))).is_err());
        let unchanged = get_token_by_id(&t.id).unwrap().unwrap();
        assert_eq!(
            (unchanged.username.as_str(), unchanged.expires_at),
            ("renamed", Some(custom))
        );

        edit(Some(("never".into(), None))).unwrap();
        let never = get_token_by_id(&t.id).unwrap().unwrap();
        assert_eq!(
            (never.expires_type.as_str(), never.expires_at),
            ("never", None)
        );
    }

    #[test]
    fn user_token_custom_renewal_never_clears_expiry() {
        init_db().unwrap();
        let expires_at = Utc::now().timestamp() + 60;
        let t = create_token(
            format!("expiry-renew-{}", Uuid::new_v4()),
            "custom".into(),
            None,
            0,
            None,
            None,
            Some(expires_at),
            0,
            0,
            vec![],
        )
        .unwrap();
        assert!(renew_token(&t.id, "custom").is_err());
        assert_eq!(
            get_token_by_id(&t.id).unwrap().unwrap().expires_at,
            Some(expires_at)
        );
        renew_token(&t.id, "week").unwrap();
        let renewed = get_token_by_id(&t.id).unwrap().unwrap();
        assert_eq!(renewed.expires_type, "week");
        assert!(renewed.expires_at.unwrap() > expires_at);
    }
}
