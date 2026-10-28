//! Admin alerts for problems an operator should hear about right away: a model
//! whose requests keep failing, upstream quota or credits that ran out, and a
//! WorkBuddy account whose credits fell below the configured threshold.
//!
//! Requests are observed from the proxy monitor, which every protocol passes
//! through, so detection is independent of the client protocol. Alerts go to
//! the webhooks and email recipients in `portal_settings.json` (`alerts`).

use crate::modules::portal_mail::{self, Mail};
use crate::modules::portal_settings::{self, PortalSettings};
use crate::proxy::monitor::ProxyRequestLog;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Models tracked at once; only failing models are kept, so this is a guard
/// against unbounded growth rather than a working limit.
const MAX_TRACKED: usize = 256;
const SETTINGS_TTL: Duration = Duration::from_secs(30);
const ERROR_SNIPPET_CHARS: usize = 300;

/// Upstream messages that mean the account or pool has no quota or credits
/// left, as opposed to a transient rate limit such as Google's generic
/// "Resource has been exhausted (e.g. check quota)".
static QUOTA_EXHAUSTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)quota[_ ]exhausted|insufficient[_ ](?:quota|credits?|balance)",
        r"|exceeded (?:your|the) (?:current )?quota",
        r"|(?:out of|no remaining|not enough) credits?",
        r"|credits? (?:exhausted|depleted|used up|insufficient)",
        r"|(?:daily|weekly|monthly) quota",
        r"|(?:余额|额度|积分|配额|点数)(?:不足|已?用尽|已?耗尽)",
    ))
    .expect("quota pattern")
});

#[derive(Default)]
struct Streak {
    failures: u32,
    /// An alert was delivered for this streak, so its end is reported too.
    alerted: bool,
}

