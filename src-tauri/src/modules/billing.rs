//! Balance billing for user keys.
//!
//! Money is stored as integer micro-yuan (¥0.000001). Each model is billed in
//! exactly one way — a fixed price per call or a price per million tokens —
//! chosen by the most specific matching wildcard rule. Every user key is billed,
//! except for its explicit free-model list. Unpriced, non-free models are denied.

use crate::modules::user_token_db::connect_db;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Token prices are quoted per million tokens.
const TOKENS_PER_PRICE: i128 = 1_000_000;
/// Upper bound for any single price or balance (¥1,000,000,000).
pub const MAX_MICROS: i64 = 1_000_000_000 * 1_000_000;
const MAX_PATTERN_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Price {
    PerCall {
        call_price_micros: i64,
    },
    PerToken {
        input_price_micros: i64,
        output_price_micros: i64,
        /// Price for cache-hit input tokens; `None` charges them as normal input.
        #[serde(default)]
        cached_price_micros: Option<i64>,
    },
}

impl Price {
    pub fn mode(&self) -> &'static str {
        match self {
            Self::PerCall { .. } => "per_call",
            Self::PerToken { .. } => "per_token",
        }
    }

    /// Charge for one finished request. `input_tokens` includes cache hits.
    pub fn cost(
        &self,
        input_tokens: i64,
        output_tokens: i64,
        cached_tokens: Option<i64>,
        status: u16,
    ) -> i64 {
        match *self {
            // A call is charged only when the client got a successful response.
            Self::PerCall { call_price_micros } => {
                if (200..400).contains(&status) {
                    call_price_micros
                } else {
                    0
                }
            }
            Self::PerToken {
                input_price_micros,
                output_price_micros,
                cached_price_micros,
            } => {
                let input = i128::from(input_tokens.max(0));
                let cached = i128::from(cached_tokens.unwrap_or(0)).clamp(0, input);
                let total = (input - cached) * i128::from(input_price_micros)
                    + cached * i128::from(cached_price_micros.unwrap_or(input_price_micros))
                    + i128::from(output_tokens.max(0)) * i128::from(output_price_micros);
                // Round up so a non-zero usage is never free.
                let micros = (total + TOKENS_PER_PRICE - 1) / TOKENS_PER_PRICE;
                micros.min(i128::from(i64::MAX)) as i64
            }
        }
    }

    /// Whether a balance may start a request at this price. A per-call price is
    /// known in advance; per-token usage is settled afterwards, so any positive
    /// balance may start it and the final charge can leave the balance negative.
    pub fn admits(&self, balance_micros: i64) -> bool {
        match *self {
            Self::PerCall { call_price_micros } => balance_micros >= call_price_micros,
            Self::PerToken { .. } => balance_micros > 0,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let amounts = match *self {
            Self::PerCall { call_price_micros } => vec![Some(call_price_micros)],
            Self::PerToken {
                input_price_micros,
                output_price_micros,
                cached_price_micros,
            } => vec![
                Some(input_price_micros),
                Some(output_price_micros),
                cached_price_micros,
            ],
        };
        if amounts
            .into_iter()
            .flatten()
            .any(|value| !(0..=MAX_MICROS).contains(&value))
        {
            return Err("Prices must be between 0 and 1,000,000,000".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Exact model ID or wildcard pattern, e.g. `gemini-*-flash*`.
    pub pattern: String,
    #[serde(flatten)]
    pub price: Price,
}

/// Immutable admission decision carried through streaming and final accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BillingPolicy {
    Free,
    Priced(Price),
}

pub fn is_free_model(free_models: &[String], model: &str) -> bool {
    let id = model.strip_prefix("models/").unwrap_or(model);
    free_models
        .iter()
        .any(|free| free.strip_prefix("models/").unwrap_or(free) == id)
}

pub fn policy_for_model(
    free_models: &[String],
    rules: &[ModelPrice],
    model: &str,
) -> Option<BillingPolicy> {
    if is_free_model(free_models, model) {
        Some(BillingPolicy::Free)
    } else {
        match_price(rules, model).map(|rule| BillingPolicy::Priced(rule.price.clone()))
    }
}

/// Called inside the user-token migration transaction.
pub(crate) fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_prices (
            pattern TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('per_call', 'per_token')),
            call_price_micros INTEGER CHECK(call_price_micros >= 0),
            input_price_micros INTEGER CHECK(input_price_micros >= 0),
            output_price_micros INTEGER CHECK(output_price_micros >= 0),
            cached_price_micros INTEGER CHECK(cached_price_micros >= 0),
            updated_at INTEGER NOT NULL,
            CHECK((mode = 'per_call' AND call_price_micros IS NOT NULL
                    AND input_price_micros IS NULL AND output_price_micros IS NULL
                    AND cached_price_micros IS NULL)
                OR (mode = 'per_token' AND call_price_micros IS NULL
                    AND input_price_micros IS NOT NULL AND output_price_micros IS NOT NULL))
        );
        CREATE TABLE IF NOT EXISTS balance_ledger (
            id TEXT PRIMARY KEY,
            token_id TEXT NOT NULL,
            delta_micros INTEGER NOT NULL,
            balance_after INTEGER,
            reason TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY(token_id) REFERENCES user_tokens(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_balance_ledger_token
            ON balance_ledger(token_id, created_at DESC);",
    )
    .map_err(|e| format!("Failed to migrate billing tables: {e}"))
}

// --- Price rules ---

fn literal_len(pattern: &str) -> usize {
    pattern.chars().filter(|c| *c != '*').count()
}

/// Exact IDs beat wildcards; among wildcards the one with more literal
/// characters wins, so `gemini-3-pro-*` overrides `gemini-*`.
pub fn match_price<'a>(rules: &'a [ModelPrice], model: &str) -> Option<&'a ModelPrice> {
    use crate::proxy::common::model_mapping::wildcard_match;
    let id = model.strip_prefix("models/").unwrap_or(model);
    let lower = id.to_lowercase();
    rules
        .iter()
        // Same case handling as model routing: exact case first, then case-insensitive.
        .filter(|rule| {
            wildcard_match(&rule.pattern, id)
                || wildcard_match(&rule.pattern.to_lowercase(), &lower)
        })
        .max_by(|a, b| {
            let rank =
                |rule: &ModelPrice| (!rule.pattern.contains('*'), literal_len(&rule.pattern));
            rank(a)
                .cmp(&rank(b))
                .then_with(|| b.pattern.cmp(&a.pattern))
        })
}

