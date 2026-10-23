//! Opt-in domestic reward collection, independent of downstream billing.
//! Protocol references: workbuddy2api v2.6.3 growth_reward.go/travel.go/task_common.py.
mod api;
#[cfg(test)]
mod tests;

use super::{
    accounts::{self, Account, AccountView, Realm},
    resources,
};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

const MAX_ACTIONS: usize = 20;
static SLOTS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));
static ACTIVE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

struct RunGuard(String);
impl RunGuard {
    fn new() -> Result<Self, String> {
        let id = uuid::Uuid::new_v4().to_string();
        ACTIVE
            .lock()
            .map_err(|_| "Activity workers unavailable")?
            .insert(id.clone());
        Ok(Self(id))
    }
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.lock() {
            active.remove(&self.0);
        }
    }
}
/// A persisted run without a live worker was interrupted (including a restart).
/// Keep its journal/idempotency key, but never leave manual controls locked forever.
pub(super) fn recover_interrupted(state: &mut State) {
    if state.status == "running"
        && ACTIVE
            .lock()
            .is_ok_and(|active| !active.contains(&state.run_id))
    {
        state.status = "partial".into();
        state.error =
            Some("Activity run interrupted; query or retry to confirm unfinished results".into());
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Query,
    Claim,
    #[serde(skip)]
    Automatic,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub last_attempt_at: i64,
    pub checked_at: i64,
    pub next_run_at: i64,
    pub status: String,
    pub error: Option<String>,
    pub items: Vec<Item>,
    pub history: Vec<Record>,
    /// Consecutive automatic runs that found nothing claimable; stretches the next delay.
    #[serde(default)]
    pub idle_streak: u32,
    #[serde(default)]
    pub(super) run_id: String,
    /// Persist the same idempotency key across ambiguous draw responses/restarts.
    /// AccountView clears this field; it is never an authentication credential.
    pub(super) pending_draw: Option<String>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            last_attempt_at: 0,
            checked_at: 0,
            next_run_at: 0,
            status: "idle".into(),
            error: None,
            items: vec![],
            history: vec![],
            idle_streak: 0,
            pending_draw: None,
            run_id: String::new(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub status: String,
    pub credits: Option<f64>,
    pub detail: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub at: i64,
    pub id: String,
    pub kind: String,
    pub name: String,
    pub status: String,
    pub credits: Option<f64>,
    pub detail: Option<String>,
}
fn item(kind: &str, id: &str, status: &str) -> Item {
    Item {
        id: id.into(),
        kind: kind.into(),
        name: id.into(),
        status: status.into(),
        credits: None,
        detail: None,
    }
}
fn eligible(account: &Account) -> bool {
    let m = &account.management;
    account.realm == Realm::Cn && account.enabled && m.managed && (m.auto_rewards || m.auto_lottery)
}
fn next_delay(account: &Account) -> i64 {
    super::identity::activity_interval_secs(&account.id, account.management.activity.idle_streak)
}
pub(super) fn due(account: &Account, now: i64) -> bool {
    let m = &account.management;
    eligible(account)
        && now >= m.activity.next_run_at
        && (m.activity.last_attempt_at == 0
            || now - m.activity.last_attempt_at >= next_delay(account))
}
/// Real inference success may collect rewards without waiting for the sparse background cadence.
pub(super) fn due_opportunistic(account: &Account, now: i64) -> bool {
    let m = &account.management;
    eligible(account)
        && (m.activity.last_attempt_at == 0
            || now - m.activity.last_attempt_at >= super::identity::ACTIVITY_OPPORTUNITY_SECS)
}
fn current(account: &Account, action: Action) -> Result<Account, String> {
    let value = accounts::get(&account.id)?;
    if value.revision != account.revision || value.realm != Realm::Cn || !value.enabled {
        return Err("WorkBuddy account changed or was disabled; query activities again".into());
    }
    if action == Action::Automatic && !value.management.managed {
        return Err("Automatic account management was disabled".into());
    }
    Ok(value)
}

/// Return promptly for HTTP/Tauri; polling the existing account list shows progress.
/// Lock and worker slot are acquired before persisting/scheduling, so double clicks
/// cannot queue another reward run. Ordinary maintenance uses the same account lock.
pub fn start(id: &str, action: Action) -> Result<AccountView, String> {
    let guard = accounts::refresh_lock(&format!("maintenance:{id}"))?
        .try_lock_owned()
        .map_err(|_| "WorkBuddy account maintenance is already running")?;
    let permit = SLOTS
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Activity workers are busy; retry shortly")?;
    let initial = accounts::get(id)?;
    if initial.realm != Realm::Cn {
        return Err("Activity rewards are only supported for domestic accounts".into());
    }
    if !initial.enabled {
        return Err("Enable the account before querying activities".into());
    }
    if action == Action::Automatic
        && !due(&initial, accounts::now())
        && !due_opportunistic(&initial, accounts::now())
    {
        return accounts::view(initial);
    }
    // Manual retries also have a short debounce; queries remain read-only.
    if action != Action::Query && accounts::now() - initial.management.activity.last_attempt_at < 30
    {
        return Err("Wait 30 seconds before retrying activity collection".into());
    }
    let running = RunGuard::new()?;
    let delay = next_delay(&initial);
    accounts::edit_activity(&initial, |m| {
        m.activity.status = "running".into();
        m.activity.run_id = running.0.clone();
        m.activity.error = None;
        // A read-only query must not postpone the automatic collection schedule.
        if action != Action::Query {
            m.activity.last_attempt_at = accounts::now();
            // Provisional delay; execute() may stretch it after an empty collection.
            m.activity.next_run_at = accounts::now() + delay;
        }
    })?;
    let initial = accounts::get(id)?;
    let response = accounts::view(initial.clone())?;
    tokio::spawn(async move {
        let (_guard, _permit, _running) = (guard, permit, running);
        execute(initial, action).await;
    });
    Ok(response)
}

async fn execute(initial: Account, action: Action) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let account = match current(&initial, action) {
        Ok(_) => {
            match accounts::fresh_before(&initial.id, deadline).await {
                Ok(Ok(account)) => account,
                _ => {
                    fail_run(&initial, "Could not prepare account credentials; retry after authorization is restored".into());
                    return;
                }
            }
        }
        Err(error) => {
            fail_run(&initial, error);
            return;
        }
    };
    let mut runner = Runner {
        state: account.management.activity.clone(),
        account,
        action,
        failures: Vec::new(),
        read_ok: 0,
    };
    let result = tokio::time::timeout_at(deadline, runner.cycle()).await;
    match result {
        Err(_) => runner
            .failures
            .push("Activity run timed out; unfinished requests remain unconfirmed".into()),
        Ok(Err(error)) => runner.failures.push(error),
        Ok(Ok(())) => {}
    }
    if runner.state.pending_draw.is_some() {
        runner.failures.push("A previous lottery result remains unconfirmed; enable free lottery and retry to verify it".into());
    }
    runner.state.status = if runner.failures.is_empty() {
        "success"
    } else if runner.read_ok > 0 {
        "partial"
    } else {
        "failed"
    }
    .into();
    runner.state.error = (!runner.failures.is_empty()).then(|| runner.failures.join("; "));
    if action != Action::Query {
        let claimed = runner
            .state
            .history
            .iter()
            .any(|r| r.at >= runner.state.last_attempt_at && r.status == "claimed");
        if action == Action::Automatic || action == Action::Claim {
            if claimed {
                runner.state.idle_streak = 0;
            } else if action == Action::Automatic {
                runner.state.idle_streak = runner.state.idle_streak.saturating_add(1).min(8);
            }
        }
        runner.state.next_run_at = accounts::now()
            + super::identity::activity_interval_secs(&runner.account.id, runner.state.idle_streak);
    }
    let _ = runner.save();
}
fn fail_run(account: &Account, error: String) {
    let mut state = account.management.activity.clone();
    state.status = "failed".into();
    state.error = Some(error);
    state.next_run_at =
        accounts::now() + super::identity::activity_interval_secs(&account.id, state.idle_streak);
    let _ = accounts::save_activity(account, state);
}
struct Runner {
    account: Account,
    action: Action,
    state: State,
    failures: Vec<String>,
    read_ok: usize,
}
impl Runner {
    fn save(&self) -> Result<(), String> {
        accounts::save_activity(&self.account, self.state.clone())
    }
    fn collect(&mut self, kind: &str, result: Result<Vec<Item>, String>) {
        match result {
            Ok(items) => {
                self.read_ok += 1;
                self.state.items.extend(items);
            }
            Err(error) => {
                let mut entry = item(kind, kind, "failed");
                entry.detail = Some(error.clone());
                self.state.items.push(entry);
                self.failures.push(error);
            }
        }
    }
    fn read_result(
        &mut self,
        kind: &str,
        result: Result<Value, api::Error>,
        parse: impl FnOnce(&Value) -> Result<Vec<Item>, String>,
    ) {
        match result {
            Ok(value) => self.collect(kind, parse(&value)),
            Err(e) if e.status == "unsupported" => {
                let mut entry = item(kind, kind, "unsupported");
                entry.detail = Some(e.detail);
                self.state.items.push(entry);
                self.read_ok += 1;
            }
            Err(e) => self.collect(kind, Err(e.detail)),
        }
    }
    async fn query(&mut self) -> Result<(), String> {
        current(&self.account, self.action)?;
        let (task_response, streak, travel, chances) = tokio::join!(
            api::request(&self.account, Method::GET, api::TASKS, None, false),
            api::request(&self.account, Method::GET, api::STREAK, None, false),
            api::request(&self.account, Method::GET, api::TRAVEL, None, false),
            api::request(&self.account, Method::GET, api::CHANCES, None, false),
        );
        self.state.items.clear();
        let account = self.account.clone();
        self.read_result("task", task_response, |v| tasks(v, &account));
        self.read_result("streak", streak, tiers);
        self.read_result("travel", travel, travel_item);
        self.read_result("lottery", chances, lottery_item);
        self.state.checked_at = accounts::now();
        self.save()
    }
    fn allowed(&self, lottery: bool) -> Result<bool, String> {
        let a = current(&self.account, self.action)?;
        Ok(if lottery {
            a.management.auto_lottery
        } else {
            self.action == Action::Claim
                || (self.action == Action::Automatic && a.management.auto_rewards)
        })
    }
    async fn cycle(&mut self) -> Result<(), String> {
        self.query().await?;
        if self.action == Action::Query {
            return Ok(());
        }
        let available: Vec<_> = self
            .state
            .items
            .iter()
            .filter(|v| v.kind != "lottery" && v.status == "available")
            .cloned()
            .collect();
        for entry in available.into_iter().take(MAX_ACTIONS) {
            if !self.allowed(false)? {
                break;
            }
            self.claim(entry).await?;
        }
        // Re-query chances after milestone claims, which can award new free draws.
        if self.allowed(true)? {
            for _ in 0..5 {
                if !self.allowed(true)? {
                    break;
                }
                // The last chance may already have been consumed before its response
                // was lost. Resolve that SAME request before asking for a new draw.
                if self.state.pending_draw.is_some() {
                    if !self
                        .claim(item("lottery", "free_lottery", "unknown"))
                        .await?
                    {
                        break;
                    }
                    continue;
                }
                let chances =
                    match api::request(&self.account, Method::GET, api::CHANCES, None, false).await
                    {
                        Ok(v) => lottery_item(&v),
                        Err(e) => Err(e.detail),
                    };
                let entries = match chances {
                    Ok(entries) => entries,
                    Err(e) => {
                        self.failures.push(e);
                        break;
                    }
                };
                let entry = entries.into_iter().next().unwrap();
                self.replace_item(entry.clone());
                if entry.status != "available" {
                    break;
                }
                if !self.claim(entry).await? {
                    break;
                }
            }
        }
        // Query again, never infer eligibility/remaining chances from a successful POST.
        self.query().await?;
        current(&self.account, self.action)?;
        match resources::credits(&self.account).await {
            Ok(credits) => accounts::edit_activity(&self.account, |m| {
                m.credits = Some(credits);
                m.credits_error = None;
            })?,
            Err(e) => {
                accounts::edit_activity(&self.account, |m| m.credits_error = Some(e.clone()))?;
                self.failures
                    .push("Rewards checked, but credit balance refresh failed".into());
            }
        }
        Ok(())
    }
    fn replace_item(&mut self, entry: Item) {
        self.state
            .items
            .retain(|old| old.kind != entry.kind || old.id != entry.id);
        self.state.items.push(entry);
    }
    async fn claim(&mut self, entry: Item) -> Result<bool, String> {
        if !self.allowed(entry.kind == "lottery")? {
            return Ok(false);
        }
        let retrying_draw = entry.kind == "lottery" && self.state.pending_draw.is_some();
        let token = if entry.kind == "lottery" {
            self.state
                .pending_draw
                .get_or_insert_with(|| format!("draw-{}", uuid::Uuid::new_v4()))
                .clone()
        } else {
            format!("reward-{}", uuid::Uuid::new_v4())
        };
        // Journal before network I/O. Process death cannot turn an ambiguous draw
        // into a new draw with another key on the next scheduled run.
        // Retry updates the existing logical draw, not an extra reward record.
        if retrying_draw {
            if let Some(index) = self.state.history.iter().position(|r| {
                r.kind == "lottery" && matches!(r.status.as_str(), "unknown" | "failed")
            }) {
                self.state.history.remove(index);
            }
        }
        self.state.history.insert(
            0,
            Record {
                at: accounts::now(),
                id: entry.id.clone(),
                kind: entry.kind.clone(),
                name: entry.name.clone(),
                status: "unknown".into(),
                credits: None,
                detail: Some("Request started; result unconfirmed".into()),
            },
        );
        self.state.history.truncate(50);
        self.save()?;
        let result = match entry.kind.as_str() {
            "task" => api::claim_task(&self.account, &entry.id).await,
            "streak" => api::request(&self.account, Method::POST, "/activity/growth/redeem", Some(json!({"tier":entry.id,"client_token":token})), false).await,
            "travel" => api::request(&self.account, Method::POST, "/activity/growth/buddy/travel/claim", Some(json!({"record_id":entry.id.parse::<i64>().map_err(|_| "Invalid travel record")?})), false).await,
            "lottery" => api::request(&self.account, Method::POST, "/activity/growth/lottery/draw", Some(json!({"client_token":token})), false).await,
            _ => return Err("Unknown reward type".into()),
        };
        let record = &mut self.state.history[0];
        let confirmed = match result {
            Ok(value) => {
                let already = value["already_claimed"].as_bool() == Some(true);
                record.status = if already { "skipped" } else { "claimed" }.into();
                let key = match entry.kind.as_str() {
                    "streak" => "credit_granted",
                    "travel" => "reward_credit",
                    "lottery" => "credit_amount",
                    _ => "credit",
                };
                record.credits = if already {
                    None
                } else {
                    api::amount(&value[key])
                };
                record.detail = if already {
                    Some("Already claimed".into())
                } else if entry.kind == "lottery" {
                    Some(api::safe_name(
                        &self.account,
                        value["prize_name"].as_str(),
                        "Lottery completed; check the official activity page for prize details",
                    ))
                } else {
                    None
                };
                if entry.kind == "lottery" {
                    self.state.pending_draw = None;
                }
                true
            }
            Err(e) => {
                record.status = if retrying_draw {
                    "unknown"
                } else if e.status == "unsupported" {
                    "skipped"
                } else {
                    e.status
                }
                .into();
                record.detail = Some(e.detail.clone());
                if entry.kind == "lottery"
                    && !retrying_draw
                    && matches!(e.status, "skipped" | "unsupported")
                {
                    self.state.pending_draw = None;
                }
                if matches!(e.status, "unknown" | "failed") {
                    self.failures.push(e.detail);
                }
                false
            }
        };
        self.save()?;
        Ok(confirmed)
    }
}

fn tasks(value: &Value, account: &Account) -> Result<Vec<Item>, String> {
    let values = value["tasks"]
        .as_array()
        .filter(|v| v.len() <= 200)
        .ok_or("Activity tasks response has an unknown format")?;
    let mut seen = HashSet::new();
    values
        .iter()
        .map(|v| {
            let id = v["task_code"]
                .as_str()
                .filter(|id| api::identifier(id))
                .ok_or("Invalid activity task ID")?;
            if !seen.insert(id) {
                return Err("Duplicate activity task ID".into());
            }
            let state = v["accept_status"]
                .as_str()
                .ok_or("Activity task status is missing")?;
            let complete = v["progress"]["current"]
                .as_u64()
                .zip(v["progress"]["target"].as_u64())
                .is_some_and(|(n, target)| target > 0 && n >= target);
            let status = match state {
                "claimed" => "claimed",
                "completed" => "available",
                "accepted" | "in_progress" if complete => "available",
                "accepted" | "in_progress" | "not_accepted" | "locked" | "pending" => "pending",
                _ => "unknown",
            };
            let mut entry = item("task", id, status);
            entry.name = api::safe_name(
                account,
                v["task_name"]
                    .as_str()
                    .or_else(|| v["name"].as_str())
                    .or_else(|| v["title"].as_str()),
                id,
            );
            entry.credits = api::amount(&v["reward"]["credit"]);
            Ok(entry)
        })
        .collect()
}
fn tiers(value: &Value) -> Result<Vec<Item>, String> {
    let status = value["redemption_status"]
        .as_object()
        .ok_or("Activity milestone status is missing")?;
    let mut entries = Vec::new();
    for (key, state) in status {
        if let Some(id) = key
            .strip_prefix("tier_")
            .and_then(|s| s.strip_suffix("_status"))
            .filter(|s| api::identifier(s))
        {
            let state = match state.as_str() {
                Some("available") => "available",
                Some("claimed") => "claimed",
                Some("locked") => "pending",
                _ => "unknown",
            };
            let mut entry = item("streak", id, state);
            entry.credits = status
                .get("tiers")
                .and_then(Value::as_array)
                .and_then(|values| values.iter().find(|v| v["tier"].as_str() == Some(id)))
                .and_then(|v| api::amount(&v["credit"]));
            entries.push(entry);
        }
    }
    if entries.is_empty() {
        return Err("Activity milestone response has an unknown format".into());
    }
    Ok(entries)
}
fn travel_item(value: &Value) -> Result<Vec<Item>, String> {
    let state = value["state"].as_str().ok_or("Travel state is missing")?;
    let record = value["record_id"].as_i64().filter(|id| *id > 0);
    let status = match state {
        "arrived" if record.is_some() => "available",
        "idle" | "traveling" => "pending",
        "claimed" => "claimed",
        _ => "unknown",
    };
    let mut entry = item(
        "travel",
        &record
            .map(|id| id.to_string())
            .unwrap_or_else(|| "travel".into()),
        status,
    );
    entry.credits = api::amount(&value["reward_credit"]);
    Ok(vec![entry])
}
fn lottery_item(value: &Value) -> Result<Vec<Item>, String> {
    let balance = value["balance"]
        .as_u64()
        .filter(|n| *n <= 100_000)
        .ok_or("Free lottery balance is missing or invalid")?;
    let mut entry = item(
        "lottery",
        "free_lottery",
        if balance > 0 { "available" } else { "pending" },
    );
    entry.detail = Some(format!("Free chances: {balance}"));
    Ok(vec![entry])
}
