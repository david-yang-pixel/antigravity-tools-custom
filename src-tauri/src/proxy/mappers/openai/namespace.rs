//! Responses `namespace` tools (Codex groups, MCP servers such as
//! `mcp__openconnector`) travel upstream as flat function names. The client
//! registered each tool as a (namespace, name) pair, so calls must return in that
//! form and replayed calls must use the same flat name as the tool list.
use serde_json::{json, Value};
use std::collections::HashMap;

/// Gateway-wide flat name for a namespaced tool.
pub fn qualify(namespace: &str, name: &str) -> String {
    let child = name.trim();
    let ns = namespace.trim();
    if child.is_empty() || ns.is_empty() || child.starts_with("mcp__") {
        return child.to_string();
    }
    if child.starts_with(ns) {
        return child.to_string();
    }
    if ns.ends_with("__") {
        return format!("{}{}", ns, child);
    }
    format!("{}__{}", ns, child)
}

/// Flat upstream name -> (namespace, client name) for one request's tools.
#[derive(Clone, Debug, Default)]
pub struct NamespaceMap(HashMap<String, (String, String)>);

impl NamespaceMap {
    /// Built from a Responses `tools` array; only one namespace level is mapped.
    pub fn from_tools(tools: Option<&Value>) -> Self {
        let mut map = HashMap::new();
        for tool in tools.and_then(Value::as_array).into_iter().flatten() {
            let Some(namespace) = tool["name"]
                .as_str()
                .filter(|_| tool["type"] == "namespace")
            else {
                continue;
            };
            for child in tool["tools"].as_array().into_iter().flatten() {
                if let Some(name) = child["name"].as_str() {
                    map.insert(qualify(namespace, name), (namespace.into(), name.into()));
                }
            }
        }
        Self(map)
    }

    /// Exact match only: the namespace this request registered for `flat`.
    pub fn get(&self, flat: &str) -> Option<&(String, String)> {
        self.0.get(flat)
    }

    /// Client (name, namespace) for a flat upstream tool name. Names this request
    /// registered resolve exactly (including `mcp__*` server namespaces); other names
    /// keep the legacy split, so clients that send flat `mcp__server__tool`
    /// functions without namespaces see no change.
    pub fn resolve(&self, flat: &str) -> (String, Option<String>) {
        match self.0.get(flat.trim()) {
            Some((namespace, name)) => (name.clone(), Some(namespace.clone())),
            None => legacy_split(flat),
        }
    }
}

fn legacy_split(qualified_name: &str) -> (String, Option<String>) {
    let name = qualified_name.trim();
    if name.starts_with("mcp__") {
        return (name.to_string(), None);
    }
    if let Some(pos) = name.find("__") {
        if pos > 0 {
            return (name[pos + 2..].to_string(), Some(name[..pos].to_string()));
        }
    }
    (name.to_string(), None)
}

/// Folds `namespace` on replayed Responses calls into the flat upstream name, so
/// history matches the flattened tool list on every conversion path.
pub fn qualify_input_calls(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        if !matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call")
        ) {
            continue;
        }
        let Some(namespace) = item
            .get("namespace")
            .and_then(Value::as_str)
            .filter(|ns| !ns.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            item["name"] = json!(qualify(&namespace, name));
        }
        if let Some(obj) = item.as_object_mut() {
            obj.remove("namespace");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_tools() -> Value {
        json!([
            {"type":"function","name":"exec_command"},
            {"type":"namespace","name":"multi_agent_v1","tools":[{"type":"function","name":"spawn_agent"}]},
            {"type":"namespace","name":"mcp__openconnector","tools":[
                {"type":"function","name":"list_connections"},
                {"type":"function","name":"list_apps"}
            ]}
        ])
    }

    #[test]
    fn mcp_namespace_calls_resolve_to_client_pairs() {
        let map = NamespaceMap::from_tools(Some(&codex_tools()));
        assert_eq!(
            map.resolve("mcp__openconnector__list_connections"),
            ("list_connections".into(), Some("mcp__openconnector".into()))
        );
        assert_eq!(
            map.resolve("multi_agent_v1__spawn_agent"),
            ("spawn_agent".into(), Some("multi_agent_v1".into()))
        );
        // Unregistered names keep the legacy behavior.
        assert_eq!(map.resolve("exec_command"), ("exec_command".into(), None));
        assert_eq!(
            map.resolve("mcp__legacy__flat_tool"),
            ("mcp__legacy__flat_tool".into(), None)
        );
        assert_eq!(
            NamespaceMap::default().resolve("ns__tool"),
            ("tool".into(), Some("ns".into()))
        );
    }

    #[test]
    fn replayed_namespace_calls_use_flat_names() {
        let mut body = json!({"input":[
            {"type":"function_call","call_id":"c1","namespace":"mcp__openconnector","name":"list_apps","arguments":"{}"},
            {"type":"function_call","call_id":"c2","name":"exec_command","arguments":"{}"},
            {"type":"function_call_output","call_id":"c1","output":"[]"}
        ]});
        qualify_input_calls(&mut body);
        assert_eq!(body["input"][0]["name"], "mcp__openconnector__list_apps");
        assert!(body["input"][0].get("namespace").is_none());
        assert_eq!(body["input"][1]["name"], "exec_command");
        // Round trip: the flat history name resolves back to the client pair.
        let map = NamespaceMap::from_tools(Some(&codex_tools()));
        assert_eq!(
            map.resolve(body["input"][0]["name"].as_str().unwrap()),
            ("list_apps".into(), Some("mcp__openconnector".into()))
        );
    }
}