#[derive(Default)]
struct State {
    streaks: HashMap<String, Streak>,
    /// Alert key -> unix seconds of the last alert sent for it.
    sent: HashMap<String, i64>,
    /// WorkBuddy accounts currently at or below the credit threshold.
    low_credit: HashSet<String>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static SETTINGS: Mutex<Option<(Instant, Option<Arc<PortalSettings>>)>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    /// Cooldown key, e.g. `failing:<model>`.
    key: String,
    title: String,
    lines: Vec<(&'static str, String)>,
}

impl Alert {
    fn text(&self, brand: &str) -> (String, String) {
        let subject = format!("[{brand}] 告警 Alert · {}", self.title);
        let mut body: Vec<String> = self
            .lines
            .iter()
            .map(|(label, value)| format!("{label}: {value}"))
            .collect();
        body.push(format!("时间 / Time: {}", beijing_now()));
        (subject, body.join("\n"))
    }
}

fn beijing_now() -> String {
    chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .format("%Y-%m-%d %H:%M:%S (UTC+8)")
        .to_string()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Drops the cached settings so the next event reads the saved file.
pub fn invalidate_settings() {
    *SETTINGS.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// Effective settings, re-read at most every [`SETTINGS_TTL`] so the request
/// path never touches the disk for each failure.
fn settings() -> Option<Arc<PortalSettings>> {
    let mut cached = SETTINGS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, settings)) = cached.as_ref() {
        if at.elapsed() < SETTINGS_TTL {
            return settings.clone();
        }
    }
    let loaded = match portal_settings::load() {
        Ok(settings) => Some(Arc::new(settings)),
        Err(error) => {
            tracing::warn!("[Alerts] cannot load settings: {error}");
            None
        }
    };
    *cached = Some((Instant::now(), loaded.clone()));
    loaded
}

/// Failures the gateway itself did not cause: admission errors (bad key,
/// balance, concurrency, unpublished model) are returned before the monitor
/// layer, so 401/403/429/5xx seen here come from the upstream or the pool.
fn is_upstream_failure(status: u16) -> bool {
    matches!(status, 401 | 403 | 429) || status >= 500
}

fn is_quota_exhausted(error: &str) -> bool {
    QUOTA_EXHAUSTED.is_match(error)
}

fn snippet(error: &str) -> String {
    let flat = error.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(ERROR_SNIPPET_CHARS).collect();
    if flat.chars().count() > ERROR_SNIPPET_CHARS {
        out.push('…');
    }
    out
}

/// Whether `key` is outside its cooldown; records the send when it is.
fn claim(state: &mut State, key: &str, cooldown_secs: i64, at: i64) -> bool {
    if state
        .sent
        .get(key)
        .is_some_and(|last| at - last < cooldown_secs)
    {
        return false;
    }
    if state.sent.len() >= 4 * MAX_TRACKED {
        state.sent.retain(|_, last| at - *last < 86_400);
    }
    state.sent.insert(key.to_string(), at);
    true
}

/// Updates the failure streaks with one finished request and returns the
/// alerts it triggers.
fn record(
    state: &mut State,
    log: &ProxyRequestLog,
    settings: &PortalSettings,
    at: i64,
) -> Vec<Alert> {
    // Warmups probe single accounts; a pool-wide alert would be misleading.
    if log.url.starts_with("/internal/") {
        return Vec::new();
    }
    let Some(model) = log
        .model
        .as_deref()
        .or(log.mapped_model.as_deref())
        .filter(|model| !model.is_empty())
    else {
        return Vec::new();
    };
    let model_line = match log.mapped_model.as_deref() {
        Some(mapped) if mapped != model => format!("{model} → {mapped}"),
        _ => model.to_string(),
    };
    let cooldown_secs = i64::from(settings.alerts.cooldown_minutes) * 60;
    if log.status < 400 {
        // Recovery answers exactly one delivered alert. Alert cooldowns survive
        // it, so traffic alternating between success and failure cannot alert
        // on every request, and every alert still gets its recovery.
        return match state.streaks.remove(model) {
            Some(streak) if streak.alerted => {
                vec![Alert {
                    key: format!("recovered:{model}"),
                    title: format!("模型已恢复 / Model recovered: {model}"),
                    lines: vec![
                        ("模型 / Model", model_line),
                        (
                            "恢复前连续失败 / Failures before recovery",
                            streak.failures.to_string(),
                        ),
                    ],
                }]
            }
            _ => Vec::new(),
        };
    }
    let error = log.error.as_deref().unwrap_or_default();
    let quota = is_quota_exhausted(error);
    if !quota && !is_upstream_failure(log.status) {
        return Vec::new();
    }
    if !state.streaks.contains_key(model) && state.streaks.len() >= MAX_TRACKED {
        return Vec::new();
    }
    let streak = state.streaks.entry(model.to_string()).or_default();
    streak.failures += 1;
    let failures = streak.failures;
    let alerts = &settings.alerts;
    let (key, title) = if quota {
        (
            format!("quota:{model}"),
            format!("上游额度不足 / Upstream quota exhausted: {model}"),
        )
    } else if failures >= alerts.failure_threshold {
        (
            format!("failing:{model}"),
            format!("模型连续失败 / Model failing: {model}"),
        )
    } else {
        return Vec::new();
    };
    if !claim(state, &key, cooldown_secs, at) {
        return Vec::new();
    }
    if let Some(streak) = state.streaks.get_mut(model) {
        streak.alerted = true;
    }
    let mut lines = vec![
        ("模型 / Model", model_line),
        ("连续失败 / Consecutive failures", failures.to_string()),
        ("最近状态码 / Last status", log.status.to_string()),
    ];
    if let Some(account) = log.account_email.as_deref().filter(|a| !a.is_empty()) {
        lines.push(("上游账号 / Upstream account", account.to_string()));
    }
    if let Some(user) = log.username.as_deref().filter(|u| !u.is_empty()) {
        lines.push(("调用用户 / User", user.to_string()));
    }
    if !error.is_empty() {
        lines.push(("错误 / Error", snippet(error)));
    }
    vec![Alert { key, title, lines }]
}

/// Called for every finished proxy request, whatever its protocol.
pub fn observe_request(log: &ProxyRequestLog) {
    // Successes only matter to a model that is already failing; skip the
    // settings lookup for the common case.
    if log.status < 400 && lock().streaks.is_empty() {
        return;
    }
    let Some(settings) = settings().filter(|s| s.alerts.enabled && s.alerts.has_targets()) else {
        return;
    };
    let alerts = record(&mut lock(), log, &settings, now());
    for alert in alerts {
        dispatch(settings.clone(), alert);
    }
}

/// Called after a WorkBuddy account's credits are refreshed. Alerts once when
/// the balance drops to the threshold, and again only after it recovered.
pub fn observe_credits(account_id: &str, label: &str, available: f64) {
    let Some(settings) = settings().filter(|s| s.alerts.enabled && s.alerts.has_targets()) else {
        return;
    };
    if let Some(alert) = record_credits(&mut lock(), &settings, account_id, label, available, now())
    {
        dispatch(settings, alert);
    }
}

fn record_credits(
    state: &mut State,
    settings: &PortalSettings,
    account_id: &str,
    label: &str,
    available: f64,
    at: i64,
) -> Option<Alert> {
    let threshold = settings.alerts.credit_threshold;
    if available > threshold {
        state.low_credit.remove(account_id);
        return None;
    }
    if state.low_credit.contains(account_id) {
        return None;
    }
    let key = format!("credits:{account_id}");
    if !claim(
        state,
        &key,
        i64::from(settings.alerts.cooldown_minutes) * 60,
        at,
    ) {
        return None;
    }
    state.low_credit.insert(account_id.to_string());
    Some(Alert {
        key,
        title: format!("上游积分不足 / Upstream credits low: {label}"),
        lines: vec![
            ("WorkBuddy 账号 / Account", label.to_string()),
            ("剩余积分 / Credits left", format!("{available:.2}")),
            ("告警阈值 / Threshold", format!("{threshold:.2}")),
        ],
    })
}

fn dispatch(settings: Arc<PortalSettings>, alert: Alert) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("[Alerts] no async runtime; dropped alert {}", alert.key);
        return;
    };
    runtime.spawn(async move {
        for result in deliver(&settings, &alert).await {
            if let Some(error) = result.error {
                tracing::warn!(
                    "[Alerts] {} via {} failed: {error}",
                    alert.key,
                    result.target
                );
            }
        }
        tracing::info!("[Alerts] sent {}", alert.key);
    });
}

