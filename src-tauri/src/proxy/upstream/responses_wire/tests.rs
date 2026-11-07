use super::*;
use serde_json::{json, Value};

fn parse(chunks: &[String]) -> Vec<Value> {
    chunks
        .iter()
        .filter(|c| *c != "[DONE]")
        .map(|c| serde_json::from_str(c).unwrap())
        .collect()
}

#[test]
fn from_chat_maps_roles_tools_and_options() {
    let out = from_chat(json!({
        "model": "gpt-5",
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": 100,
        "max_completion_tokens": 100,
        "n": 1,
        "temperature": 0.5,
        "parallel_tool_calls": false,
        "reasoning_effort": "high",
        "response_format": {"type":"json_schema","json_schema":{"name":"out","schema":{"type":"object"},"strict":true}},
        "messages": [
            {"role":"system","content":"be brief"},
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":"calling","reasoning_content":"secret","tool_calls":[
                {"id":"c1","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]},
            {"role":"tool","tool_call_id":"c1","content":"done"}
        ],
        "tools": [{"type":"function","function":{"name":"f","description":"d","parameters":{"type":"object"},"strict":true}}],
        "tool_choice": {"type":"function","function":{"name":"f"}}
    }))
    .unwrap();
    assert_eq!(out["model"], "gpt-5");
    assert_eq!(out["stream"], true);
    assert_eq!(out["parallel_tool_calls"], false);
    assert_eq!(out["temperature"], 0.5);
    for dropped in [
        "stream_options",
        "max_tokens",
        "max_completion_tokens",
        "n",
        "messages",
    ] {
        assert!(out.get(dropped).is_none(), "{dropped} must be dropped");
    }
    assert_eq!(
        out["input"],
        json!([
            {"type":"message","role":"system","content":[{"type":"input_text","text":"be brief"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"calling"}]},
            {"type":"function_call","call_id":"c1","name":"f","arguments":"{\"a\":1}"},
            {"type":"function_call_output","call_id":"c1","output":"done"}
        ])
    );
    assert_eq!(
        out["tools"],
        json!([{"type":"function","name":"f","description":"d","parameters":{"type":"object"},"strict":true}])
    );
    assert_eq!(out["tool_choice"], json!({"type":"function","name":"f"}));
    assert_eq!(out["reasoning"], json!({"effort":"high","summary":"auto"}));
    assert_eq!(
        out["text"]["format"],
        json!({"type":"json_schema","name":"out","schema":{"type":"object"},"strict":true})
    );
}

#[test]
fn from_chat_passes_string_tool_choice_and_json_object() {
    let out = from_chat(json!({
        "model":"m","messages":[{"role":"user","content":"x"}],
        "tool_choice":"required","response_format":{"type":"json_object"}
    }))
    .unwrap();
    assert_eq!(out["tool_choice"], "required");
    assert_eq!(out["text"]["format"], json!({"type":"json_object"}));
    assert_eq!(out["input"][0]["content"][0]["text"], "x");
}

#[test]
fn from_chat_maps_image_parts_to_input_image() {
    let out = from_chat(json!({
        "model":"m",
        "messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}]
    }))
    .unwrap();
    assert_eq!(
        out["input"][0]["content"][0],
        json!({"type":"input_image","image_url":"data:image/png;base64,AAAA"})
    );
}

#[test]
fn from_chat_rejects_unknown_role() {
    assert!(from_chat(json!({"model":"m","messages":[{"role":"alien","content":"x"}]})).is_err());
}

fn full_stream() -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}),
        json!({"type":"response.output_text.delta","delta":"Hel"}),
        json!({"type":"response.output_text.delta","delta":"lo"}),
        json!({"type":"response.reasoning_summary_text.delta","delta":"think"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"c1","name":"f","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"{\"a\""}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":":1}"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":3}}}}),
    ]
}

