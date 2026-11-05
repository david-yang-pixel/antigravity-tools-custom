//! Persisted admission for both manual and background daily check-in.
//! Only explicit recovery clears a rejection; toggles and restarts keep the budget.
use serde::{Deserialize, Serialize};

pub const DAILY_ATTEMPTS: u8 = 3;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Pause {
    Authorization,
    Rejected,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    pub attempt_date: Option<String>,
    pub attempts: u8,
    pub next_attempt_at: i64,
    pub pause: Option<Pause>,
}

impl State {
    pub fn admission(&self, today: &str, now: i64) -> Result<(), String> {
        if let Some(pause) = self.pause {
            return Err(match pause {
                Pause::Authorization => "WorkBuddy check-in is paused; reauthorize this account",
                Pause::Rejected => "WorkBuddy check-in is paused after an upstream rejection; review the error before resuming",
            }.into());
        }
        if self.attempt_date.as_deref() == Some(today) && self.attempts >= DAILY_ATTEMPTS {
            return Err(
                "WorkBuddy check-in reached today's attempt limit (3); try tomorrow".into(),
            );
        }
        if self.next_attempt_at > now {
            return Err(format!(
                "WorkBuddy check-in is cooling down; retry in {} seconds",
                self.next_attempt_at.saturating_sub(now)
            ));
        }
        Ok(())
    }

    /// Persist before credential preparation or network I/O. A cancelled request
    /// retains its attempt and cooldown, including across a process restart.
    pub fn reserve(&mut self, today: String, now: i64, retry_secs: i64) {
        if self.attempt_date.as_ref() != Some(&today) {
            self.attempt_date = Some(today);
            self.attempts = 0;
        }
        self.attempts = self.attempts.saturating_add(1);
        let multiplier = 1i64 << self.attempts.saturating_sub(1).min(3);
        self.next_attempt_at = now.saturating_add(retry_secs.saturating_mul(multiplier));
    }

    pub fn fail(&mut self, failure: &Failure) {
        self.pause = failure.pause;
        if let Some(until) = failure.retry_at {
            self.next_attempt_at = self.next_attempt_at.max(until);
        }
    }
}

#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub pause: Option<Pause>,
    pub retry_at: Option<i64>,
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            pause: None,
            retry_at: None,
        }
    }
}

/// Retry-After accepts either delay-seconds or an HTTP date. Never shorten a
/// server's requested hold, including a hold that crosses the Beijing day boundary.
pub fn retry_at(value: &str, now: i64) -> Option<i64> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(now.saturating_add(value.parse::<i64>().unwrap_or(i64::MAX)));
    }
    chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.timestamp().max(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workbuddy_checkin_budget_survives_reload_and_midnight_does_not_shorten_hold() {
        let mut state = State::default();
        let at = 1792511940; // shortly before Beijing midnight
        for attempt in 0..DAILY_ATTEMPTS {
            let now = at + i64::from(attempt) * 86400 / 4;
            state.admission("2026-10-20", now).unwrap();
            state.reserve("2026-10-20".into(), now, 3600);
            state = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
            assert!(state.admission("2026-10-20", now + 1).is_err());
        }
        assert!(state.admission("2026-10-20", i64::MAX).is_err());
        assert!(state
            .admission("2026-10-21", state.next_attempt_at - 1)
            .is_err());
        let now = state.next_attempt_at;
        state.admission("2026-10-21", now).unwrap();
        state.reserve("2026-10-21".into(), now, 3600);
        assert_eq!(state.attempts, 1);
        assert_eq!(state.next_attempt_at, now + 3600);
    }

    #[test]
    fn workbuddy_checkin_rejections_need_explicit_recovery_without_resetting_budget() {
        let mut state = State::default();
        state.reserve("2026-10-20".into(), 100, 3600);
        state.fail(&Failure {
            message: "rejected".into(),
            pause: Some(Pause::Rejected),
            retry_at: Some(10000),
        });
        assert!(state.admission("2026-10-21", 20000).is_err());
        state.pause = None;
        assert!(state.admission("2026-10-20", 9999).is_err());
        assert_eq!(state.attempts, 1);
        state.admission("2026-10-20", 10000).unwrap();
    }

    #[test]
    fn workbuddy_checkin_retry_after_dates_seconds_and_untrusted_bounds() {
        let now = 1792511940;
        assert_eq!(retry_at("120", now), Some(now + 120));
        assert_eq!(retry_at("9999999999999999999999999", now), Some(i64::MAX));
        let date = chrono::DateTime::from_timestamp(now + 7200, 0)
            .unwrap()
            .to_rfc2822();
        assert_eq!(retry_at(&date, now), Some(now + 7200));
        assert_eq!(retry_at("Wed, 21 Oct 2015 07:28:00 GMT", now), Some(now));
        assert_eq!(retry_at("-1", now), None);
        assert_eq!(retry_at("garbage", now), None);
        assert_eq!(retry_at("", now), None);
    }
}
