//! Built-in WorkBuddy accounts and model routes. No external Manager is required.
pub mod accounts;
pub mod client;
pub mod login;
pub mod maintenance;
pub mod resources;

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{LazyLock, Mutex},
};

static SETTINGS_LOCK: Mutex<()> = Mutex::new(());
// Classification only: a broken configuration must never execute cached settings.
static LAST_VALID_ROUTES: LazyLock<Mutex<HashMap<PathBuf, BTreeSet<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    /// Client-visible alias -> native WorkBuddy model ID.
    pub models: BTreeMap<String, String>,
    /// WorkBuddy-only pin; independent from the Google account pool.
    pub preferred_account_id: Option<String>,
}
#[derive(Default, Deserialize)]
pub struct SettingsUpdate {
    pub enabled: Option<bool>,
    pub models: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "nullable_account_id")]
    pub preferred_account_id: Option<Option<String>>,
}
fn nullable_account_id<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

impl Settings {
    pub fn target_model(
        &self,
        model: &str,
        mapping: &std::collections::HashMap<String, String>,
    ) -> Option<&str> {
        self.models
            .get(model)
            .or_else(|| {
                crate::proxy::common::model_mapping::resolve_custom_model_route(model, mapping)
                    .and_then(|alias| self.models.get(&alias))
            })
            .map(String::as_str)
    }
    fn validate(&self) -> Result<(), String> {
        if self.models.len() > 500 {
            return Err("WorkBuddy supports at most 500 routes".into());
        }
        for (alias, target) in &self.models {
            if !valid_model_id(alias) || alias.contains('/') || !valid_model_id(target) {
                return Err("WorkBuddy model IDs must contain 1-200 ASCII letters, digits, '.', '_', '-' or '/' (client names cannot contain '/')".into());
            }
        }
        Ok(())
    }
}
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
}
fn settings_path() -> Result<PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join("workbuddy.json"))
}
fn stored() -> Result<Settings, String> {
    match std::fs::read(settings_path()?) {
        Ok(data) => serde_json::from_slice(&data).map_err(|_| "Invalid workbuddy.json".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(_) => Err("Cannot read WorkBuddy settings".into()),
    }
}
fn overrides(settings: &mut Settings) -> Result<Vec<&'static str>, String> {
    let mut locked = Vec::new();
    if let Ok(value) = std::env::var("ABV_WORKBUDDY_ENABLED") {
        settings.enabled = match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err("ABV_WORKBUDDY_ENABLED must be true or false".into()),
        };
        locked.push("enabled");
    }
    if let Ok(value) = std::env::var("ABV_WORKBUDDY_MODELS") {
        settings.models = serde_json::from_str(&value)
            .map_err(|_| "ABV_WORKBUDDY_MODELS must be a JSON object")?;
        locked.push("models");
    }
    Ok(locked)
}
pub fn load() -> Result<Settings, String> {
    let mut settings = stored()?;
    overrides(&mut settings)?;
    settings.validate()?;
    remember_routes(&settings);
    Ok(settings)
}
fn remember_routes(settings: &Settings) {
    if let (Ok(path), Ok(mut routes)) = (settings_path(), LAST_VALID_ROUTES.lock()) {
        routes.insert(path, settings.models.keys().cloned().collect());
    }
}

