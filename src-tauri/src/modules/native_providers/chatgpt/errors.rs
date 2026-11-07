//! Classifies upstream answers into the shared lease outcomes.
use super::quota::exhausted_until;
use crate::modules::native_providers::http::{Failure, Penalty};
use reqwest::header::HeaderMap;
use serde_json::{json, Value};

const QUOTA_EXHAUSTED: &str = "Account quota exhausted";
const SHORT_COOLDOWN: u64 = 5;
const OVERLOADED_CODES: [&str; 2] = ["server_is_overloaded", "slow_down"];
const OVERLOADED_TEXT: [&str; 5] = [
    "an error occurred while processing your request",
    "you can retry your request",
    "selected model is at capacity",
    "servers are currently overloaded",
    "server is overloaded",
];
const CLOUDFLARE_TEXT: [&str; 4] = [
    "just a moment",
    "error code: 1010",
    "cloudflare_error",
    "browser_signature_banned",
];

/// Error codes found at `error.code`, `error.type`, `response.error.code`, `detail.code`, `code`.
fn codes(body: &Value) -> Vec<&str> {
    [
        &body["error"]["code"],
        &body["error"]["type"],
        &body["response"]["error"]["code"],
        &body["detail"]["code"],
        &body["code"],
    ]
    .into_iter()
    .filter_map(Value::as_str)
    .collect()
}

/// `deactivated_workspace`, `workspace_deactivated`, `account_suspended`, ... in either order.
fn access_state(code: &str) -> bool {
    const SUBJECT: [&str; 4] = ["workspace", "account", "organization", "org"];
    const STATE: [&str; 3] = ["deactivated", "disabled", "suspended"];
    code.split_once('_').is_some_and(|(a, b)| {
        (SUBJECT.contains(&a) && STATE.contains(&b)) || (STATE.contains(&a) && SUBJECT.contains(&b))
    })
}

fn message(body: &Value, raw: &str) -> String {
    [
        &body["error"]["message"],
        &body["response"]["error"]["message"],
        &body["detail"]["message"],
        &body["detail"],
        &body["message"],
    ]
    .into_iter()
    .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
    .map(String::from)
    .unwrap_or_else(|| raw.chars().take(500).collect())
}

fn overloaded(codes: &[&str], text: &str) -> bool {
    let text = text.to_lowercase();
    codes.iter().any(|c| OVERLOADED_CODES.contains(c))
        || OVERLOADED_TEXT.iter().any(|t| text.contains(t))
}

fn model_not_found(codes: &[&str], text: &str) -> bool {
    let text = text.to_lowercase();
    codes.contains(&"model_not_found")
        || (text.contains("model")
            && (text.contains("does not exist") || text.contains("not found")))
}

/// Validation, content-policy and context-window errors belong to the request, not the account.
fn request_fault(codes: &[&str], text: &str) -> bool {
    let text = text.to_lowercase();
    codes.iter().any(|c| {
        matches!(*c, "invalid_request_error" | "context_length_exceeded") || c.contains("policy")
    }) || text.contains("maximum context length")
        || text.contains("context length")
}

fn cloudflare(headers: &HeaderMap, raw: &str) -> bool {
    let lower = raw.to_lowercase();
    let challenged = headers
        .get("cf-mitigated")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_lowercase().contains("challenge"));
    let html = {
        let head = lower.trim_start();
        head.starts_with("<!doctype html") || head.starts_with("<html")
    };
    challenged || html || CLOUDFLARE_TEXT.iter().any(|t| lower.contains(t))
}

pub fn classify(status: u16, headers: &HeaderMap, body: &str, at: i64) -> Failure {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let codes = codes(&json);
    let text = message(&json, body);
    let fail =
        |status, penalty, failover| Failure::classified(status, text.clone(), penalty, failover);
    let disable = || Penalty::Disable {
        message: text.clone(),
    };
    if codes.iter().any(|c| access_state(c)) {
        return fail(status, disable(), true);
    }
    match status {
        401 => {
            let revoked = codes
                .iter()
                .any(|c| matches!(*c, "token_revoked" | "token_invalidated"))
                || json["detail"] == "Unauthorized";
            let penalty = if revoked {
                disable()
            } else {
                Penalty::Breaker { retry_after: None }
            };
            fail(401, penalty, true)
        }
        402 => fail(402, disable(), true),
        403 if cloudflare(headers, body) => fail(502, Penalty::Neutral, true),
        403 => fail(403, Penalty::Breaker { retry_after: None }, true),
        429 => {
            let penalty = match exhausted_until(headers, &json, at) {
                Some(until) => Penalty::Until {
                    at: until,
                    message: QUOTA_EXHAUSTED,
                },
                None => Penalty::Breaker {
                    retry_after: Some(
                        headers
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok()?.trim().parse().ok())
                            .unwrap_or(SHORT_COOLDOWN),
                    ),
                },
            };
            fail(429, penalty, true)
        }
        400 if overloaded(&codes, &text) => fail(503, Penalty::Neutral, true),
        400 if model_not_found(&codes, &text) => fail(400, Penalty::Neutral, true),
        // Server-side trouble and unsupported methods are not the account's fault.
        405 | 500.. => fail(status, Penalty::Neutral, true),
        // Context-length, policy and other validation errors belong to the request.
        _ => fail(status, Penalty::Neutral, false),
    }
}

/// An `error` or `response.failed` event received inside a stream. Events carry no HTTP
/// status, so one is inferred from the code and the HTTP rules are applied.
pub fn classify_event(event: &Value, at: i64) -> Failure {
    let error = event
        .get("error")
        .or_else(|| event.pointer("/response/error"))
        .cloned()
        .unwrap_or(Value::Null);
    let body = json!({ "error": error });
    let found = codes(&body);
    let text = message(&body, "");
    let status = if found.iter().any(|c| access_state(c)) {
        403
    } else if found
        .iter()
        .any(|c| matches!(*c, "usage_limit_reached" | "rate_limit_exceeded"))
    {
        429
    } else if found
        .iter()
        .any(|c| matches!(*c, "token_revoked" | "token_invalidated"))
    {
        401
    } else if overloaded(&found, &text)
        || model_not_found(&found, &text)
        || request_fault(&found, &text)
    {
        400
    } else {
        502
    };
    classify(status, &HeaderMap::new(), &body.to_string(), at)
}