pub(crate) fn load_prices_with(conn: &Connection) -> Result<Vec<ModelPrice>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT pattern, mode, call_price_micros, input_price_micros, output_price_micros,
             cached_price_micros FROM model_prices ORDER BY pattern",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            let mode: String = row.get(1)?;
            let price = if mode == "per_call" {
                Price::PerCall {
                    call_price_micros: row.get(2)?,
                }
            } else {
                Price::PerToken {
                    input_price_micros: row.get(3)?,
                    output_price_micros: row.get(4)?,
                    cached_price_micros: row.get(5)?,
                }
            };
            Ok(ModelPrice {
                pattern: row.get(0)?,
                price,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

pub fn load_prices() -> Result<Vec<ModelPrice>, String> {
    load_prices_with(&connect_db()?)
}

/// Replaces the whole price list atomically.
pub fn save_prices(rules: Vec<ModelPrice>) -> Result<Vec<ModelPrice>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut normalized = Vec::with_capacity(rules.len());
    for mut rule in rules {
        rule.pattern = rule.pattern.trim().to_string();
        if rule.pattern.is_empty()
            || rule.pattern.len() > MAX_PATTERN_LEN
            || rule.pattern.chars().any(char::is_whitespace)
        {
            return Err(format!("Invalid model pattern: {:?}", rule.pattern));
        }
        if !seen.insert(rule.pattern.clone()) {
            return Err(format!("Duplicate model pattern: {}", rule.pattern));
        }
        rule.price.validate()?;
        normalized.push(rule);
    }

    let mut conn = connect_db()?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp();
    tx.execute("DELETE FROM model_prices", [])
        .map_err(|e| e.to_string())?;
    for rule in &normalized {
        let (call, input, output, cached) = match rule.price {
            Price::PerCall { call_price_micros } => (Some(call_price_micros), None, None, None),
            Price::PerToken {
                input_price_micros,
                output_price_micros,
                cached_price_micros,
            } => (
                None,
                Some(input_price_micros),
                Some(output_price_micros),
                cached_price_micros,
            ),
        };
        tx.execute(
            "INSERT INTO model_prices (pattern, mode, call_price_micros, input_price_micros,
             output_price_micros, cached_price_micros, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                rule.pattern,
                rule.price.mode(),
                call,
                input,
                output,
                cached,
                now
            ],
        )
        .map_err(|e| format!("Failed to save price for {}: {e}", rule.pattern))?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    load_prices()
}

// --- Balances ---

pub fn validate_balance(balance_micros: Option<i64>) -> Result<(), String> {
    match balance_micros {
        Some(value) if !(0..=MAX_MICROS).contains(&value) => {
            Err("Balance must be between 0 and 1,000,000,000".into())
        }
        _ => Ok(()),
    }
}

fn record_ledger(
    conn: &Connection,
    token_id: &str,
    delta: i64,
    balance_after: Option<i64>,
    reason: &str,
    now: i64,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO balance_ledger (id, token_id, delta_micros, balance_after, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            uuid::Uuid::new_v4().to_string(),
            token_id,
            delta,
            balance_after,
            reason,
            now
        ],
    )
    .map_err(|e| format!("Failed to record balance change: {e}"))?;
    Ok(())
}

