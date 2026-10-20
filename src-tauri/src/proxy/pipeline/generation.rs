//! Protocol-independent generation parameter compatibility for Antigravity.
use serde_json::Value;
const PENALTIES: [&str; 2] = ["presencePenalty", "frequencyPenalty"];

pub fn normalize_penalties(body: &mut Value) {
    if let Some(config) = body
        .pointer_mut("/request/generationConfig")
        .and_then(Value::as_object_mut)
    {
        for key in PENALTIES {
            if config.get(key).and_then(Value::as_f64) == Some(0.0) {
                config.remove(key);
            }
        }
    }
}
pub fn has_penalties(body: &Value) -> bool {
    body.pointer("/request/generationConfig")
        .and_then(Value::as_object)
        .is_some_and(|config| PENALTIES.iter().any(|key| config.contains_key(*key)))
}
pub fn remove_rejected_penalties(body: &mut Value, error: &Value) -> bool {
    let rejected = error
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| {
            message
                .trim()
                .trim_end_matches('.')
                .eq_ignore_ascii_case("Penalty is not enabled for this model")
        });
    if !rejected {
        return false;
    }
    let Some(config) = body
        .pointer_mut("/request/generationConfig")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let mut removed = false;
    for key in PENALTIES {
        removed |= config.remove(key).is_some();
    }
    removed
}

/// Inspect only a bounded error prefix and replay it for callers when unrelated.
/// Successful SSE responses never pass through this buffer.
pub async fn inspect_error(mut response: rquest::Response) -> (rquest::Response, Option<Value>) {
    use futures::StreamExt;
    let mut bytes = Vec::new();
    let mut chunks = Vec::new();
    let mut complete = false;
    while bytes.len() <= 64 * 1024 {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if chunk.len() > 64 * 1024 - bytes.len() {
                    chunks.push(Ok(chunk));
                    break;
                }
                bytes.extend_from_slice(&chunk);
                chunks.push(Ok(chunk));
            }
            Ok(None) => {
                complete = true;
                break;
            }
            Err(error) => {
                chunks.push(Err(std::io::Error::other(error.to_string())));
                break;
            }
        }
    }
    let error = if complete {
        serde_json::from_slice(&bytes).ok()
    } else {
        None
    };
    let response: axum::http::Response<rquest::Body> = response.into();
    let (parts, remaining) = response.into_parts();
    let stream = futures::stream::iter(chunks).chain(
        axum::body::Body::new(remaining)
            .into_data_stream()
            .map(|chunk| chunk.map_err(|error| std::io::Error::other(error.to_string()))),
    );
    (
        rquest::Response::from(axum::http::Response::from_parts(
            parts,
            rquest::Body::wrap_stream(stream),
        )),
        error,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn penalty_compat_only_drops_zero_or_explicitly_rejected_fields() {
        let original = json!({"model":"future-model", "project":"same-project","request":{"contents":[{"text":"presencePenalty"}],"generationConfig":{"presencePenalty":0.0,"frequencyPenalty":0.5,"temperature":0.7}}});
        let mut body = original.clone();
        normalize_penalties(&mut body);
        assert!(body["request"]["generationConfig"]
            .get("presencePenalty")
            .is_none());
        assert_eq!(body["request"]["generationConfig"]["frequencyPenalty"], 0.5);
        assert!(!remove_rejected_penalties(
            &mut body,
            &json!({"error":{"message":"Invalid contents"}})
        ));
        assert!(remove_rejected_penalties(
            &mut body,
            &json!({"error":{"message":"Penalty is not enabled for this model."}})
        ));
        assert!(!has_penalties(&body));
        assert_eq!(body["model"], original["model"]);
        assert_eq!(body["project"], original["project"]);
        assert_eq!(body["request"]["contents"], original["request"]["contents"]);
        assert_eq!(body["request"]["generationConfig"]["temperature"], 0.7);
    }
    #[tokio::test]
    async fn penalty_compat_replays_unrelated_error_response_and_bounds_inspection() {
        for content in [
            r#"{"error":{"message":"Invalid contents"}}"#.to_string(),
            "x".repeat(70 * 1024),
        ] {
            let response = rquest::Response::from(
                axum::http::Response::builder()
                    .status(400)
                    .header("x-test", "preserved")
                    .body(content.clone())
                    .unwrap(),
            );
            let (response, value) = inspect_error(response).await;
            assert_eq!(response.status(), 400);
            assert_eq!(response.headers()["x-test"], "preserved");
            if content.len() > 64 * 1024 {
                assert!(value.is_none());
            }
            assert_eq!(response.text().await.unwrap(), content);
        }
    }
}
