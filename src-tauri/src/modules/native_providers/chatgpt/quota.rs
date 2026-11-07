//! Rate-limit windows reported in `x-codex-*` response headers.
use crate::modules::native_providers::{epoch, Quota, QuotaPack};
use crate::modules::native_providers::{now, store, Account};
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

struct Window {
    used: f64,
    reset_after: Option<i64>,
    minutes: Option<i64>,
}

fn header(h: &HeaderMap, name: &str) -> Option<f64> {
    h.get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}

fn window(h: &HeaderMap, which: &str) -> Option<Window> {
    Some(Window {
        used: header(h, &format!("x-codex-{which}-used-percent"))?,
        reset_after: header(h, &format!("x-codex-{which}-reset-after-seconds")).map(|n| n as i64),
        minutes: header(h, &format!("x-codex-{which}-window-minutes")).map(|n| n as i64),
    })
}

/// A window's name from its reported length: whole days as `Nd`, whole hours as `Nh`,
/// otherwise minutes as `Nm` (300 → "5h", 10080 → "7d", 43200 → "30d").
fn length_name(minutes: i64) -> String {
    match minutes {
        m if m >= 1440 && m % 1440 == 0 => format!("{}d", m / 1440),
        m if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// Windows named `(name, window)`, shorter first. A window that reports its length is named
/// by it; one that does not falls back to its rank: the smaller known window is "5h", a single
/// known window of at most 360 minutes is "5h", and with no lengths known primary is "7d".
fn labelled(h: &HeaderMap) -> Vec<(String, Window)> {
    let (primary, secondary) = (window(h, "primary"), window(h, "secondary"));
    let minutes = |w: &Option<Window>| w.as_ref().and_then(|w| w.minutes);
    let primary_is_short = match (minutes(&primary), minutes(&secondary)) {
        (Some(p), Some(s)) => p < s,
        (Some(p), None) => p <= 360,
        (None, Some(s)) => s > 360,
        (None, None) => false,
    };
    let (short, long) = if primary_is_short {
        (primary, secondary)
    } else {
        (secondary, primary)
    };
    [("5h", short), ("7d", long)]
        .into_iter()
        .filter_map(|(rank, w)| {
            let w = w?;
            let name = w
                .minutes
                .filter(|m| *m > 0)
                .map_or_else(|| rank.to_owned(), length_name);
            Some((name, w))
        })
        .collect()
}

pub fn from_headers(h: &HeaderMap, at: i64) -> Option<Quota> {
    let packs: Vec<QuotaPack> = labelled(h)
        .into_iter()
        .map(|(name, w)| QuotaPack {
            name,
            total: Some(100.0),
            used: Some(w.used),
            remaining: Some(100.0 - w.used),
            unlimited: false,
            expires_at: w.reset_after.map(|s| at + s),
        })
        .collect();
    // Percent windows do not add up: the account is as constrained as its fullest window.
    let remaining = packs.iter().filter_map(|p| p.remaining).reduce(f64::min)?;
    Some(Quota {
        unit: "%".into(),
        total: Some(100.0),
        remaining: Some(remaining),
        used: Some(100.0 - remaining),
        unlimited: false,
        updated_at: at,
        packs,
    })
}

/// Reset time of an exhausted account: the body's `resets_at`, else `resets_in_seconds`, else
/// the reset of a header window that is fully used (7d first).
pub fn exhausted_until(h: &HeaderMap, body: &Value, at: i64) -> Option<i64> {
    let error = body.get("error").unwrap_or(body);
    let from_body = error
        .get("resets_at")
        .and_then(epoch)
        .or_else(|| {
            let seconds = error.get("resets_in_seconds")?;
            let seconds = seconds
                .as_f64()
                .or_else(|| seconds.as_str()?.parse().ok())?;
            Some(at + seconds as i64)
        })
        .filter(|t| *t > at);
    from_body.or_else(|| {
        labelled(h)
            .into_iter()
            .rev()
            .find(|(_, w)| w.used >= 100.0)
            .and_then(|(_, w)| Some(at + w.reset_after?))
    })
}

/// Seconds between persisted snapshots of one account.
const WRITE_INTERVAL: i64 = 30;
static LAST_WRITE: LazyLock<Mutex<HashMap<String, i64>>> = LazyLock::new(Default::default);

/// Stores the quota windows an answer reported, at most once per 30 s per account unless a
/// window just filled up. Only `quota` is written.
pub fn record(a: &Account, h: &HeaderMap) {
    let at = now();
    let Some(quota) = from_headers(h, at) else {
        return;
    };
    let full = quota
        .packs
        .iter()
        .any(|p| p.used.is_some_and(|u| u >= 100.0));
    {
        let mut last = LAST_WRITE.lock().unwrap_or_else(|e| e.into_inner());
        if !full && last.get(&a.id).is_some_and(|t| at - t < WRITE_INTERVAL) {
            return;
        }
        last.insert(a.id.clone(), at);
    }
    let _ = store::update(a.provider, &a.id, None, |account| {
        account.quota = Some(quota)
    });
}
