//! The account's own Codex model catalog.
use super::{client, endpoints, errors};
use crate::modules::native_providers::{http::Failure, now, Account, Model};
use serde_json::Value;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

pub fn parse(v: &Value) -> Result<Vec<Model>, Failure> {
    let list = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| Failure::new(502, "ChatGPT model list has an unexpected shape"))?;
    let mut out: Vec<Model> = Vec::new();
    for item in list {
        let slug = item
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let skipped = slug.is_empty()
            || slug.contains('*')
            || item["visibility"] == "hide"
            || item["supported_in_api"] == false
            || out.iter().any(|m| m.id == slug);
        if skipped {
            continue;
        }
        let context = |key: &str| item.get(key).and_then(Value::as_u64).filter(|n| *n > 0);
        out.push(Model {
            id: slug.into(),
            name: item
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(slug)
                .into(),
            context_window: context("context_window")
                .or_else(|| context("max_context_window"))
                .unwrap_or(0),
            vision: item["input_modalities"]
                .as_array()
                .is_some_and(|m| m.iter().any(|m| m == "image")),
            reasoning: item["supported_reasoning_levels"]
                .as_array()
                .is_some_and(|l| !l.is_empty()),
            is_free: false,
        });
    }
    Ok(out)
}

pub async fn fetch(client: &reqwest::Client, a: &Account) -> Result<Vec<Model>, Failure> {
    let mut url = endpoints::url(&endpoints::current().backend, "/codex/models")?;
    url.query_pairs_mut()
        .append_pair("client_version", &super::headers::client_version());
    let mut h = client::headers(a, "", &Default::default(), client::Endpoint::Models)?;
    h.remove("session_id");
    h.remove("conversation_id");
    let response = client
        .get(url)
        .headers(h)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| Failure::sending(&e, "ChatGPT model list connection failed"))?;
    let status = response.status().as_u16();
    let response_headers = response.headers().clone();
    let body = client::read_bytes(response).await?;
    if !(200..300).contains(&status) {
        return Err(errors::classify(
            status,
            &response_headers,
            &String::from_utf8_lossy(&body),
            now(),
        ));
    }
    let json: Value = serde_json::from_slice(&body)
        .map_err(|_| Failure::new(502, "ChatGPT model list returned invalid JSON"))?;
    parse(&json)
}