#[test]
fn chat_chunks_text_reasoning_tools_and_usage() {
    let mut chunks = ChatChunks::new();
    let mut out = Vec::new();
    for event in full_stream() {
        out.extend(chunks.push(&event).unwrap());
    }
    assert_eq!(out.last().unwrap(), "[DONE]");
    let parsed = parse(&out);
    for c in &parsed {
        assert_eq!(c["object"], "chat.completion.chunk");
        assert_eq!(c["id"], "resp_1");
        assert_eq!(c["model"], "gpt-5");
        assert!(c["created"].as_u64().is_some());
    }
    let content: String = parsed
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Hello");
    let reasoning: String = parsed
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["reasoning_content"].as_str())
        .collect();
    assert_eq!(reasoning, "think");
    let calls: Vec<&Value> = parsed
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"].get(0))
        .collect();
    assert_eq!(calls[0]["index"], 0);
    assert_eq!(calls[0]["id"], "c1");
    assert_eq!(calls[0]["function"]["name"], "f");
    let args: String = calls
        .iter()
        .filter_map(|c| c["function"]["arguments"].as_str())
        .collect();
    assert_eq!(args, "{\"a\":1}");
    assert!(calls[1..].iter().all(|c| c.get("id").is_none()));
    let finish = parsed
        .iter()
        .find(|c| c["choices"][0]["finish_reason"].is_string())
        .unwrap();
    assert_eq!(finish["choices"][0]["finish_reason"], "tool_calls");
    let usage = parsed.last().unwrap();
    assert_eq!(usage["choices"], json!([]));
    assert_eq!(usage["usage"]["prompt_tokens"], 10);
    assert_eq!(usage["usage"]["completion_tokens"], 5);
    assert_eq!(usage["usage"]["total_tokens"], 15);
    assert_eq!(usage["usage"]["prompt_tokens_details"]["cached_tokens"], 3);
}

#[test]
fn chat_chunks_default_ids_and_stop_finish() {
    let mut chunks = ChatChunks::new();
    let out = chunks
        .push(&json!({"type":"response.output_text.delta","delta":"x"}))
        .unwrap();
    let first: Value = serde_json::from_str(&out[0]).unwrap();
    assert_eq!(first["id"], "chatcmpl-responses");
    assert_eq!(first["model"], "");
    let out = chunks
        .push(&json!({"type":"response.completed","response":{"status":"completed"}}))
        .unwrap();
    let parsed = parse(&out);
    assert_eq!(parsed[0]["choices"][0]["finish_reason"], "stop");
    assert_eq!(out.last().unwrap(), "[DONE]");
    assert!(parsed.iter().all(|c| c.get("usage").is_none()));
}

#[test]
fn chat_chunks_function_call_done_backfills_missing_arguments() {
    let mut chunks = ChatChunks::new();
    chunks
        .push(&json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"c1","name":"f"}}))
        .unwrap();
    let out = chunks
        .push(&json!({"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"c1","name":"f","arguments":"{\"z\":1}"}}))
        .unwrap();
    let parsed = parse(&out);
    assert_eq!(
        parsed[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{\"z\":1}"
    );
}

#[test]
fn chat_chunks_incomplete_max_tokens_is_length() {
    let mut chunks = ChatChunks::new();
    let out = chunks
        .push(&json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}))
        .unwrap();
    assert_eq!(parse(&out)[0]["choices"][0]["finish_reason"], "length");
}

#[test]
fn chat_chunks_failed_is_error() {
    let mut chunks = ChatChunks::new();
    let err = chunks
        .push(&json!({"type":"response.failed","response":{"error":{"message":"boom"}}}))
        .unwrap_err();
    assert!(err.contains("boom"));
    let err = ChatChunks::new()
        .push(&json!({"type":"error","message":"bad"}))
        .unwrap_err();
    assert!(err.contains("bad"));
}

#[test]
fn data_only_frames_use_json_type() {
    assert_eq!(
        event_type(&json!({"type":"response.output_text.delta"})),
        Some("response.output_text.delta")
    );
    assert_eq!(event_type(&json!({"x":1})), None);
}

