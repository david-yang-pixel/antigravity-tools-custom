use super::*;
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode, Uri},
    routing::any,
    Json, Router,
};
use std::{collections::HashMap, sync::Mutex};

#[derive(Default)]
struct Upstream {
    writes: Vec<(String, Value)>,
    claimed: HashSet<String>,
    draws: HashMap<String, Value>,
    chances: u64,
    ambiguous_draw: bool,
    reject_reads: bool,
    reject_draw: bool,
    disable_on_claim: Option<String>,
}
struct Mock {
    data: Arc<Mutex<Upstream>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
        std::env::remove_var("STELLAR_TEST_WORKBUDDY_BASE");
    }
}
impl Mock {
    async fn new() -> Self {
        let data = Arc::new(Mutex::new(Upstream::default()));
        let state = data.clone();
        let app = Router::new().fallback(any(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let state = state.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer test-access");
                assert_eq!(headers["x-user-id"], "activity-test");
                let mut s = state.lock().unwrap();
                if method == Method::GET && s.reject_reads {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"code":401,"msg":"test-access forbidden"})));
                }
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let path = uri.path();
                if method == Method::POST { s.writes.push((path.into(), body.clone())); }
                let value = match path {
                    api::TASKS => json!({"tasks":[
                        {"task_code":"ready","task_name":"Completed task","accept_status":if s.claimed.contains("task") {"claimed"} else {"completed"},"progress":{"current":1,"target":1}},
                        {"task_code":"locked","accept_status":"locked","progress":{"current":99,"target":1}},
                        {"task_code":"todo","accept_status":"in_progress","progress":{"current":0,"target":1}}
                    ]}),
                    api::STREAK => json!({"streak":{"days":7},"redemption_status":{"tier_7d_status":if s.claimed.contains("streak") {"claimed"} else {"available"},"tier_14d_status":"locked","tiers":[{"tier":"7d","credit":7}]}}),
                    api::TRAVEL => if s.claimed.contains("travel") { json!({"state":"idle"}) } else { json!({"state":"arrived","record_id":42,"reward_credit":9}) },
                    api::CHANCES => json!({"balance":s.chances}),
                    "/activity/growth/tasks/ready/claim" => {
                        s.claimed.insert("task".into());
                        if let Some(id) = s.disable_on_claim.take() { accounts::set_enabled(&id, false).unwrap(); }
                        json!({"credit":5})
                    },
                    "/activity/growth/redeem" => { assert_eq!(body["tier"], "7d"); s.claimed.insert("streak".into()); json!({"credit_granted":7}) },
                    "/activity/growth/buddy/travel/claim" => { assert_eq!(body["record_id"], 42); s.claimed.insert("travel".into()); json!({"reward_credit":9}) },
                    "/activity/growth/lottery/draw" => {
                        if s.reject_draw {
                            return (StatusCode::BAD_REQUEST, Json(json!({"code":1,"msg":"insufficient lottery chance balance"})));
                        }
                        let token = body["client_token"].as_str().unwrap().to_string();
                        if let Some(result) = s.draws.get(&token) { result.clone() }
                        else {
                            assert!(s.chances > 0);
                            s.chances -= 1;
                            let value = json!({"credit_amount":2,"prize_name":"Two credits"});
                            s.draws.insert(token, value.clone());
                            if s.ambiguous_draw {
                                s.ambiguous_draw = false;
                                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"code":500,"msg":"test-access uncertain"})));
                            }
                            value
                        }
                    },
                    "/v2/billing/meter/get-user-resource" => json!({"Accounts":[{"CapacityRemain":123.0}]}),
                    _ => panic!("unexpected activity request: {method} {path}"),
                };
                (StatusCode::OK, Json(json!({"code":0,"data":value})))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STELLAR_TEST_WORKBUDDY_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { data, task }
    }
}
fn account() -> Account {
    let value = accounts::parse_import(json!({"uid":"activity-test","accessToken":"test-access","expiresAt":accounts::now()+86400})).unwrap();
    let view = accounts::upsert(value).unwrap();
    accounts::get(&view.id).unwrap()
}
async fn run(id: &str, action: Action) -> Account {
    let running = RunGuard::new().unwrap();
    accounts::edit_management(id, |m| {
        m.activity.run_id = running.0.clone();
        m.activity.status = "running".into();
        m.activity.last_attempt_at = accounts::now();
        m.activity.next_run_at = accounts::now() + INTERVAL;
    })
    .unwrap();
    execute(accounts::get(id).unwrap(), action).await;
    accounts::get(id).unwrap()
}

