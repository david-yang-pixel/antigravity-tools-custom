//! Candidate discovery and inference tests never publish models. Publication is an admin action.
use super::{account, model_verification, workbuddy};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Mutex,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Antigravity,
    Workbuddy,
}

#[derive(Serialize)]
pub struct Candidate {
    pub id: String,
    pub status: &'static str,
    pub checked_at: Option<i64>,
    pub added: bool,
}

type Selection = BTreeMap<String, Provider>;
static LOCK: Mutex<()> = Mutex::new(());

fn path() -> Result<std::path::PathBuf, String> {
    Ok(account::get_data_dir()?.join("published_models.json"))
}
fn read() -> Result<Selection, String> {
    match std::fs::read(path()?) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| "Invalid published model selection".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Selection::new()),
        Err(_) => Err("Cannot read published model selection".into()),
    }
}
pub fn selected_provider(model: &str) -> Option<Provider> {
    read().ok()?.get(model).copied()
}
pub fn managed_models() -> Result<Vec<String>, String> {
    Ok(read()?.into_keys().collect())
}

#[derive(Default)]
struct Aggregate {
    accounts: usize,
    verified: usize,
    unsupported: usize,
    checked_at: Option<i64>,
}
impl Aggregate {
    fn observe(&mut self, verified: bool, unsupported: bool, checked_at: Option<i64>) {
        self.accounts += 1;
        self.verified += usize::from(verified);
        self.unsupported += usize::from(unsupported);
        self.checked_at = self.checked_at.max(checked_at);
    }
    fn status(&self) -> &'static str {
        if self.verified > 0 {
            "verified"
        } else if self.accounts > 0 && self.unsupported == self.accounts {
            "unsupported"
        } else if self.checked_at.is_some() {
            "inconclusive"
        } else {
            "untested"
        }
    }
}

pub fn candidates(provider: Provider) -> Result<Vec<Candidate>, String> {
    let selected = read()?;
    let mut models: BTreeMap<String, Aggregate> = BTreeMap::new();
    match provider {
        Provider::Antigravity => {
            for account in account::list_accounts()?
                .into_iter()
                .filter(model_verification::eligible)
            {
                let value = serde_json::to_value(&account).map_err(|e| e.to_string())?;
                let verified = model_verification::verified_model_ids(&value);
                let evidence = model_verification::current_evidence(&value);
                if let Some(quota) = account.quota {
                    for model in quota.models {
                        let observation = evidence.as_ref().and_then(|e| e.models.get(&model.name));
                        models.entry(model.name.clone()).or_default().observe(
                            verified.contains(&model.name),
                            observation.is_some_and(|o| {
                                o.outcome == model_verification::Outcome::Unsupported
                            }),
                            observation.map(|o| o.checked_at),
                        );
                    }
                }
            }
        }
        Provider::Workbuddy => {
            let settings = workbuddy::load()?;
            let tested = workbuddy::verification::status()?.tested_models;
            for account in workbuddy::accounts::all()?.into_iter().filter(|account| {
                account.enabled
                    && settings
                        .preferred_account_id
                        .as_ref()
                        .is_none_or(|id| id == &account.id)
            }) {
                let mut routes: BTreeMap<String, String> = account
                    .models
                    .iter()
                    .map(|model| (model.id.clone(), model.id.clone()))
                    .collect();
                routes.extend(settings.models.clone());
                for (alias, target) in &routes {
                    if account.models.iter().any(|model| &model.id == target) {
                        models.entry(alias.clone()).or_default().observe(
                            account.verified_models.contains_key(target)
                                && !account.unavailable_models.contains(target),
                            account.unavailable_models.contains(target),
                            account
                                .verified_models
                                .get(target)
                                .copied()
                                .or_else(|| tested.get(target).copied())
                                .or_else(|| {
                                    account
                                        .unavailable_models
                                        .contains(target)
                                        .then_some(account.models_verified_at)
                                })
                                .map(|time| time * 1000),
                        );
                    }
                }
            }
        }
    }
    // Keep manually added models removable even after an account/catalog disappears.
    for (model, source) in &selected {
        if *source == provider {
            models.entry(model.clone()).or_default();
        }
    }
    Ok(models
        .into_iter()
        .map(|(id, evidence)| Candidate {
            added: selected.get(&id) == Some(&provider),
            id,
            status: evidence.status(),
            checked_at: evidence.checked_at,
        })
        .collect())
}