#[test]
fn aggregate_takes_completed_response() {
    let out = aggregate(&[
        json!({"type":"response.created","response":{"id":"r"}}),
        json!({"type":"response.completed","response":{"id":"r","status":"completed","created_at":7,"output":[{"type":"message","id":"m"}]}}),
    ])
    .unwrap();
    assert_eq!(out["id"], "r");
    assert_eq!(out["created_at"], 7);
    assert_eq!(out["output"][0]["id"], "m");
}

#[test]
fn aggregate_rebuilds_empty_output_from_item_done() {
    let item = json!({"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"hi"}]});
    let out = aggregate(&[
        json!({"type":"response.output_item.done","item":item}),
        json!({"type":"response.output_item.done","item":item}),
        json!({"type":"response.done","response":{"status":"completed","output":[]}}),
    ])
    .unwrap();
    assert_eq!(out["output"], json!([item]));
}

#[test]
fn aggregate_rebuilds_from_deltas_when_no_items() {
    let out = aggregate(&[
        json!({"type":"response.reasoning_summary_text.delta","delta":"why"}),
        json!({"type":"response.output_text.delta","delta":"a"}),
        json!({"type":"response.output_text.delta","delta":"b"}),
        json!({"type":"response.completed","response":{"status":"completed"}}),
    ])
    .unwrap();
    let output = out["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["summary"][0]["text"], "why");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "ab");
}

#[test]
fn aggregate_tool_call_only_answer() {
    let call =
        json!({"type":"function_call","id":"fc_1","call_id":"c1","name":"f","arguments":"{}"});
    let out = aggregate(&[
        json!({"type":"response.output_item.done","item":call}),
        json!({"type":"response.completed","response":{"status":"completed","output":[]}}),
    ])
    .unwrap();
    assert_eq!(out["output"], json!([call]));

    let out = aggregate(&[
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_2","call_id":"c2","name":"g","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"{\"a\":"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"1}"}),
        json!({"type":"response.completed","response":{"status":"completed"}}),
    ])
    .unwrap();
    assert_eq!(out["output"][0]["call_id"], "c2");
    assert_eq!(out["output"][0]["arguments"], "{\"a\":1}");
}

#[test]
fn aggregate_failed_is_error() {
    let err = aggregate(&[
        json!({"type":"response.failed","response":{"error":{"message":"nope"}}}),
        json!({"type":"response.completed","response":{}}),
    ])
    .unwrap_err();
    assert!(err.contains("nope"));
    assert!(aggregate(&[json!({"type":"response.created","response":{}})]).is_err());
}

#[test]
fn aggregate_fills_created_at() {
    let out =
        aggregate(&[json!({"type":"response.incomplete","response":{"status":"incomplete"}})])
            .unwrap();
    assert!(out["created_at"].as_u64().unwrap() > 0);
}

#[test]
fn carries_output_detects_output_events() {
    for t in [
        "response.output_text.delta",
        "response.reasoning_summary_text.delta",
        "response.reasoning_text.delta",
        "response.function_call_arguments.delta",
        "response.refusal.delta",
    ] {
        assert!(carries_output(&json!({"type":t,"delta":"x"})), "{t}");
    }
    assert!(carries_output(
        &json!({"type":"response.output_item.added","item":{"type":"function_call"}})
    ));
    assert!(!carries_output(
        &json!({"type":"response.output_item.added","item":{"type":"message"}})
    ));
    assert!(carries_output(
        &json!({"type":"response.completed","response":{"output":[{"type":"message"}]}})
    ));
    assert!(!carries_output(
        &json!({"type":"response.completed","response":{"output":[]}})
    ));
    assert!(!carries_output(
        &json!({"type":"response.created","response":{}})
    ));
    assert!(!carries_output(
        &json!({"type":"response.in_progress","response":{}})
    ));
}
