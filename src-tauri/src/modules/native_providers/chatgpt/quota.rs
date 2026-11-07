//! Rate-limit windows reported in `x-codex-*` response headers.
use crate::modules::native_providers::{epoch, Quota, QuotaPack};
use reqwest::header::HeaderMap;
use serde_json::Value;

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

/// Windows labelled `(label, window)`; 5h first. The smaller known window is "5h"; a single
/// known window of at most 360 minutes is "5h"; with none known primary is "7d".
fn labelled(h: &HeaderMap) -> Vec<(&'static str, Window)> {
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
        .filter_map(|(name, w)| Some((name, w?)))
        .collect()
}

pub fn from_headers(h: &HeaderMap, at: i64) -> Option<Quota> {
    let packs: Vec<QuotaPack> = labelled(h)
        .into_iter()
        .map(|(name, w)| QuotaPack {
            name: name.into(),
            total: Some(100.0),
            used: Some(w.used),
            remaining: Some(100.0 - w.used),
            unlimited: false,
            expires_at: w.reset_after.map(|s| at + s),
        })
        .collect();
    if packs.is_empty() {
        return None;
    }
    let mut quota = Quota::from_packs("%", packs);
    quota.updated_at = at;
    Some(quota)
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
