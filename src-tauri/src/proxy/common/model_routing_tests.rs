use super::{resolve_user_model_route, ModelRoutingMode, DYNAMIC_MODEL_FORWARDING_RULES};
use crate::proxy::mappers::{claude, gemini, openai};
use serde_json::json;

#[test]
fn exact_model_ids_survive_all_four_protocol_adapters() {
    for model in [
        "gemini-2.5-flash",
        "gemini-2.5-pro",
        "gemini-3-flash",
        "gemini-3-flash-preview",
        "claude-sonnet-4-5",
        "Future-Upstream-online",
    ] {
        let mapping = [(model.to_owned(), "different-model".to_owned())]
            .into_iter()
            .collect();
        let mapped = ModelRoutingMode::Exact.resolve(model, &mapping, Some("low"));
        assert_eq!(mapped, model);
        let request: openai::OpenAIRequest = serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{"type": "function", "function": {
                "name": "lookup", "parameters": {"type": "object", "properties": {}}
            }}]
        }))
        .unwrap();
        for responses in [false, true] {
            let (body, _, _, _) = openai::transform_openai_request_with_routing(
                &request,
                "test-project",
                &mapped,
                None,
                "exact-model-test",
                None,
                responses,
                ModelRoutingMode::Exact,
            );
            assert_eq!(body["model"], model);
            assert!(body["request"]["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()));
        }
        let request: claude::ClaudeRequest = serde_json::from_value(json!({
            "model": model, "messages": [{"role": "user", "content": "Hello"}]
        }))
        .unwrap();
        let (body, _) = claude::transform_claude_request_with_routing(
            &request,
            "test-project",
            false,
            None,
            "exact-model-test",
            "exact-model-test",
            None,
            ModelRoutingMode::Exact,
        )
        .unwrap();
        assert_eq!(body["model"], model);
        let body = gemini::wrap_request_with_routing(
            &json!({"contents": [{"role":"user","parts":[{"text":"Hello"}]}]}),
            "test-project",
            model,
            None,
            None,
            None,
            None,
            None,
            None,
            ModelRoutingMode::Exact,
        );
        assert_eq!(body["model"], model);
    }
}

#[test]
fn exact_model_mode_bypasses_automatic_and_custom_routes() {
    let id = "exact-model-deprecated-test";
    DYNAMIC_MODEL_FORWARDING_RULES.insert(id.into(), "gemini-3.1-flash-lite".into());
    let mapping = [(id.into(), "explicit-target".into())]
        .into_iter()
        .collect();
    assert_eq!(ModelRoutingMode::Exact.resolve(id, &mapping, None), id);
    assert_eq!(
        resolve_user_model_route(id, &mapping).as_deref(),
        Some("explicit-target")
    );
    assert_eq!(
        ModelRoutingMode::Compatible.resolve(id, &mapping, None),
        "gemini-3.1-flash-lite"
    );
    DYNAMIC_MODEL_FORWARDING_RULES.remove(id);
    assert_eq!(
        ModelRoutingMode::Compatible.resolve("gemini-2.5-pro", &Default::default(), None),
        "gemini-pro-agent"
    );
}