pub async fn fetch(provider: Provider, refresh: bool) -> Result<Vec<Candidate>, String> {
    if refresh {
        tokio::time::timeout(std::time::Duration::from_secs(90), async {
            match provider {
                Provider::Antigravity => {
                    let stats = account::refresh_all_quotas_logic().await?;
                    if stats.success == 0 && stats.failed > 0 {
                        return Err("Could not refresh Antigravity candidate models".into());
                    }
                }
                Provider::Workbuddy => {
                    let settings = workbuddy::load()?;
                    for account in workbuddy::accounts::all()?.into_iter().filter(|account| {
                        account.enabled
                            && settings
                                .preferred_account_id
                                .as_ref()
                                .is_none_or(|id| id == &account.id)
                    }) {
                        workbuddy::accounts::sync(&account.id, false).await?;
                    }
                }
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|_| "Fetching models timed out; try again".to_string())??;
    }
    candidates(provider)
}

pub fn edit(
    provider: Provider,
    add: Vec<String>,
    remove: Vec<String>,
) -> Result<Vec<Candidate>, String> {
    if add.len() > 500 || remove.len() > 500 {
        return Err("At most 500 models per edit".into());
    }
    let verified: HashSet<_> = candidates(provider)?
        .into_iter()
        .filter(|row| row.status == "verified")
        .map(|row| row.id)
        .collect();
    if add.iter().any(|id| !verified.contains(id)) {
        return Err(
            "Only successfully tested models can be added. Test the selected models first.".into(),
        );
    }
    {
        let _guard = LOCK.lock().map_err(|_| "Model selection unavailable")?;
        let mut selected = read()?;
        if add
            .iter()
            .any(|id| selected.get(id).is_some_and(|source| *source != provider))
        {
            return Err(
                "This model ID is already added from the other provider. Remove it there first."
                    .into(),
            );
        }
        for id in remove {
            if selected.get(&id) == Some(&provider) {
                selected.remove(&id);
            }
        }
        for id in add {
            selected.insert(id, provider);
        }
        let bytes = serde_json::to_vec_pretty(&selected).map_err(|e| e.to_string())?;
        crate::utils::fs::write_atomic(path()?, &bytes)
            .map_err(|_| "Cannot save model selection")?;
    }
    candidates(provider)
}

pub fn workbuddy_verified(model: &str, mapping: &HashMap<String, String>) -> bool {
    let Ok(settings) = workbuddy::load() else {
        return false;
    };
    if !settings.enabled {
        return false;
    }
    let Some(target) = settings.target_model(model, mapping) else {
        return false;
    };
    workbuddy::accounts::all().is_ok_and(|accounts| {
        accounts.iter().any(|account| {
            account.enabled
                && settings
                    .preferred_account_id
                    .as_ref()
                    .is_none_or(|id| id == &account.id)
                && account.supports_model(&target)
                && account.verified_models.contains_key(target)
        })
    })
}

pub fn published(
    antigravity: HashSet<String>,
    mapping: &HashMap<String, String>,
) -> Result<Vec<String>, String> {
    let selected = read()?;

    Ok(selected
        .into_iter()
        .filter_map(|(id, provider)| {
            let available = match provider {
                Provider::Antigravity => antigravity.contains(&id),
                Provider::Workbuddy => workbuddy_verified(&id, mapping),
            };
            available.then_some(id)
        })
        .collect())
}

pub fn test_status(provider: Provider) -> Result<serde_json::Value, String> {
    match provider {
        Provider::Antigravity => {
            serde_json::to_value(model_verification::status()?).map_err(|e| e.to_string())
        }
        Provider::Workbuddy => {
            let status = workbuddy::verification::status()?;
            Ok(serde_json::json!({
                "running":status.running, "total":status.total, "completed":status.completed,
                "verified":status.available, "unsupported":status.unavailable, "inconclusive":status.failed,
                "current_model":status.current_model, "started_at":status.started_at, "finished_at":status.finished_at,
                "errors":status.errors
            }))
        }
    }
}

pub fn start_test(
    provider: Provider,
    model_ids: Vec<String>,
    manager: std::sync::Arc<crate::proxy::TokenManager>,
    upstream: std::sync::Arc<crate::proxy::upstream::client::UpstreamClient>,
) -> Result<serde_json::Value, String> {
    let candidates: HashSet<_> = candidates(provider)?
        .into_iter()
        .map(|row| row.id)
        .collect();
    if model_ids.is_empty()
        || model_ids.len() > 500
        || model_ids.iter().any(|id| !candidates.contains(id))
    {
        return Err("Select candidate models to test".into());
    }
    match provider {
        Provider::Antigravity => {
            model_verification::start(manager, upstream, None, Some(model_ids))?;
        }
        Provider::Workbuddy => {
            let settings = workbuddy::load()?;
            let targets = model_ids
                .into_iter()
                .map(|id| settings.models.get(&id).cloned().unwrap_or(id))
                .collect();
            workbuddy::verification::start_selected(Some(targets))?;
        }
    }
    test_status(provider)
}

#[cfg(test)]
pub(crate) fn seed_selection(models: &[(&str, Provider)]) {
    std::fs::write(
        path().unwrap(),
        serde_json::to_vec(
            &models
                .iter()
                .map(|(id, provider)| (id.to_string(), *provider))
                .collect::<Selection>(),
        )
        .unwrap(),
    )
    .unwrap();
}

#[cfg(test)]
mod tests;