/// Records the opening balance of a newly created billed key.
pub(crate) fn record_opening_balance(
    conn: &Connection,
    token_id: &str,
    balance_micros: Option<i64>,
    now: i64,
) -> Result<(), String> {
    match balance_micros {
        Some(balance) if balance != 0 => {
            record_ledger(conn, token_id, balance, Some(balance), "admin", now)
        }
        _ => Ok(()),
    }
}

/// Current balance of a key, inside the caller's transaction.
pub(crate) fn current_balance(conn: &Connection, token_id: &str) -> Result<Option<i64>, String> {
    conn.query_row(
        "SELECT balance_micros FROM user_tokens WHERE id = ?1",
        params![token_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "Token not found".to_string())
}

/// Records an administrator balance edit; callers reject disabling billing.
pub(crate) fn record_admin_change(
    conn: &Connection,
    token_id: &str,
    previous: Option<i64>,
    target: Option<i64>,
    now: i64,
) -> Result<(), String> {
    if previous == target {
        return Ok(());
    }
    let delta = target.unwrap_or(0).saturating_sub(previous.unwrap_or(0));
    record_ledger(conn, token_id, delta, target, "admin", now)
}

/// Settle the admission-time policy in the same transaction as usage. No policy
/// means a non-inference/denied request; configuration changes never reprice it.
pub(crate) fn settle(
    conn: &Connection,
    token_id: &str,
    policy: Option<&BillingPolicy>,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: Option<i64>,
    status: u16,
) -> Result<Option<(i64, &'static str)>, String> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    // A deleted key cannot be charged; do not label its final usage as paid.
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM user_tokens WHERE id = ?1)",
            params![token_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if !exists {
        return Ok(None);
    }
    let (cost, mode) = match policy {
        BillingPolicy::Free => (0, "free"),
        BillingPolicy::Priced(price) => (
            price.cost(input_tokens, output_tokens, cached_tokens, status),
            price.mode(),
        ),
    };
    if cost > 0 {
        conn.execute(
            "UPDATE user_tokens SET balance_micros = balance_micros - ?1 WHERE id = ?2",
            params![cost, token_id],
        )
        .map_err(|e| format!("Failed to charge balance: {e}"))?;
    }
    Ok(Some((cost, mode)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_of_deleted_key_has_no_charge() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE user_tokens (id TEXT PRIMARY KEY, balance_micros INTEGER);
            INSERT INTO user_tokens VALUES ('deleted', 100);
            DELETE FROM user_tokens WHERE id='deleted';",
        )
        .unwrap();
        for policy in [
            BillingPolicy::Free,
            BillingPolicy::Priced(Price::PerCall {
                call_price_micros: 10,
            }),
        ] {
            assert_eq!(
                settle(&conn, "deleted", Some(&policy), 20, 10, None, 200).unwrap(),
                None
            );
        }
    }

    fn per_token(input: i64, output: i64, cached: Option<i64>) -> Price {
        Price::PerToken {
            input_price_micros: input,
            output_price_micros: output,
            cached_price_micros: cached,
        }
    }

    fn rule(pattern: &str, price: Price) -> ModelPrice {
        ModelPrice {
            pattern: pattern.into(),
            price,
        }
    }

    #[test]
    fn billing_cost_per_call_and_per_token() {
        let call = Price::PerCall {
            call_price_micros: 100_000,
        };
        assert_eq!(call.cost(5_000, 5_000, None, 200), 100_000);
        assert_eq!(call.cost(0, 0, None, 429), 0, "failed calls are free");
        assert_eq!(call.cost(0, 0, None, 0), 0);

        // ¥2 / M input, ¥8 / M output, cache hits ¥0.5 / M.
        let tokens = per_token(2_000_000, 8_000_000, Some(500_000));
        // 1M uncached input + 1M cached + 0.5M output = 2 + 0.5 + 4 yuan.
        assert_eq!(
            tokens.cost(2_000_000, 500_000, Some(1_000_000), 200),
            6_500_000
        );
        // Usage is charged even when the response failed after producing tokens.
        assert_eq!(tokens.cost(10, 0, None, 500), 20);
        // Sub-micro usage rounds up; zero usage is free.
        assert_eq!(per_token(1, 1, None).cost(1, 0, None, 200), 1);
        assert_eq!(tokens.cost(0, 0, None, 200), 0);
        // Missing cache price falls back to the input price; bad cache data is clamped.
        assert_eq!(per_token(1_000_000, 0, None).cost(10, 0, Some(10), 200), 10);
        assert_eq!(
            per_token(1_000_000, 0, Some(0)).cost(10, 0, Some(99), 200),
            0
        );
    }

    #[test]
    fn billing_admission_depends_on_mode() {
        let call = Price::PerCall {
            call_price_micros: 100,
        };
        assert!(call.admits(100) && !call.admits(99));
        let tokens = per_token(1, 1, None);
        assert!(tokens.admits(1) && !tokens.admits(0) && !tokens.admits(-5));
    }

    #[test]
    fn billing_most_specific_rule_wins() {
        let rules = vec![
            rule("gemini-*", per_token(1, 1, None)),
            rule("gemini-3-pro-*", per_token(2, 2, None)),
            rule(
                "gemini-3-pro-image",
                Price::PerCall {
                    call_price_micros: 3,
                },
            ),
        ];
        let pick = |model: &str| match_price(&rules, model).map(|r| r.pattern.as_str());
        assert_eq!(pick("gemini-3-pro-image"), Some("gemini-3-pro-image"));
        assert_eq!(pick("models/gemini-3-pro-high"), Some("gemini-3-pro-*"));
        assert_eq!(pick("gemini-3-flash"), Some("gemini-*"));
        assert_eq!(pick("claude-opus-4-5"), None, "unpriced models are free");
    }

    #[test]
    fn billing_price_json_carries_exactly_one_mode() {
        let parsed: ModelPrice = serde_json::from_value(serde_json::json!({
            "pattern": "x", "mode": "per_call", "call_price_micros": 5
        }))
        .unwrap();
        assert_eq!(
            parsed.price,
            Price::PerCall {
                call_price_micros: 5
            }
        );
        assert!(serde_json::from_value::<ModelPrice>(serde_json::json!({
            "pattern": "x", "mode": "per_token", "call_price_micros": 5
        }))
        .is_err());
    }
}
