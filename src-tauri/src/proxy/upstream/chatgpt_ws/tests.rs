use super::transcript::{Rebuild, Transcript, TRANSCRIPT_LIMIT};
use serde_json::{json, Value};

fn user(text: &str) -> Value {
    json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
}
fn assistant(text: &str) -> Value {
    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
}
fn reasoning(cipher: &str) -> Value {
    json!({"type":"reasoning","summary":[],"encrypted_content":cipher})
}

/// Turn 1 (`u1` → `r1: a1`) and turn 2 continuing `r1` (`u2` → `r2: a2`).
fn two_turns(limit: usize) -> Transcript {
    let mut t = Transcript::new(limit);
    t.begin(None);
    t.record_request(&[user("u1")]);
    t.record_response("r1", &[assistant("a1")]);
    t.begin(Some("r1"));
    t.record_request(&[user("u2")]);
    t.record_response("r2", &[assistant("a2")]);
    t
}

#[test]
fn rebuild_includes_history_up_to_reference_plus_new_input() {
    let t = two_turns(TRANSCRIPT_LIMIT);
    assert_eq!(
        t.rebuild("r1", &[user("u3")]),
        Rebuild::Full(vec![user("u1"), assistant("a1"), user("u3")])
    );
    assert_eq!(
        t.rebuild("r2", &[user("u3")]),
        Rebuild::Full(vec![
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2"),
            user("u3")
        ])
    );
    // A branch from r1 replaces what followed it.
    let mut t = two_turns(TRANSCRIPT_LIMIT);
    t.begin(Some("r1"));
    t.record_request(&[user("u2b")]);
    t.record_response("r2b", &[assistant("a2b")]);
    assert_eq!(
        t.rebuild("r2b", &[]),
        Rebuild::Full(vec![
            user("u1"),
            assistant("a1"),
            user("u2b"),
            assistant("a2b")
        ])
    );
    assert_eq!(t.rebuild("r2", &[]), Rebuild::Unknown);
}

#[test]
fn rebuild_strips_encrypted_reasoning_and_compaction() {
    let call = json!({"type":"function_call","call_id":"c1","name":"ls","arguments":"{}"});
    let output = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
    let mut t = Transcript::new(TRANSCRIPT_LIMIT);
    t.begin(None);
    t.record_request(&[
        user("u1"),
        json!({"type":"compaction","encrypted_content":"cmp"}),
    ]);
    t.record_response("r1", &[reasoning("enc-1"), call.clone()]);
    let Rebuild::Full(items) = t.rebuild("r1", &[reasoning("enc-2"), output.clone()]) else {
        panic!("expected a full rebuild");
    };
    assert_eq!(items, vec![user("u1"), call, output]);
    assert!(!Value::Array(items)
        .to_string()
        .contains("encrypted_content"));
}

#[test]
fn rebuild_unknown_id_is_unknown() {
    let t = two_turns(TRANSCRIPT_LIMIT);
    assert_eq!(t.rebuild("resp_elsewhere", &[user("u3")]), Rebuild::Unknown);
    // A turn continuing an unknown response is not recorded.
    let mut t = two_turns(TRANSCRIPT_LIMIT);
    t.begin(Some("resp_elsewhere"));
    t.record_request(&[user("u3")]);
    t.record_response("r3", &[assistant("a3")]);
    assert_eq!(t.rebuild("r3", &[]), Rebuild::Unknown);
    // A full turn starts a new conversation.
    let mut t = two_turns(TRANSCRIPT_LIMIT);
    t.begin(None);
    t.record_request(&[user("x")]);
    t.record_response("r9", &[assistant("y")]);
    assert_eq!(t.rebuild("r1", &[]), Rebuild::Unknown);
    assert_eq!(
        t.rebuild("r9", &[]),
        Rebuild::Full(vec![user("x"), assistant("y")])
    );
}

#[test]
fn rebuild_orphaned_output_is_refused() {
    let orphan = json!({"type":"function_call_output","call_id":"c9","output":"ok"});
    let t = two_turns(TRANSCRIPT_LIMIT);
    assert_eq!(t.rebuild("r2", &[orphan.clone()]), Rebuild::Orphaned);
    let mut t = Transcript::new(TRANSCRIPT_LIMIT);
    t.begin(None);
    t.record_request(&[user("u1"), orphan]);
    t.record_response("r1", &[assistant("a1")]);
    assert_eq!(t.rebuild("r1", &[user("u2")]), Rebuild::Orphaned);
    let custom = json!({"type":"custom_tool_call_output","call_id":"c8","output":"ok"});
    assert_eq!(t.rebuild("r1", &[custom]), Rebuild::Orphaned);
}

#[test]
fn memory_bound_drops_oldest_and_reports_dropped() {
    let item = user(&"x".repeat(100));
    let size = item.to_string().len();
    let mut t = Transcript::new(size * 3);
    t.begin(None);
    t.record_request(&[item.clone()]);
    t.record_response("r1", &[item.clone()]);
    assert!(matches!(t.rebuild("r1", &[]), Rebuild::Full(v) if v.len() == 2));
    t.begin(Some("r1"));
    t.record_request(&[item.clone()]);
    t.record_response("r2", &[item.clone()]);
    assert_eq!(t.len(), 3, "the oldest item is dropped above the limit");
    assert_eq!(t.rebuild("r2", &[user("next")]), Rebuild::Dropped);
    assert_eq!(t.rebuild("r1", &[]), Rebuild::Dropped);
    // A fresh conversation is whole again.
    t.begin(None);
    t.record_request(&[user("u")]);
    t.record_response("r3", &[assistant("a")]);
    assert_eq!(
        t.rebuild("r3", &[]),
        Rebuild::Full(vec![user("u"), assistant("a")])
    );
}

#[test]
fn rejected_digests_strip_matching_items_and_cap_at_512() {
    let mut t = Transcript::new(TRANSCRIPT_LIMIT);
    t.begin(None);
    t.record_request(&[user("u1")]);
    t.record_response("r1", &[reasoning("enc-x"), assistant("a1")]);
    t.begin(Some("r1"));
    t.record_request(&[user("u2")]);
    t.record_response("r2", &[assistant("a2")]);
    assert_eq!(t.len(), 5);
    t.reject(&[reasoning("enc-x"), user("no cipher")]);
    assert_eq!(t.len(), 4, "rejected ciphertext leaves the transcript");
    assert_eq!(
        t.rebuild("r1", &[]),
        Rebuild::Full(vec![user("u1"), assistant("a1")])
    );
    assert_eq!(
        t.rebuild("r2", &[]),
        Rebuild::Full(vec![
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2")
        ])
    );
    let mut later = vec![reasoning("enc-x"), reasoning("enc-y"), user("u3")];
    t.strip_rejected(&mut later);
    assert_eq!(later, vec![reasoning("enc-y"), user("u3")]);
    // Recorded later turns drop rejected ciphertexts too.
    t.begin(Some("r2"));
    t.record_request(&[reasoning("enc-x"), user("u3")]);
    assert_eq!(t.len(), 5);

    let mut t = Transcript::new(TRANSCRIPT_LIMIT);
    let rejected: Vec<Value> = (0..513).map(|n| reasoning(&format!("enc-{n}"))).collect();
    t.reject(&rejected);
    let mut items = vec![reasoning("enc-0"), reasoning("enc-1"), reasoning("enc-512")];
    t.strip_rejected(&mut items);
    assert_eq!(
        items,
        vec![reasoning("enc-0")],
        "the oldest digest is evicted"
    );
}
