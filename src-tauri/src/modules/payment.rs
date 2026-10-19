//! Balance top-up orders paid through an EPay-compatible gateway.
//!
//! An order is credited exactly once: the paid transition, the balance credit and
//! a ledger row keyed by the order number commit together, and a unique index on
//! that ledger key rejects any second credit at the database level.

use crate::modules::user_token_db::connect_db;
use chrono::Utc;
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// One yuan of payment buys one yuan of balance.
pub const MICROS_PER_CENT: i64 = 10_000;
/// Pending orders older than this are closed once the gateway confirms they are
/// unpaid; a late payment still credits them.
pub const ORDER_TTL_SECS: i64 = 24 * 3600;
/// Pending orders the gateway never answered for are given up after this.
pub const PENDING_GIVE_UP_SECS: i64 = 7 * 24 * 3600;
/// Unpaid orders one key may hold at a time.
pub const MAX_PENDING_PER_KEY: i64 = 5;

/// Called inside the user-token migration transaction, after the billing tables.
pub(crate) fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS payment_orders (
            out_trade_no TEXT PRIMARY KEY,
            token_id TEXT NOT NULL,
            user_id TEXT,
            amount_cents INTEGER NOT NULL CHECK(amount_cents > 0),
            credit_micros INTEGER NOT NULL CHECK(credit_micros > 0),
            pay_type TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'paid', 'closed')),
            trade_no TEXT UNIQUE,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            paid_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_payment_orders_token
            ON payment_orders(token_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_payment_orders_status
            ON payment_orders(status, created_at);",
    )
    .map_err(|e| format!("Failed to migrate payment orders: {e}"))?;
    let has_ref: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('balance_ledger') WHERE name = 'ref')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to inspect balance ledger: {e}"))?;
    if !has_ref {
        conn.execute("ALTER TABLE balance_ledger ADD COLUMN ref TEXT", [])
            .map_err(|e| format!("Failed to migrate balance ledger: {e}"))?;
    }
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_balance_ledger_ref
            ON balance_ledger(reason, ref) WHERE ref IS NOT NULL",
        [],
    )
    .map_err(|e| format!("Failed to index balance ledger: {e}"))?;
    Ok(())
}

/// What the order's owner sees; no token or user IDs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PublicOrder {
    pub out_trade_no: String,
    pub amount_cents: i64,
    pub credit_micros: i64,
    pub pay_type: String,
    pub status: String,
    pub created_at: i64,
    pub paid_at: Option<i64>,
}

fn order_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PublicOrder> {
    Ok(PublicOrder {
        out_trade_no: row.get("out_trade_no")?,
        amount_cents: row.get("amount_cents")?,
        credit_micros: row.get("credit_micros")?,
        pay_type: row.get("pay_type")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        paid_at: row.get("paid_at")?,
    })
}

/// Digits only (the gateway documents the field as numeric), 24 of 32 allowed:
/// a UTC timestamp for readability plus 10 random digits for uniqueness.
pub fn new_order_no(now: i64) -> String {
    let stamp = chrono::DateTime::from_timestamp(now, 0)
        .unwrap_or_else(Utc::now)
        .format("%Y%m%d%H%M%S");
    format!(
        "{stamp}{:010}",
        rand::thread_rng().gen_range(0..10_000_000_000u64)
    )
}

pub fn create_order(
    token_id: &str,
    user_id: Option<&str>,
    amount_cents: i64,
    pay_type: &str,
    now: i64,
) -> Result<PublicOrder, String> {
    let order = PublicOrder {
        out_trade_no: new_order_no(now),
        amount_cents,
        credit_micros: amount_cents
            .checked_mul(MICROS_PER_CENT)
            .ok_or("Amount too large")?,
        pay_type: pay_type.to_string(),
        status: "pending".into(),
        created_at: now,
        paid_at: None,
    };
    connect_db()?
        .execute(
            "INSERT INTO payment_orders (out_trade_no, token_id, user_id, amount_cents,
             credit_micros, pay_type, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?7)",
            params![
                order.out_trade_no,
                token_id,
                user_id,
                order.amount_cents,
                order.credit_micros,
                order.pay_type,
                now
            ],
        )
        .map_err(|e| format!("Failed to create payment order: {e}"))?;
    Ok(order)
}