#[derive(Debug, Serialize)]
pub struct DeliveryResult {
    /// Webhook host or email address; never the full URL, which holds secrets.
    pub target: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Sends a sample alert to every configured target, even while alerts are
/// switched off, so the admin can check the channels before enabling them.
pub async fn send_test() -> Result<Vec<DeliveryResult>, String> {
    let settings = portal_settings::load()?;
    if !settings.alerts.has_targets() {
        return Err("Add an alert webhook or email first".into());
    }
    let alert = Alert {
        key: "test".into(),
        title: "测试告警 / Test alert".into(),
        lines: vec![(
            "说明 / Note",
            "告警通道可用 / This alert channel is working.".into(),
        )],
    };
    Ok(deliver(&settings, &alert).await)
}

async fn deliver(settings: &PortalSettings, alert: &Alert) -> Vec<DeliveryResult> {
    let (subject, body) = alert.text(portal_mail::brand(&settings.smtp));
    let mut results = Vec::new();
    for url in &settings.alerts.webhooks {
        let target = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "webhook".into());
        let outcome = post_webhook(url, &subject, &body).await;
        results.push(DeliveryResult {
            target,
            ok: outcome.is_ok(),
            error: outcome.err(),
        });
    }
    for email in &settings.alerts.emails {
        let mail = Mail {
            subject: subject.clone(),
            body: format!("{body}\n"),
        };
        let outcome = portal_mail::send(&settings.smtp, email, mail).await;
        results.push(DeliveryResult {
            target: email.clone(),
            ok: outcome.is_ok(),
            error: outcome.err(),
        });
    }
    results
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    WeCom,
    DingTalk,
    Feishu,
    Slack,
    Discord,
    ServerChan,
    /// Any other endpoint: `title`, `body` and `text` cover Bark, Telegram
    /// (`sendMessage?chat_id=` in the URL) and most JSON receivers.
    Generic,
}

fn flavor(url: &str) -> Flavor {
    let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
    else {
        return Flavor::Generic;
    };
    let is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if host == "qyapi.weixin.qq.com" {
        Flavor::WeCom
    } else if is("dingtalk.com") {
        Flavor::DingTalk
    } else if is("feishu.cn") || is("larksuite.com") {
        Flavor::Feishu
    } else if host == "hooks.slack.com" {
        Flavor::Slack
    } else if is("discord.com") || is("discordapp.com") {
        Flavor::Discord
    } else if is("ftqq.com") || is("ft07.com") {
        Flavor::ServerChan
    } else {
        Flavor::Generic
    }
}

fn payload(flavor: Flavor, subject: &str, body: &str) -> Value {
    let text = format!("{subject}\n{body}");
    match flavor {
        Flavor::WeCom | Flavor::DingTalk => json!({"msgtype": "text", "text": {"content": text}}),
        Flavor::Feishu => json!({"msg_type": "text", "content": {"text": text}}),
        Flavor::Slack => json!({"text": text}),
        // Discord rejects content over 2000 characters.
        Flavor::Discord => json!({"content": text.chars().take(1900).collect::<String>()}),
        // ServerChan titles are capped at 32 characters.
        Flavor::ServerChan => json!({
            "title": subject.chars().take(32).collect::<String>(),
            "desp": body.replace('\n', "\n\n"),
        }),
        Flavor::Generic => json!({
            "event": "alert",
            "title": subject,
            "body": body,
            "text": text,
            "timestamp": now(),
        }),
    }
}