#[test]
fn activity_eligibility_is_explicit_and_legacy_accounts_are_opt_in() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let a = account();
    assert!(!a.management.auto_rewards && !a.management.auto_lottery);
    assert!(!due(&a, accounts::now()));
    let mut a = a;
    a.management.auto_rewards = true;
    assert!(due(&a, accounts::now()));
    a.realm = Realm::Global;
    assert!(!due(&a, accounts::now()));
    assert!(serde_json::from_value::<Action>(json!("automatic")).is_err());
    a.realm = Realm::Cn;
    a.management.activity.last_attempt_at = accounts::now();
    assert!(!due(&a, accounts::now() + 30));
    let entries = tasks(
        &json!({"tasks":[
            {"task_code":"locked","accept_status":"locked","progress":{"current":20,"target":1}},
            {"task_code":"zero","accept_status":"accepted","progress":{"current":0,"target":0}},
            {"task_code":"new","accept_status":"new_unknown_state"}
        ]}),
        &a,
    )
    .unwrap();
    assert!(entries.iter().all(|e| e.status != "available"));
    assert!(tasks(
        &json!({"tasks":[{"task_code":"../oops","accept_status":"completed"}]}),
        &a
    )
    .is_err());
    assert!(tiers(&json!({"streak":{"days":100}})).is_err());
    assert_eq!(
        travel_item(&json!({"state":"arrived"})).unwrap()[0].status,
        "unknown"
    );
    assert!(lottery_item(&json!({"balance":-1})).is_err());
}

#[tokio::test]
async fn activity_query_is_read_only_and_claims_only_eligible_rewards() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mock = Mock::new().await;
    let a = account();
    let queried = run(&a.id, Action::Query).await;
    assert_eq!(queried.management.activity.status, "success");
    assert!(mock.data.lock().unwrap().writes.is_empty());
    let claimed = run(&a.id, Action::Claim).await;
    assert_eq!(claimed.management.activity.status, "success");
    assert_eq!(claimed.management.activity.history.len(), 3);
    assert_eq!(
        claimed
            .management
            .activity
            .history
            .iter()
            .filter_map(|r| r.credits)
            .sum::<f64>(),
        21.0
    );
    assert_eq!(claimed.management.credits.unwrap().available, 123.0);
    assert_eq!(mock.data.lock().unwrap().draws.len(), 0);
    let again = run(&a.id, Action::Claim).await;
    assert_eq!(again.management.activity.history.len(), 3);
    assert_eq!(
        mock.data
            .lock()
            .unwrap()
            .writes
            .iter()
            .filter(|(p, _)| p != "/v2/billing/meter/get-user-resource")
            .count(),
        3
    );
}

#[tokio::test]
async fn activity_draw_retry_keeps_idempotency_key_across_reload() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mock = Mock::new().await;
    {
        let mut s = mock.data.lock().unwrap();
        s.chances = 2;
        s.ambiguous_draw = true;
    }
    let a = account();
    accounts::configure(&a.id, None, None, None, Some(false), Some(true)).unwrap();
    let first = run(&a.id, Action::Automatic).await;
    assert_eq!(first.management.activity.status, "partial");
    assert_eq!(first.management.activity.history[0].status, "unknown");
    let token = first.management.activity.pending_draw.clone().unwrap();
    assert!(accounts::view(first.clone())
        .unwrap()
        .management
        .activity
        .pending_draw
        .is_none());
    assert!(!first
        .management
        .activity
        .error
        .unwrap()
        .contains("test-access"));
    let second = run(&a.id, Action::Automatic).await;
    assert_eq!(second.management.activity.status, "success");
    assert!(second.management.activity.pending_draw.is_none());
    let s = mock.data.lock().unwrap();
    let draws: Vec<_> = s
        .writes
        .iter()
        .filter(|(p, _)| p.ends_with("/draw"))
        .collect();
    assert_eq!(draws.len(), 3); // one ambiguous response, same-key replay, one new draw
    assert_eq!(draws[0].1["client_token"], token);
    assert_eq!(draws[1].1["client_token"], token);
    assert_ne!(draws[2].1["client_token"], token);
    assert_eq!(s.draws.len(), 2);
    assert_eq!(s.chances, 0);
    assert!(s.claimed.is_empty()); // lottery is independent of auto_rewards
}