/// An order, only if it belongs to `token_id`.
pub fn order_for_token(token_id: &str, out_trade_no: &str) -> Result<Option<PublicOrder>, String> {
    connect_db()?
        .query_row(
            "SELECT * FROM payment_orders WHERE out_trade_no = ?1 AND token_id = ?2",
            params![out_trade_no, token_id],
            order_from_row,
        )
        .optional()
        .map_err(|e| e.to_string())
}

pub fn recent_orders(token_id: &str, limit: u32) -> Result<Vec<PublicOrder>, String> {
    let conn = connect_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT * FROM payment_orders WHERE token_id = ?1
             ORDER BY created_at DESC, out_trade_no DESC LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![token_id, limit], order_from_row)
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

/// Unpaid orders created in `[since, until]` with their creation time, least
/// recently checked first so a backlog is worked through in turn.
pub fn pending_orders(since: i64, until: i64, limit: u32) -> Result<Vec<(String, i64)>, String> {
    let conn = connect_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT out_trade_no, created_at FROM payment_orders
             WHERE status = 'pending' AND created_at >= ?1 AND created_at <= ?2
             ORDER BY updated_at, created_at LIMIT ?3",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![since, until, limit], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

/// Records that a pending order was just checked with the gateway.
pub fn mark_checked(out_trade_no: &str, now: i64) -> Result<(), String> {
    connect_db()?
        .execute(
            "UPDATE payment_orders SET updated_at = ?2 WHERE out_trade_no = ?1 AND status = 'pending'",
            params![out_trade_no, now],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Unpaid orders a key created since `since`.
pub fn pending_count(token_id: &str, since: i64) -> Result<i64, String> {
    connect_db()?
        .query_row(
            "SELECT COUNT(*) FROM payment_orders WHERE token_id = ?1 AND status = 'pending' AND created_at >= ?2",
            params![token_id, since],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())
}

/// Closes one order the gateway reported as unpaid.
pub fn close_order(out_trade_no: &str, now: i64) -> Result<bool, String> {
    connect_db()?
        .execute(
            "UPDATE payment_orders SET status = 'closed', updated_at = ?2
             WHERE out_trade_no = ?1 AND status = 'pending'",
            params![out_trade_no, now],
        )
        .map(|changed| changed == 1)
        .map_err(|e| e.to_string())
}

/// Closes pending orders created before `before` without asking the gateway;
/// only for orders it never answered for. Closed orders can still be credited
/// if their payment arrives later.
pub fn close_stale(before: i64, now: i64) -> Result<usize, String> {
    connect_db()?
        .execute(
            "UPDATE payment_orders SET status = 'closed', updated_at = ?2
             WHERE status = 'pending' AND created_at < ?1",
            params![before, now],
        )
        .map_err(|e| e.to_string())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Credit {
    Credited {
        token_id: String,
        credit_micros: i64,
    },
    AlreadyPaid,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreditError {
    UnknownOrder,
    AmountMismatch {
        expected_cents: i64,
        paid_cents: i64,
    },
    Database(String),
}

impl std::fmt::Display for CreditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOrder => write!(f, "unknown order"),
            Self::AmountMismatch {
                expected_cents,
                paid_cents,
            } => write!(
                f,
                "paid {paid_cents} cents, order is {expected_cents} cents"
            ),
            Self::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

fn db(error: impl std::fmt::Display) -> CreditError {
    CreditError::Database(error.to_string())
}

/// Credits a verified payment. Idempotent: a repeated or concurrent call for the
/// same order returns `AlreadyPaid` without touching the balance.
pub fn credit_paid_order(
    out_trade_no: &str,
    trade_no: Option<&str>,
    paid_cents: i64,
    now: i64,
) -> Result<Credit, CreditError> {
    let mut conn = connect_db().map_err(db)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(db)?;
    let Some((token_id, amount_cents, credit_micros, status)) = tx
        .query_row(
            "SELECT token_id, amount_cents, credit_micros, status FROM payment_orders
             WHERE out_trade_no = ?1",
            params![out_trade_no],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(db)?
    else {
        return Err(CreditError::UnknownOrder);
    };
    if status == "paid" {
        return Ok(Credit::AlreadyPaid);
    }
    if paid_cents != amount_cents {
        return Err(CreditError::AmountMismatch {
            expected_cents: amount_cents,
            paid_cents,
        });
    }
    tx.execute(
        "UPDATE payment_orders SET status = 'paid', trade_no = COALESCE(?2, trade_no),
         paid_at = ?3, updated_at = ?3 WHERE out_trade_no = ?1 AND status <> 'paid'",
        params![out_trade_no, trade_no, now],
    )
    .map_err(db)?;
    // A payment turns billing on if an administrator switched it off meanwhile:
    // the money must land somewhere visible.
    let credited = tx
        .execute(
            "UPDATE user_tokens SET balance_micros = COALESCE(balance_micros, 0) + ?1,
             updated_at = ?2 WHERE id = ?3",
            params![credit_micros, now, token_id],
        )
        .map_err(db)?;
    if credited == 0 {
        // The key was deleted after ordering. Keep the order paid so the payment
        // is not retried forever; the administrator refunds it from the gateway.
        tracing::error!(
            "[Payment] order {out_trade_no} paid but its key no longer exists; refund it manually"
        );
    } else {
        let balance_after: Option<i64> = tx
            .query_row(
                "SELECT balance_micros FROM user_tokens WHERE id = ?1",
                params![token_id],
                |row| row.get(0),
            )
            .map_err(db)?;
        tx.execute(
            "INSERT INTO balance_ledger (id, token_id, delta_micros, balance_after, reason, ref, created_at)
             VALUES (?1, ?2, ?3, ?4, 'topup', ?5, ?6)",
            params![
                uuid::Uuid::new_v4().to_string(),
                token_id,
                credit_micros,
                balance_after,
                out_trade_no,
                now
            ],
        )
        .map_err(db)?;
        // Paying activates a registered key still waiting for activation; keys an
        // administrator disabled with the whole account stay off.
        tx.execute(
            "UPDATE user_tokens SET enabled = 1, updated_at = ?2
             WHERE id = ?1 AND enabled = 0 AND owner_user_id IS NOT NULL
             AND EXISTS (SELECT 1 FROM portal_users p WHERE p.id = owner_user_id AND p.status = 'active')",
            params![token_id, now],
        )
        .map_err(db)?;
    }
    tx.commit().map_err(db)?;
    Ok(Credit::Credited {
        token_id,
        credit_micros,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::user_token_db as tokens;

    fn billed_key(balance: Option<i64>) -> tokens::UserToken {
        tokens::init_db().unwrap();
        let key = tokens::create_token(
            format!("pay-{}", uuid::Uuid::new_v4().simple()),
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
        tokens::update_token_with_balance(
            &key.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(balance),
        )
        .unwrap();
        key
    }

    fn balance(id: &str) -> Option<i64> {
        tokens::get_token_by_id(id).unwrap().unwrap().balance_micros
    }

    #[test]
    fn payment_order_numbers_are_numeric_and_unique() {
        let a = new_order_no(1_760_000_000);
        let b = new_order_no(1_760_000_000);
        assert_eq!(a.len(), 24);
        assert!(a.bytes().all(|c| c.is_ascii_digit()));
        assert!(a.starts_with("2025"));
        assert_ne!(a, b);
    }

    #[test]
    fn payment_credit_is_exactly_once_and_checks_amount() {
        let key = billed_key(Some(1_000));
        let order = create_order(&key.id, None, 500, "alipay", 1_000).unwrap();
        assert_eq!(order.credit_micros, 5_000_000);
        // Gateway trade numbers are unique, and the test database outlives a run.
        let trade = format!("T{}", order.out_trade_no);
        assert_eq!(
            credit_paid_order(&order.out_trade_no, Some(trade.as_str()), 499, 1_001),
            Err(CreditError::AmountMismatch {
                expected_cents: 500,
                paid_cents: 499
            })
        );
        assert_eq!(balance(&key.id), Some(1_000), "a mismatch credits nothing");
        assert_eq!(
            credit_paid_order(&order.out_trade_no, Some(trade.as_str()), 500, 1_002),
            Ok(Credit::Credited {
                token_id: key.id.clone(),
                credit_micros: 5_000_000
            })
        );
        assert_eq!(
            credit_paid_order(&order.out_trade_no, Some(trade.as_str()), 500, 1_003),
            Ok(Credit::AlreadyPaid)
        );
        assert_eq!(balance(&key.id), Some(5_001_000));
        let stored = order_for_token(&key.id, &order.out_trade_no)
            .unwrap()
            .unwrap();
        assert_eq!(
            (stored.status.as_str(), stored.paid_at),
            ("paid", Some(1_002))
        );
        assert_eq!(
            order_for_token("someone-else", &order.out_trade_no).unwrap(),
            None
        );
        assert_eq!(
            credit_paid_order("no-such-order", None, 500, 1_004),
            Err(CreditError::UnknownOrder)
        );
        // The ledger refuses a second top-up row for the same order outright.
        let duplicate = connect_db().unwrap().execute(
            "INSERT INTO balance_ledger (id, token_id, delta_micros, reason, ref, created_at)
             VALUES ('dup', ?1, 1, 'topup', ?2, 0)",
            params![key.id, order.out_trade_no],
        );
        assert!(duplicate.is_err());
    }

    #[test]
    fn payment_concurrent_credits_add_once() {
        let key = billed_key(Some(0));
        let order = create_order(&key.id, None, 1_234, "alipay", 2_000).unwrap();
        let trade = format!("T{}", order.out_trade_no);
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (no, trade) = (order.out_trade_no.clone(), trade.clone());
                std::thread::spawn(move || credit_paid_order(&no, Some(&trade), 1_234, 2_001 + i))
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Credit::Credited { .. }))
                .count(),
            1
        );
        assert_eq!(balance(&key.id), Some(12_340_000));
    }

    #[test]
    fn payment_late_payment_for_closed_order_still_credits() {
        let key = billed_key(Some(0));
        let order = create_order(&key.id, None, 600, "alipay", 3_000).unwrap();
        let listed = |no: &str| {
            pending_orders(2_999, 3_001, 100)
                .unwrap()
                .iter()
                .any(|(n, created)| n == no && *created == 3_000)
        };
        assert!(listed(&order.out_trade_no));
        assert_eq!(pending_count(&key.id, 0).unwrap(), 1);
        mark_checked(&order.out_trade_no, 3_001).unwrap();
        assert!(close_order(&order.out_trade_no, 3_002).unwrap());
        assert!(
            !close_order(&order.out_trade_no, 3_003).unwrap(),
            "already closed"
        );
        assert!(!listed(&order.out_trade_no));
        assert_eq!(pending_count(&key.id, 0).unwrap(), 0);
        assert!(matches!(
            credit_paid_order(&order.out_trade_no, None, 600, 9_000),
            Ok(Credit::Credited { .. })
        ));
        assert_eq!(balance(&key.id), Some(6_000_000));
        let recent = recent_orders(&key.id, 10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].status, "paid");
    }

    #[test]
    fn payment_activates_pending_registered_key_only() {
        use crate::modules::portal_account as accounts;
        tokens::init_db().unwrap();
        let now = 5_000_000;
        let email = format!("pay-{}@example.com", uuid::Uuid::new_v4().simple());
        let code =
            match accounts::request_code(&email, accounts::CodePurpose::Register, now).unwrap() {
                accounts::CodeDelivery::Code { code, .. } => code,
                other => panic!("{other:?}"),
            };
        let defaults = accounts::RegistrationDefaults {
            max_concurrency: 0,
            allowed_models: vec![],
            expires_type: "never".into(),
        };
        let (user, key) =
            accounts::register(&email, &code, "correct horse", defaults, now).unwrap();
        assert!(!key.enabled);
        let order = create_order(&key.id, Some(&user.id), 500, "alipay", now).unwrap();
        credit_paid_order(&order.out_trade_no, None, 500, now + 1).unwrap();
        let paid = tokens::get_token_by_id(&key.id).unwrap().unwrap();
        assert!(paid.enabled, "paying activates a pending registered key");
        assert_eq!(paid.balance_micros, Some(5_000_000));

        // A disabled account keeps its key off even when a payment lands.
        accounts::set_account_enabled(&user.id, false).unwrap();
        let second = create_order(&key.id, Some(&user.id), 500, "alipay", now + 2).unwrap();
        credit_paid_order(&second.out_trade_no, None, 500, now + 3).unwrap();
        let off = tokens::get_token_by_id(&key.id).unwrap().unwrap();
        assert!(!off.enabled);
        assert_eq!(off.balance_micros, Some(10_000_000));
    }
}