/// Used only after configuration loading fails. Known WorkBuddy routes fail closed,
/// while unrelated providers remain available. Never infer enablement from this data.
pub fn known_model_on_configuration_error(model: &str, mapping: &HashMap<String, String>) -> bool {
    let Ok(path) = settings_path() else {
        return false;
    };
    let mut names = LAST_VALID_ROUTES
        .lock()
        .ok()
        .and_then(|routes| routes.get(&path).cloned())
        .unwrap_or_default();
    // Recover route keys even when another field or one target is invalid.
    let mut collect = |value: Option<serde_json::Value>| {
        if let Some(serde_json::Value::Object(models)) = value {
            names.extend(
                models
                    .into_iter()
                    .map(|(name, _)| name)
                    .filter(|name| valid_model_id(name)),
            );
        }
    };
    collect(
        std::fs::read(path)
            .ok()
            .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
            .and_then(|mut value| value.get_mut("models").map(serde_json::Value::take)),
    );
    collect(
        std::env::var("ABV_WORKBUDDY_MODELS")
            .ok()
            .and_then(|value| serde_json::from_str(&value).ok()),
    );
    if let Ok(accounts) = accounts::all() {
        names.extend(
            accounts
                .into_iter()
                .flat_map(|account| account.models.into_iter().map(|model| model.id)),
        );
    }
    names.contains(model)
        || crate::proxy::common::model_mapping::resolve_custom_model_route(model, mapping)
            .is_some_and(|alias| names.contains(&alias))
}
#[derive(Serialize)]
pub struct SettingsView {
    pub enabled: bool,
    pub models: BTreeMap<String, String>,
    pub available_models: Vec<String>,
    pub account_count: usize,
    pub preferred_account_id: Option<String>,
    pub env_locked: Vec<&'static str>,
}
pub fn view() -> Result<SettingsView, String> {
    let mut settings = stored()?;
    let env_locked = overrides(&mut settings)?;
    settings.validate()?;
    remember_routes(&settings);
    let accounts = accounts::list()?;
    let mut models: Vec<String> = accounts
        .iter()
        .filter(|a| a.enabled)
        .flat_map(|a| a.models.iter().map(|m| m.id.clone()))
        .collect();
    models.sort();
    models.dedup();
    Ok(SettingsView {
        enabled: settings.enabled,
        models: settings.models,
        available_models: models,
        account_count: accounts.len(),
        preferred_account_id: settings.preferred_account_id,
        env_locked,
    })
}
fn persist(settings: &Settings) -> Result<(), String> {
    settings.validate()?;
    let data =
        serde_json::to_vec_pretty(settings).map_err(|_| "Cannot serialize WorkBuddy settings")?;
    crate::utils::fs::write_atomic(settings_path()?, &data)
        .map_err(|_| "Cannot save WorkBuddy settings".to_string())?;
    let mut effective = settings.clone();
    if overrides(&mut effective).is_ok() && effective.validate().is_ok() {
        remember_routes(&effective);
    }
    Ok(())
}
pub fn save(update: SettingsUpdate) -> Result<SettingsView, String> {
    {
        let _lock = SETTINGS_LOCK
            .lock()
            .map_err(|_| "WorkBuddy settings unavailable")?;
        let mut settings = stored()?;
        let mut effective = settings.clone();
        let locked = overrides(&mut effective)?;
        if !locked.contains(&"enabled") {
            if let Some(enabled) = update.enabled {
                settings.enabled = enabled;
            }
        }
        if !locked.contains(&"models") {
            if let Some(models) = update.models {
                settings.models = models;
            }
        }
        if let Some(preferred) = update.preferred_account_id {
            if let Some(id) = preferred.as_ref() {
                let account = accounts::get(id)?;
                if !account.enabled {
                    return Err("Enable the WorkBuddy account before selecting it".into());
                }
            }
            settings.preferred_account_id = preferred;
        }
        effective = settings.clone();
        overrides(&mut effective)?;
        effective.validate()?;
        persist(&settings)?;
    }
    view()
}
/// Discover new aliases without replacing an administrator's existing routes.
pub fn register_models(models: &[accounts::Model]) -> Result<(), String> {
    if models.is_empty() {
        return Ok(());
    }
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| "WorkBuddy settings unavailable")?;
    let first_sync = !settings_path()?
        .try_exists()
        .map_err(|_| "Cannot read WorkBuddy settings")?;
    let mut settings = stored()?;
    for model in models {
        let alias = model.id.clone();
        if settings.models.len() < 500 && valid_model_id(&alias) && !alias.contains('/') {
            settings
                .models
                .entry(alias)
                .or_insert_with(|| model.id.clone());
        }
    }
    if first_sync {
        settings.enabled = true;
    }
    persist(&settings)
}
pub fn extend_catalog(models: &mut Vec<String>) {
    if let Ok(settings) = load() {
        if settings.enabled {
            models.extend(settings.models.into_keys());
        }
    }
    models.sort();
    models.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workbuddy_model_sync_preserves_explicitly_disabled_empty_settings() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        save(serde_json::from_value(serde_json::json!({"enabled":false,"models":{}})).unwrap())
            .unwrap();
        register_models(&[accounts::Model {
            id: "native-model".into(),
            name: "Model".into(),
        }])
        .unwrap();
        let settings = load().unwrap();
        assert!(!settings.enabled);
        assert_eq!(settings.models["native-model"], "native-model");
    }

    #[test]
    fn workbuddy_broken_settings_classification_preserves_provider_and_directory_isolation() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let settings = Settings {
            models: BTreeMap::from([("custom-wb".into(), "native-model".into())]),
            ..Default::default()
        };
        persist(&settings).unwrap();
        load().unwrap();
        let mut account =
            accounts::parse_import(serde_json::json!({"uid":"test","accessToken":"test"})).unwrap();
        account.models.push(accounts::Model {
            id: "native-model".into(),
            name: "Model".into(),
        });
        accounts::upsert(account).unwrap();
        std::fs::write(settings_path().unwrap(), b"{").unwrap();
        assert!(load().is_err());
        let mapping = HashMap::from([("client-alias".into(), "custom-wb".into())]);
        for known in ["custom-wb", "client-alias", "native-model"] {
            assert!(known_model_on_configuration_error(known, &mapping));
        }
        assert!(!known_model_on_configuration_error(
            "gemini-2.5-flash",
            &mapping
        ));
        {
            let _other = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
            assert!(!known_model_on_configuration_error("custom-wb", &mapping));
            assert!(!known_model_on_configuration_error(
                "native-model",
                &mapping
            ));
        }
        assert!(known_model_on_configuration_error("custom-wb", &mapping));
    }

    #[test]
    fn workbuddy_invalid_environment_uses_known_routes_without_enabling_them() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        struct RestoreEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                match &self.0 {
                    Some(value) => std::env::set_var("ABV_WORKBUDDY_MODELS", value),
                    None => std::env::remove_var("ABV_WORKBUDDY_MODELS"),
                }
            }
        }
        let _restore = RestoreEnv(std::env::var_os("ABV_WORKBUDDY_MODELS"));
        std::env::set_var("ABV_WORKBUDDY_MODELS", r#"{"env-wb":"native-model"}"#);
        persist(&Settings {
            enabled: false,
            models: BTreeMap::from([("stored-wb".into(), "native-model".into())]),
            ..Default::default()
        })
        .unwrap();
        assert!(!load().unwrap().enabled);
        std::env::set_var("ABV_WORKBUDDY_MODELS", "{");
        assert!(load().is_err());
        let mapping = HashMap::new();
        assert!(known_model_on_configuration_error("env-wb", &mapping));
        assert!(known_model_on_configuration_error("stored-wb", &mapping));
        assert!(!known_model_on_configuration_error(
            "gemini-2.5-flash",
            &mapping
        ));
        // An invalid target must still classify the alias as WorkBuddy, even on cold start.
        std::env::set_var("ABV_WORKBUDDY_MODELS", r#"{"invalid-target-wb":42}"#);
        assert!(load().is_err());
        assert!(known_model_on_configuration_error(
            "invalid-target-wb",
            &mapping
        ));
        assert!(!stored().unwrap().enabled);
    }

    #[test]
    fn workbuddy_native_names_and_partial_settings_preserve_routes() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        register_models(&[accounts::Model {
            id: "glm-5.3-flash".into(),
            name: "GLM".into(),
        }])
        .unwrap();
        let view = view().unwrap();
        assert!(view.enabled);
        assert_eq!(view.models.get("glm-5.3-flash").unwrap(), "glm-5.3-flash");
        assert!(!view.models.contains_key("wb-glm-5.3-flash"));
        let account = accounts::upsert(
            accounts::parse_import(serde_json::json!({"uid":"test", "accessToken":"test"}))
                .unwrap(),
        )
        .unwrap();
        save(
            serde_json::from_value(serde_json::json!({"preferred_account_id":account.id})).unwrap(),
        )
        .unwrap();
        save(serde_json::from_value(serde_json::json!({"enabled":false})).unwrap()).unwrap();
        let saved = load().unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.models, view.models);
        assert_eq!(
            saved.preferred_account_id.as_deref(),
            Some(account.id.as_str())
        );
        save(serde_json::from_value(serde_json::json!({"preferred_account_id":null})).unwrap())
            .unwrap();
        assert!(load().unwrap().preferred_account_id.is_none());
    }
}