#[tokio::test]
async fn activity_last_draw_is_reconciled_even_with_zero_remaining_chances() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mock = Mock::new().await;
    {
        let mut s = mock.data.lock().unwrap();
        s.chances = 1;
        s.ambiguous_draw = true;
    }
    let a = account();
    accounts::configure(&a.id, None, None, None, Some(false), Some(true)).unwrap();
    let first = run(&a.id, Action::Automatic).await;
    let token = first.management.activity.pending_draw.unwrap();
    assert_eq!(mock.data.lock().unwrap().chances, 0);
    let before = mock.data.lock().unwrap().writes.len();
    let queried = run(&a.id, Action::Query).await;
    assert_eq!(queried.management.activity.status, "partial");
    assert_eq!(mock.data.lock().unwrap().writes.len(), before);
    // An eligibility rejection does not confirm what happened to the previous draw.
    mock.data.lock().unwrap().reject_draw = true;
    let rejected = run(&a.id, Action::Automatic).await;
    assert_eq!(rejected.management.activity.status, "partial");
    assert_eq!(
        rejected.management.activity.pending_draw.as_deref(),
        Some(token.as_str())
    );
    assert_eq!(rejected.management.activity.history.len(), 1);
    assert_eq!(rejected.management.activity.history[0].status, "unknown");
    mock.data.lock().unwrap().reject_draw = false;
    let confirmed = run(&a.id, Action::Automatic).await;
    assert_eq!(confirmed.management.activity.status, "success");
    assert!(confirmed.management.activity.pending_draw.is_none());
    assert_eq!(confirmed.management.activity.history.len(), 1);
    assert_eq!(confirmed.management.activity.history[0].status, "claimed");
    let s = mock.data.lock().unwrap();
    assert_eq!(s.draws.len(), 1);
    assert!(s
        .writes
        .iter()
        .filter(|(p, _)| p.ends_with("/draw"))
        .all(|(_, v)| v["client_token"] == token));
}

#[test]
fn activity_interrupted_run_recovers_without_losing_pending_draw_or_preferences() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let a = account();
    let running = RunGuard::new().unwrap();
    accounts::edit_management(&a.id, |m| {
        m.activity.status = "running".into();
        m.activity.run_id = running.0.clone();
        m.activity.pending_draw = Some("same-draw-after-restart".into());
    })
    .unwrap();
    assert_eq!(
        accounts::get(&a.id).unwrap().management.activity.status,
        "running"
    );
    drop(running); // Simulate the persisted journal outliving its process/worker.
    let recovered = accounts::get(&a.id).unwrap();
    assert_eq!(recovered.management.activity.status, "partial");
    assert!(recovered
        .management
        .activity
        .error
        .unwrap()
        .contains("interrupted"));
    assert_eq!(
        recovered.management.activity.pending_draw.as_deref(),
        Some("same-draw-after-restart")
    );
    assert!(!recovered.management.auto_rewards && !recovered.management.auto_lottery);
    assert_eq!(
        accounts::list().unwrap()[0].management.activity.status,
        "partial"
    );
}

#[test]
fn activity_journal_survives_reauthorization_but_cannot_overwrite_a_new_run() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let a = account();
    accounts::edit_management(&a.id, |m| m.activity.run_id = "old-job".into()).unwrap();
    let a = accounts::get(&a.id).unwrap();
    let replacement = accounts::parse_import(json!({"uid":"activity-test","accessToken":"replacement-token","expiresAt":accounts::now()+86400})).unwrap();
    accounts::upsert(replacement).unwrap();
    assert!(current(&a, Action::Claim).is_err());
    let mut state = a.management.activity.clone();
    state.status = "partial".into();
    accounts::save_activity(&a, state.clone()).unwrap();
    assert_eq!(
        accounts::get(&a.id).unwrap().access_token,
        "replacement-token"
    );
    accounts::edit_management(&a.id, |m| m.activity.run_id = "new-job".into()).unwrap();
    assert!(accounts::save_activity(&a, state).is_err());
}

#[tokio::test]
async fn activity_disable_stops_remaining_writes_and_preserves_preferences() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mock = Mock::new().await;
    let a = account();
    accounts::configure(&a.id, None, None, None, Some(true), Some(false)).unwrap();
    mock.data.lock().unwrap().disable_on_claim = Some(a.id.clone());
    let result = run(&a.id, Action::Automatic).await;
    assert!(!result.enabled);
    assert_eq!(result.management.activity.status, "partial");
    assert_eq!(mock.data.lock().unwrap().writes.len(), 1);
    assert!(result.management.auto_rewards);
    // A stale maintenance observation cannot overwrite a newly changed preference/journal.
    let mut stale = a;
    accounts::save_observation(&mut stale).unwrap();
    let saved = accounts::get(&stale.id).unwrap();
    assert!(saved.management.auto_rewards);
    assert_eq!(saved.management.activity.history.len(), 1);
}

#[tokio::test]
async fn activity_auth_errors_never_trigger_claims_and_concurrent_jobs_do_not_queue() {
    let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    let mock = Mock::new().await;
    let a = account();
    mock.data.lock().unwrap().reject_reads = true;
    let result = run(&a.id, Action::Claim).await;
    assert_eq!(result.management.activity.status, "failed");
    assert!(result.management.activity.history.is_empty());
    // Only the informational balance refresh can POST after failed read-only queries.
    assert!(mock
        .data
        .lock()
        .unwrap()
        .writes
        .iter()
        .all(|(p, _)| p == "/v2/billing/meter/get-user-resource"));
    mock.data.lock().unwrap().reject_reads = false;
    let first = start(&a.id, Action::Query).unwrap();
    assert_eq!(first.management.activity.status, "running");
    assert!(start(&a.id, Action::Query).is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        while accounts::get(&a.id).unwrap().management.activity.status == "running" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