fn http_client() -> Result<reqwest::Client, String> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to build alert HTTP client: {e}"))?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// Errors never include the URL, which usually carries the webhook key.
async fn post_webhook(url: &str, subject: &str, body: &str) -> Result<(), String> {
    let flavor = flavor(url);
    let response = http_client()?
        .post(url)
        .json(&payload(flavor, subject, body))
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e.without_url()))?;
    let status = response.status();
    let reply: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    // These services answer 200 with an error code for rejected messages.
    let (code, message) = match flavor {
        Flavor::WeCom | Flavor::DingTalk => (&reply["errcode"], &reply["errmsg"]),
        Flavor::Feishu => (&reply["code"], &reply["msg"]),
        Flavor::ServerChan => (&reply["code"], &reply["message"]),
        _ => return Ok(()),
    };
    match code.as_i64() {
        Some(code) if code != 0 => Err(format!(
            "rejected ({code}): {}",
            message.as_str().unwrap_or_default()
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(threshold: u32) -> PortalSettings {
        let mut settings = PortalSettings::default();
        settings.alerts.enabled = true;
        settings.alerts.failure_threshold = threshold;
        settings.alerts.cooldown_minutes = 30;
        settings.alerts.credit_threshold = 10.0;
        settings
    }

    fn log(model: &str, status: u16, error: Option<&str>) -> ProxyRequestLog {
        serde_json::from_value(json!({
            "id": "x", "timestamp": 0, "method": "POST", "url": "/v1/chat/completions",
            "status": status, "duration": 1, "model": model, "error": error,
            "account_email": "pool@example.com", "username": "alice",
        }))
        .unwrap()
    }

    #[test]
    fn alerts_fire_after_consecutive_failures_and_report_recovery() {
        let settings = settings(3);
        let mut state = State::default();
        let mut at = 1_000;
        for _ in 0..2 {
            assert!(record(&mut state, &log("m", 503, Some("boom")), &settings, at).is_empty());
        }
        // A client error neither counts nor resets the streak.
        assert!(record(&mut state, &log("m", 400, Some("bad")), &settings, at).is_empty());
        let fired = record(&mut state, &log("m", 503, Some("boom")), &settings, at);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].key, "failing:m");
        assert!(fired[0]
            .lines
            .contains(&("连续失败 / Consecutive failures", "3".into())));
        // Inside the cooldown further failures stay quiet.
        at += 60;
        assert!(record(&mut state, &log("m", 503, None), &settings, at).is_empty());
        // Another model has its own streak.
        assert!(record(&mut state, &log("other", 500, None), &settings, at).is_empty());
        let recovered = record(&mut state, &log("m", 200, None), &settings, at);
        assert_eq!(recovered[0].key, "recovered:m");
        assert!(recovered[0]
            .lines
            .contains(&("恢复前连续失败 / Failures before recovery", "4".into())));
        // A new outage inside the cooldown stays quiet; after it, it alerts again.
        for _ in 0..3 {
            assert!(record(&mut state, &log("m", 502, None), &settings, at).is_empty());
        }
        at += 30 * 60;
        assert_eq!(
            record(&mut state, &log("m", 502, None), &settings, at)[0].key,
            "failing:m"
        );
        // A streak that never alerted ends silently.
        assert!(record(&mut state, &log("other", 200, None), &settings, at).is_empty());
    }

    #[test]
    fn alerts_report_quota_exhaustion_immediately_and_skip_warmups() {
        let settings = settings(100);
        let mut state = State::default();
        let fired = record(
            &mut state,
            &log("m", 429, Some(r#"{"error":{"status":"QUOTA_EXHAUSTED"}}"#)),
            &settings,
            0,
        );
        assert_eq!(fired[0].key, "quota:m");
        let mut warmup = log("w", 429, Some("insufficient_quota"));
        warmup.url = "/internal/warmup -> w".into();
        assert!(record(&mut state, &warmup, &settings, 0).is_empty());
    }

    #[test]
    fn alerts_flapping_traffic_respects_cooldown() {
        let settings = settings(5);
        let mut state = State::default();
        let quota = log("m", 429, Some("insufficient_quota"));
        let ok = log("m", 200, None);
        let mut sent = Vec::new();
        for i in 0..20 {
            sent.extend(record(&mut state, &quota, &settings, i));
            sent.extend(record(&mut state, &ok, &settings, i));
        }
        let keys: Vec<_> = sent.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["quota:m", "recovered:m"]);
    }

    #[test]
    fn alerts_every_delivered_alert_gets_its_recovery() {
        let settings = settings(1);
        let mut state = State::default();
        let fail = log("m", 503, None);
        let ok = log("m", 200, None);
        let minute = 60;
        assert_eq!(record(&mut state, &fail, &settings, 0)[0].key, "failing:m");
        assert_eq!(
            record(&mut state, &ok, &settings, 29 * minute)[0].key,
            "recovered:m"
        );
        // The failing cooldown has elapsed, so the next outage alerts again,
        // and its recovery arrives even though the last one was 2 minutes ago.
        assert_eq!(
            record(&mut state, &fail, &settings, 30 * minute)[0].key,
            "failing:m"
        );
        assert_eq!(
            record(&mut state, &ok, &settings, 31 * minute)[0].key,
            "recovered:m"
        );
    }

    #[test]
    fn alerts_quota_pattern_ignores_transient_rate_limits() {
        for exhausted in [
            "QUOTA_EXHAUSTED",
            "insufficient_quota",
            "You exceeded your current quota",
            "Insufficient credits for this request",
            "credits exhausted",
            "daily quota reached",
            "账户余额不足",
            "积分已用尽",
        ] {
            assert!(is_quota_exhausted(exhausted), "{exhausted}");
        }
        for transient in [
            "Resource has been exhausted (e.g. check quota).",
            "RESOURCE_EXHAUSTED",
            "rate limit exceeded",
            "upstream connect error",
        ] {
            assert!(!is_quota_exhausted(transient), "{transient}");
        }
    }

    #[test]
    fn alerts_credit_low_fires_once_until_recovered() {
        let settings = settings(5);
        let mut state = State::default();
        assert!(record_credits(&mut state, &settings, "a", "acct", 50.0, 0).is_none());
        let low = record_credits(&mut state, &settings, "a", "acct", 8.5, 0).unwrap();
        assert!(low
            .lines
            .contains(&("剩余积分 / Credits left", "8.50".into())));
        assert!(record_credits(&mut state, &settings, "a", "acct", 2.0, 99_999).is_none());
        assert!(record_credits(&mut state, &settings, "a", "acct", 20.0, 99_999).is_none());
        assert!(record_credits(&mut state, &settings, "a", "acct", 1.0, 99_999).is_some());
    }

    #[test]
    fn alerts_webhook_flavors_and_payloads() {
        assert_eq!(
            flavor("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=k"),
            Flavor::WeCom
        );
        assert_eq!(
            flavor("https://oapi.dingtalk.com/robot/send?access_token=t"),
            Flavor::DingTalk
        );
        assert_eq!(
            flavor("https://open.feishu.cn/open-apis/bot/v2/hook/x"),
            Flavor::Feishu
        );
        assert_eq!(
            flavor("https://open.larksuite.com/open-apis/bot/v2/hook/x"),
            Flavor::Feishu
        );
        assert_eq!(
            flavor("https://hooks.slack.com/services/a/b/c"),
            Flavor::Slack
        );
        assert_eq!(
            flavor("https://discord.com/api/webhooks/1/x"),
            Flavor::Discord
        );
        assert_eq!(
            flavor("https://sctapi.ftqq.com/SCT1.send"),
            Flavor::ServerChan
        );
        assert_eq!(flavor("https://api.day.app/key"), Flavor::Generic);
        assert_eq!(flavor("https://notdingtalk.com/x"), Flavor::Generic);

        let wecom = payload(Flavor::WeCom, "S", "B");
        assert_eq!(wecom["text"]["content"], "S\nB");
        assert_eq!(payload(Flavor::Feishu, "S", "B")["content"]["text"], "S\nB");
        let sc = payload(Flavor::ServerChan, &"标".repeat(40), "a\nb");
        assert_eq!(sc["title"].as_str().unwrap().chars().count(), 32);
        assert_eq!(sc["desp"], "a\n\nb");
        let generic = payload(Flavor::Generic, "S", "B");
        assert_eq!(
            (generic["title"].as_str(), generic["body"].as_str()),
            (Some("S"), Some("B"))
        );
    }
}
