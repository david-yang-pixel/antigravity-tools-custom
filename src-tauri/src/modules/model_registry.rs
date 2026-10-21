//! Explicit public model bindings. Discovery and basic-text evidence never grant publication.
//! This module performs local metadata/file reads only; upstream probes belong to the caller.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::modules::{account, workbuddy};
use crate::utils::fs::{file_stamp, write_atomic, FileStamp};

pub const ANTIGRAVITY_SOURCE: &str = "antigravity:default";
pub const WORKBUDDY_SOURCE: &str = "workbuddy:default";
pub const MAX_PUBLISHED_MODELS: usize = 5_000;
pub const MAX_CANDIDATES: usize = 10_000;
pub const MAX_VERIFICATION_RESULTS: usize = 20_000;
pub const MAX_VERIFY_CANDIDATES: usize = 5;
pub const VERIFY_TIMEOUT_SECONDS: u64 = 30;
const SCHEMA_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ID_BYTES: usize = 512;
const MAX_RECORD_ID_BYTES: usize = 2048;
const MAX_SAFE_REVISION: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelCapability {
    pub key: String,
    pub label: String,
    pub status: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelSource {
    pub id: String,
    pub provider_type: String,
    pub display_name: String,
    pub enabled: bool,
    pub account_count: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidateModel {
    pub id: String,
    pub source_id: String,
    pub provider_type: String,
    pub native_model: String,
    pub display_name: String,
    pub account_count: usize,
    pub verified_account_count: usize,
    pub verification_status: String,
    pub last_verified_at: Option<i64>,
    pub ignored: bool,
    pub capabilities: Vec<ModelCapability>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishedModel {
    pub id: String,
    pub public_id: String,
    pub display_name: String,
    pub source_id: String,
    pub native_model: String,
    pub enabled: bool,
    pub legacy: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistryView {
    pub schema_version: u32,
    pub revision: u64,
    pub sources: Vec<ModelSource>,
    pub candidates: Vec<CandidateModel>,
    pub published_models: Vec<PublishedModel>,
    pub notices: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveModelRegistryRequest {
    pub revision: u64,
    pub published_models: Vec<PublishedModel>,
    pub ignored_candidate_ids: Vec<String>,
    #[serde(default)]
    pub allow_unverified: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshModelCandidatesRequest {
    #[serde(default)]
    pub source_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyModelCandidatesRequest {
    pub candidate_ids: Vec<String>,
    pub confirm_upstream_usage: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelVerificationResult {
    pub candidate_id: String,
    pub status: String,
    pub message: String,
    pub checked_at: i64,
    pub account_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelVerificationReport {
    pub results: Vec<ModelVerificationResult>,
    pub registry: ModelRegistryView,
}

// Only routing identities and sanitized text evidence are persisted, never account objects,
// upstream errors, prompts, quota payloads, credentials, or user-token database rows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CandidateRecord {
    id: String,
    source_id: String,
    native_model: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StoredRegistry {
    schema_version: u32,
    revision: u64,
    candidates: Vec<CandidateRecord>,
    published_models: Vec<PublishedModel>,
    ignored_candidate_ids: Vec<String>,
    verification_results: Vec<ModelVerificationResult>,
}
struct Snapshot {
    stamp: FileStamp,
    store: StoredRegistry,
    published_index: HashMap<String, usize>,
}
struct Cached {
    path: PathBuf,
    snapshot: Arc<Snapshot>,
}
// Serializes read/modify/write operations as well as lookups. A single path-tagged entry
// avoids unbounded caches when changing data directories and cannot leak between test dirs.
static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

#[derive(Default, Clone)]
struct Evidence {
    declared: bool,
    declared_vision: Option<bool>,
    declared_reasoning: Option<bool>,
    status: Option<&'static str>,
    checked_at: i64,
}
#[derive(Default)]
struct Observation {
    accounts: BTreeMap<String, Evidence>,
}
struct Discovery {
    settings: workbuddy::Settings,
    sources: Vec<ModelSource>,
    records: BTreeMap<String, CandidateRecord>,
    observations: HashMap<String, Observation>,
    // Account identifiers are transient and never serialized as discovery data.
    all_accounts: HashMap<String, HashSet<String>>,
    eligible_accounts: HashMap<String, HashSet<String>>,
}

/// Length-independent, reversible source/native encoding; no delimiter ambiguity or hashing
/// collisions, and no case folding. Callers must not parse IDs to determine a route.
pub fn candidate_id(source_id: &str, native_model: &str) -> String {
    format!(
        "candidate:{}:{}",
        URL_SAFE_NO_PAD.encode(source_id),
        URL_SAFE_NO_PAD.encode(native_model)
    )
}
fn provider(source: &str) -> Result<&'static str, String> {
    match source {
        ANTIGRAVITY_SOURCE => Ok("antigravity"),
        WORKBUDDY_SOURCE => Ok("workbuddy"),
        _ => Err("Unknown model source".into()),
    }
}
fn normalized_public_id(value: &str) -> &str {
    value.strip_prefix("models/").unwrap_or(value)
}
fn valid_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '*' | '?' | '[' | ']' | '{' | '}')
        })
}
fn validate_native(source: &str, native: &str) -> Result<(), String> {
    provider(source)?;
    if !valid_id(native, MAX_ID_BYTES)
        || (source == WORKBUDDY_SOURCE && !workbuddy::valid_model_id(native))
    {
        return Err("Invalid native model ID for source".into());
    }
    Ok(())
}
fn valid_account_id(value: &str) -> bool {
    value.len() <= 256
        && !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b))
}
fn verification_message(status: &str) -> Result<&'static str, String> {
    match status {
        "verified" => {
            Ok("Basic text probe succeeded; reasoning, vision and tools were not tested.")
        }
        "unavailable" => {
            Ok("Basic text target was unavailable; advanced capabilities were not tested.")
        }
        "failed" => Ok("Basic text probe failed; this does not prove the target is unsupported."),
        _ => Err("Invalid verification status".into()),
    }
}
fn validate_result(result: &ModelVerificationResult) -> Result<(), String> {
    if result.checked_at <= 0
        || result.message != verification_message(&result.status)?
        || (result.status != "failed" && result.account_id.is_none())
        || result
            .account_id
            .as_deref()
            .is_some_and(|id| !valid_account_id(id))
    {
        return Err("Invalid or unsanitized verification evidence".into());
    }
    Ok(())
}
fn validate_store(store: &StoredRegistry) -> Result<(), String> {
    if store.schema_version != SCHEMA_VERSION {
        return Err("Unsupported model registry schema version".into());
    }
    if store.revision == 0 || store.revision > MAX_SAFE_REVISION {
        return Err("Invalid model registry revision".into());
    }
    if store.candidates.len() > MAX_CANDIDATES
        || store.published_models.len() > MAX_PUBLISHED_MODELS
        || store.ignored_candidate_ids.len() > MAX_CANDIDATES
        || store.verification_results.len() > MAX_VERIFICATION_RESULTS
    {
        return Err("Model registry collection limit exceeded".into());
    }
    let mut candidates = HashSet::new();
    for candidate in &store.candidates {
        validate_native(&candidate.source_id, &candidate.native_model)?;
        if candidate.id != candidate_id(&candidate.source_id, &candidate.native_model)
            || !candidates.insert(candidate.id.as_str())
        {
            return Err("Invalid or duplicate candidate ID".into());
        }
    }
    let mut record_ids = HashSet::new();
    let mut public_ids = HashSet::new();
    for model in &store.published_models {
        validate_native(&model.source_id, &model.native_model)?;
        if !valid_id(&model.id, MAX_RECORD_ID_BYTES)
            || !record_ids.insert(model.id.as_str())
            || !valid_id(&model.public_id, MAX_ID_BYTES)
            || normalized_public_id(&model.public_id) != model.public_id
            || !public_ids.insert(model.public_id.as_str())
            || model.display_name.trim().is_empty()
            || model.display_name.len() > 256
            || model.display_name.chars().any(char::is_control)
            || !candidates.contains(candidate_id(&model.source_id, &model.native_model).as_str())
        {
            return Err("Invalid or duplicate publication/source/target".into());
        }
    }
    let mut ignored = HashSet::new();
    for id in &store.ignored_candidate_ids {
        if !candidates.contains(id.as_str()) || !ignored.insert(id) {
            return Err("Invalid or duplicate ignored candidate ID".into());
        }
    }
    let mut probes = HashSet::new();
    for result in &store.verification_results {
        validate_result(result)?;
        if !candidates.contains(result.candidate_id.as_str())
            || !probes.insert((&result.candidate_id, &result.account_id))
        {
            return Err("Invalid or duplicate verification candidate/account".into());
        }
    }
    Ok(())
}
fn registry_path() -> Result<PathBuf, String> {
    Ok(account::get_data_dir()?.join("model_registry.json"))
}
fn load(path: &Path, cache: &mut Option<Cached>) -> Result<Option<Arc<Snapshot>>, String> {
    let stamp = match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE_BYTES => {
            file_stamp(path).ok_or("Cannot inspect model registry")?
        }
        Ok(_) => {
            *cache = None;
            return Err("Invalid model registry file or file size limit exceeded".into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            *cache = None;
            return Ok(None);
        }
        Err(_) => {
            *cache = None;
            return Err("Cannot read model registry".into());
        }
    };
    if let Some(entry) = cache.as_ref() {
        if entry.path == path && entry.snapshot.stamp == stamp {
            return Ok(Some(Arc::clone(&entry.snapshot)));
        }
    }
    *cache = None;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| "Cannot read model registry")?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Model registry file size limit exceeded".into());
    }
    let store: StoredRegistry =
        serde_json::from_slice(&bytes).map_err(|_| "Corrupt model_registry.json; access denied")?;
    validate_store(&store).map_err(|error| format!("Invalid model_registry.json: {error}"))?;
    if file_stamp(path).as_ref() != Some(&stamp) {
        return Err("Model registry conflict: file changed while reading; reload and retry".into());
    }
    let published_index = store
        .published_models
        .iter()
        .enumerate()
        .map(|(index, model)| (model.public_id.clone(), index))
        .collect();
    let snapshot = Arc::new(Snapshot {
        stamp,
        store,
        published_index,
    });
    *cache = Some(Cached {
        path: path.to_path_buf(),
        snapshot: Arc::clone(&snapshot),
    });
    Ok(Some(snapshot))
}
fn persist(
    path: &Path,
    store: &StoredRegistry,
    expected: Option<&Snapshot>,
    cache: &mut Option<Cached>,
) -> Result<(), String> {
    validate_store(store)?;
    let bytes = serde_json::to_vec_pretty(store).map_err(|_| "Cannot encode model registry")?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Model registry file size limit exceeded".into());
    }
    // Catch out-of-process rewrites since this operation's read, not just API revisions.
    let unchanged = match expected {
        Some(previous) => file_stamp(path).as_ref() == Some(&previous.stamp),
        None => !path
            .try_exists()
            .map_err(|_| "Cannot inspect model registry")?,
    };
    if !unchanged {
        *cache = None;
        return Err("Model registry revision conflict: reload and retry".into());
    }
    write_atomic(path, &bytes).map_err(|_| "Cannot atomically save model registry")?;
    // Do not attach our data to a stamp obtained after an external writer may have run.
    *cache = None;
    Ok(())
}
fn next_revision(store: &mut StoredRegistry) -> Result<(), String> {
    store.revision = store
        .revision
        .checked_add(1)
        .filter(|value| *value <= MAX_SAFE_REVISION)
        .ok_or("Model registry revision exhausted")?;
    Ok(())
}
fn insert_record(
    records: &mut BTreeMap<String, CandidateRecord>,
    source: &str,
    native: &str,
) -> Result<String, String> {
    validate_native(source, native)?;
    let id = candidate_id(source, native);
    if !records.contains_key(&id) && records.len() >= MAX_CANDIDATES {
        return Err("Model candidate limit exceeded".into());
    }
    records
        .entry(id.clone())
        .or_insert_with(|| CandidateRecord {
            id: id.clone(),
            source_id: source.into(),
            native_model: native.into(),
        });
    Ok(id)
}
fn discover() -> Result<Discovery, String> {
    // Invalid WB files/env overrides must not silently become an empty AG-only registry.
    let settings =
        workbuddy::load().map_err(|_| "Invalid or unreadable WorkBuddy configuration")?;
    let ag = account::list_accounts().map_err(|_| "Cannot read Antigravity account metadata")?;
    let wb = workbuddy::accounts::list().map_err(|_| "Cannot read WorkBuddy account metadata")?;
    if ag.len() > MAX_CANDIDATES || wb.len() > MAX_CANDIDATES {
        return Err("Model discovery account limit exceeded".into());
    }
    let mut discovery = Discovery {
        sources: vec![
            ModelSource {
                id: ANTIGRAVITY_SOURCE.into(),
                provider_type: "antigravity".into(),
                display_name: "Antigravity".into(),
                enabled: ag.iter().any(|a| !a.disabled && !a.proxy_disabled),
                account_count: ag.len(),
            },
            ModelSource {
                id: WORKBUDDY_SOURCE.into(),
                provider_type: "workbuddy".into(),
                display_name: "WorkBuddy".into(),
                enabled: settings.enabled,
                account_count: wb.len(),
            },
        ],
        settings,
        records: BTreeMap::new(),
        observations: HashMap::new(),
        all_accounts: HashMap::new(),
        eligible_accounts: HashMap::new(),
    };
    for a in ag {
        discovery
            .all_accounts
            .entry(ANTIGRAVITY_SOURCE.into())
            .or_default()
            .insert(a.id.clone());
        let eligible = !a.disabled && !a.proxy_disabled;
        if eligible {
            discovery
                .eligible_accounts
                .entry(ANTIGRAVITY_SOURCE.into())
                .or_default()
                .insert(a.id.clone());
        }
        if let Some(quota) = a.quota {
            if quota.models.len() > MAX_CANDIDATES {
                return Err("Model discovery metadata limit exceeded".into());
            }
            for model in quota.models {
                let id = insert_record(&mut discovery.records, ANTIGRAVITY_SOURCE, &model.name)?;
                if eligible {
                    discovery
                        .observations
                        .entry(id)
                        .or_default()
                        .accounts
                        .insert(
                            a.id.clone(),
                            Evidence {
                                declared: true,
                                declared_vision: model.supports_images,
                                declared_reasoning: model.supports_thinking,
                                ..Evidence::default()
                            },
                        );
                }
            }
        }
    }
    for summary in wb {
        // Public account views intentionally omit verified_models; retrieve the local stored
        // account, never fresh()/sync(), and copy only model identities and text evidence.
        let a = workbuddy::accounts::get(&summary.id)
            .map_err(|_| "Cannot read WorkBuddy model evidence")?;
        discovery
            .all_accounts
            .entry(WORKBUDDY_SOURCE.into())
            .or_default()
            .insert(a.id.clone());
        let eligible = a.enabled
            && discovery
                .settings
                .preferred_account_id
                .as_ref()
                .is_none_or(|id| id == &a.id);
        if eligible {
            discovery
                .eligible_accounts
                .entry(WORKBUDDY_SOURCE.into())
                .or_default()
                .insert(a.id.clone());
        }
        if a.models.len() > MAX_CANDIDATES
            || a.verified_models.len() > MAX_CANDIDATES
            || a.unavailable_models.len() > MAX_CANDIDATES
        {
            return Err("Model discovery metadata limit exceeded".into());
        }
        let natives: BTreeSet<_> = a
            .models
            .iter()
            .map(|m| m.id.as_str())
            .chain(a.verified_models.keys().map(String::as_str))
            .chain(a.unavailable_models.iter().map(String::as_str))
            .collect();
        for native in natives {
            let id = insert_record(&mut discovery.records, WORKBUDDY_SOURCE, native)?;
            if eligible {
                let verified_at = a.verified_models.get(native).copied().filter(|at| *at > 0);
                let unavailable = a.unavailable_models.contains(native);
                discovery
                    .observations
                    .entry(id)
                    .or_default()
                    .accounts
                    .insert(
                        a.id.clone(),
                        Evidence {
                            declared: a.models.iter().any(|m| m.id == native),
                            status: if unavailable {
                                Some("unavailable")
                            } else {
                                verified_at.map(|_| "verified")
                            },
                            checked_at: if unavailable {
                                // Old denials have no per-model timestamp. Do not reuse
                                // another model's latest probe time as this one's evidence.
                                0
                            } else {
                                verified_at.unwrap_or(0)
                            },
                            ..Evidence::default()
                        },
                    );
            }
        }
    }
    for native in discovery.settings.models.values() {
        insert_record(&mut discovery.records, WORKBUDDY_SOURCE, native)?;
    }
    Ok(discovery)
}
fn legacy_target(
    public_id: &str,
    mapping: &HashMap<String, String>,
    settings: &workbuddy::Settings,
) -> (String, String) {
    let normalized = normalized_public_id(public_id);
    if let Some(native) = settings
        .target_model(normalized, mapping)
        .or_else(|| settings.target_model(public_id, mapping))
    {
        (WORKBUDDY_SOURCE.into(), native.into())
    } else {
        let routing_key = if mapping.contains_key(public_id) {
            public_id
        } else {
            normalized
        };
        (
            ANTIGRAVITY_SOURCE.into(),
            crate::proxy::common::model_mapping::resolve_legacy_model_route(routing_key, mapping),
        )
    }
}
fn catalog_routes(
    legacy_ids: &[String],
    mapping: &HashMap<String, String>,
    discovery: &mut Discovery,
) -> Result<BTreeMap<String, (String, String)>, String> {
    if legacy_ids.len() > MAX_PUBLISHED_MODELS || mapping.len() > MAX_CANDIDATES {
        return Err("Legacy catalog or mapping limit exceeded".into());
    }
    // The old non-raw catalog also included wildcard mapping keys. They remain
    // migration routing rules, never publication identities or validation errors.
    let mut names: BTreeSet<String> = legacy_ids
        .iter()
        .filter(|id| !id.contains('*'))
        .cloned()
        .collect();
    // A wildcard is a routing rule, never a public model. Preserve only exact custom keys.
    names.extend(mapping.keys().filter(|key| !key.contains('*')).cloned());
    names.extend(discovery.settings.models.keys().cloned());
    let mut routes = BTreeMap::new();
    for original in names {
        let public = normalized_public_id(&original);
        if !valid_id(public, MAX_ID_BYTES) {
            return Err("Invalid legacy catalog public model ID".into());
        }
        let (source, native) = legacy_target(&original, mapping, &discovery.settings);
        insert_record(&mut discovery.records, &source, &native)?;
        // Prefix-equivalent catalog entries must not hide conflicting bindings.
        if let Some(previous) = routes.insert(public.to_string(), (source.clone(), native.clone()))
        {
            if previous != (source, native) {
                return Err("Conflicting legacy models/ prefix aliases".into());
            }
        }
    }
    if routes.len() > MAX_PUBLISHED_MODELS {
        return Err("Legacy publication limit exceeded".into());
    }
    Ok(routes)
}
/// Model IDs that the old dispatcher already admitted for user keys: successful
/// calls, exact price rules and per-key free grants. Wildcard price rules stay
/// billing patterns; the concrete IDs they cover come from the catalog/history.
fn historical_ids(data_dir: &Path) -> Result<Vec<String>, String> {
    let path = data_dir.join("user_tokens.db");
    if !path
        .try_exists()
        .map_err(|_| "Cannot inspect historical model database")?
    {
        return Ok(Vec::new());
    }
    // Never call user_token_db::init_db/get_connection: those create or migrate tables.
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "Cannot read existing historical model database")?;
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| "Cannot bound historical model database wait")?;
    let has_column = |table: &str, column: &str| -> Result<bool, String> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            [table, column],
            |row| row.get(0),
        )
        .map_err(|_| "Cannot inspect historical model database".into())
    };
    let limit = (MAX_PUBLISHED_MODELS + 1) as i64;
    let mut ids = BTreeSet::new();
    let mut collect = |sql: &str, error: &str| -> Result<(), String> {
        let mut stmt = conn.prepare(sql).map_err(|_| error.to_string())?;
        let rows = stmt
            .query_map([limit], |row| row.get::<_, String>(0))
            .map_err(|_| error.to_string())?;
        for row in rows {
            ids.insert(row.map_err(|_| error.to_string())?);
            if ids.len() > MAX_PUBLISHED_MODELS {
                return Err("Historical publication limit exceeded".into());
            }
        }
        Ok(())
    };
    if has_column("token_usage_logs", "model")? {
        collect(
            "SELECT DISTINCT model FROM token_usage_logs WHERE status >= 200 AND status < 300 AND model IS NOT NULL AND model <> '' ORDER BY model LIMIT ?1",
            "Cannot read historical successful model IDs",
        )?;
    }
    if has_column("model_prices", "pattern")? {
        collect(
            "SELECT pattern FROM model_prices WHERE instr(pattern, '*') = 0 ORDER BY pattern LIMIT ?1",
            "Cannot read priced model IDs",
        )?;
    }
    if has_column("user_tokens", "free_models")? {
        collect(
            "SELECT DISTINCT j.value FROM user_tokens AS t, json_each(CASE WHEN json_valid(t.free_models) THEN t.free_models ELSE '[]' END) AS j WHERE j.type = 'text' ORDER BY j.value LIMIT ?1",
            "Cannot read free model IDs",
        )?;
    }
    // Stored strings are untrusted: they cannot create wildcard or malformed publications.
    Ok(ids
        .into_iter()
        .filter(|id| valid_id(normalized_public_id(id), MAX_ID_BYTES))
        .collect())
}
fn merge_candidates(store: &mut StoredRegistry, discovery: &Discovery) -> Result<bool, String> {
    let mut records: BTreeMap<_, _> = store
        .candidates
        .iter()
        .map(|record| (record.id.clone(), record.clone()))
        .collect();
    let old_len = records.len();
    for (id, candidate) in &discovery.records {
        records
            .entry(id.clone())
            .or_insert_with(|| candidate.clone());
    }
    if records.len() > MAX_CANDIDATES {
        return Err("Model candidate limit exceeded".into());
    }
    let changed = records.len() != old_len;
    store.candidates = records.into_values().collect();
    Ok(changed)
}
fn initialize(
    path: &Path,
    legacy_ids: &[String],
    mapping: &HashMap<String, String>,
    discovery: &mut Discovery,
    cache: &mut Option<Cached>,
) -> Result<StoredRegistry, String> {
    let mut ids = legacy_ids.to_vec();
    ids.extend(historical_ids(
        path.parent().ok_or("Invalid registry path")?,
    )?);
    // Deduplicate before applying the finite migration collection limit.
    ids.sort();
    ids.dedup();
    let routes = catalog_routes(&ids, mapping, discovery)?;
    let store = StoredRegistry {
        schema_version: SCHEMA_VERSION,
        revision: 1,
        candidates: discovery.records.values().cloned().collect(),
        published_models: routes
            .into_iter()
            .map(|(public_id, (source_id, native_model))| PublishedModel {
                id: format!("published:{}", URL_SAFE_NO_PAD.encode(&public_id)),
                display_name: public_id.chars().take(128).collect(),
                public_id,
                source_id,
                native_model,
                enabled: true,
                legacy: true,
            })
            .collect(),
        ignored_candidate_ids: Vec::new(),
        verification_results: Vec::new(),
    };
    persist(path, &store, None, cache)?;
    Ok(store)
}
fn capabilities(
    source: &str,
    verified: bool,
    accounts: &BTreeMap<String, Evidence>,
) -> Vec<ModelCapability> {
    let mut result = Vec::new();
    let mut add = |key: &str, label: &str, status: &str, reason: &str| {
        result.push(ModelCapability {
            key: key.into(),
            label: label.into(),
            status: status.into(),
            reason: reason.into(),
        });
    };
    add(
        "text",
        "Basic text",
        if verified { "supported" } else { "unverified" },
        if verified {
            "Successful basic text evidence only; not an advanced capability probe."
        } else {
            "Catalog/quota declarations are not successful inference evidence."
        },
    );
    for (key, label, values) in [
        (
            "declared_reasoning",
            "Upstream reasoning declaration",
            accounts
                .values()
                .filter_map(|e| e.declared_reasoning)
                .collect::<Vec<_>>(),
        ),
        (
            "declared_vision",
            "Upstream vision declaration",
            accounts
                .values()
                .filter_map(|e| e.declared_vision)
                .collect::<Vec<_>>(),
        ),
    ] {
        let declared = !values.is_empty();
        add(
            key,
            label,
            if declared {
                "conditional"
            } else {
                "unverified"
            },
            if values.contains(&true) {
                "At least one eligible account's quota metadata declares support. This is an upstream declaration, not an actual capability probe."
            } else if declared {
                "Available quota metadata does not declare support. This is metadata, not an inference test or a guarantee for the account pool."
            } else {
                "No explicit native capability declaration is available; do not infer support from model names."
            },
        );
    }
    for (key, label) in [
        ("reasoning", "Native reasoning"),
        ("vision", "Native vision"),
        ("tools", "Native tools"),
    ] {
        add(key, label, "unverified", "Not safely probed; text success and catalog declarations do not verify this capability.");
    }
    for (key, label) in [
        ("thinking_budget", "Thinking budget filtering"),
        ("thinking_backfill", "Thinking block backfill"),
        ("image_cropping", "Image cropping"),
        ("agent_compression", "Agent context compression"),
        ("global_system_prompt", "Global system prompt"),
        ("multimodal_limits", "Multimodal request limits"),
    ] {
        if source == ANTIGRAVITY_SOURCE {
            add(
                key,
                label,
                "conditional",
                if key == "agent_compression" {
                    "Only applicable Claude Messages/Cowork compression paths; not universal Agent support and not a per-model verification."
                } else {
                    "Implemented in the shared Antigravity pipeline; requires applicable model support and configuration, not verified for every model."
                },
            );
        } else {
            add(key, label, "not_integrated", "Not wired into WorkBuddy; this does not establish absence of native upstream support. Gemini thought signatures must not be replayed here.");
        }
    }
    add(
        "aliases",
        "Public aliases",
        "supported",
        "Explicit source/native registry bindings are supported; billing retains the public ID.",
    );
    add("protocol_adaptation", "Protocol adaptation", "supported", "Gateway protocol adapters are integrated; individual native modalities and tool behavior remain unverified.");
    add(
        "gemini_protocol",
        "Native Gemini endpoint",
        if source == ANTIGRAVITY_SOURCE {
            "conditional"
        } else {
            "unsupported"
        },
        if source == ANTIGRAVITY_SOURCE {
            "Gateway Gemini adapter is integrated; the native model's supported request fields remain model-dependent."
        } else {
            "WorkBuddy has no native Gemini endpoint adapter in this gateway."
        },
    );
    add(
        "image_generation",
        "Image-generation endpoint",
        if source == ANTIGRAVITY_SOURCE {
            "unverified"
        } else {
            "unsupported"
        },
        if source == ANTIGRAVITY_SOURCE {
            "Separate image-generation verification is required; basic text does not prove image output."
        } else {
            "WorkBuddy image-generation endpoints are not integrated; this is not a statement about the upstream's native capabilities."
        },
    );
    result
}
fn candidate_view(
    record: &CandidateRecord,
    store: &StoredRegistry,
    discovery: &Discovery,
) -> CandidateModel {
    let mut accounts: BTreeMap<String, Evidence> = discovery
        .observations
        .get(&record.id)
        .map(|observation| {
            observation
                .accounts
                .iter()
                .map(|(id, evidence)| (id.clone(), evidence.clone()))
                .collect()
        })
        .unwrap_or_default();
    if record.source_id == WORKBUDDY_SOURCE {
        // WB dispatch requires membership in the current native model catalog.
        // Keep probe history on disk, but do not resurrect a withdrawn grant.
        accounts.retain(|_, evidence| evidence.declared);
    }
    let eligible = discovery.eligible_accounts.get(&record.source_id);
    let mut global: Option<&ModelVerificationResult> = None;
    for probe in store
        .verification_results
        .iter()
        .filter(|p| p.candidate_id == record.id)
    {
        if let Some(id) = &probe.account_id {
            if !eligible.is_some_and(|ids| ids.contains(id))
                || (record.source_id == WORKBUDDY_SOURCE && !accounts.contains_key(id))
            {
                continue;
            }
            let evidence = accounts.entry(id.clone()).or_default();
            if probe.checked_at >= evidence.checked_at {
                evidence.status = match probe.status.as_str() {
                    "verified" => Some("verified"),
                    "unavailable" => Some("unavailable"),
                    _ => Some("failed"),
                };
                evidence.checked_at = probe.checked_at;
            }
        } else {
            global = Some(probe);
        }
    }
    let verified_account_count = accounts
        .values()
        .filter(|e| e.status == Some("verified"))
        .count();
    let account_count = accounts
        .values()
        .filter(|e| (e.declared && e.status != Some("unavailable")) || e.status == Some("verified"))
        .count();
    let latest_account = accounts
        .values()
        .filter(|e| e.checked_at > 0)
        .map(|e| e.checked_at)
        .max();
    let last_verified_at = latest_account
        .into_iter()
        .chain(global.map(|p| p.checked_at))
        .max();
    let all_unavailable = if record.source_id == WORKBUDDY_SOURCE {
        // WB dispatch requires membership in the account's stored model catalog. Accounts
        // that only declare unrelated models cannot make a denied target merely unverified.
        !accounts.is_empty() && accounts.values().all(|e| e.status == Some("unavailable"))
    } else {
        eligible.is_some_and(|ids| {
            !ids.is_empty()
                && ids.iter().all(|id| {
                    accounts
                        .get(id)
                        .is_some_and(|e| e.status == Some("unavailable"))
                })
        })
    };
    let status = if verified_account_count > 0 {
        "verified"
    } else if all_unavailable {
        "unavailable"
    } else if global.is_some_and(|probe| probe.status == "failed")
        || accounts.values().any(|e| e.status == Some("failed"))
    {
        "failed"
    } else {
        "unverified"
    };
    CandidateModel {
        id: record.id.clone(),
        source_id: record.source_id.clone(),
        provider_type: provider(&record.source_id).unwrap_or("unknown").into(),
        native_model: record.native_model.clone(),
        // Never reuse account nicknames/emails or arbitrary upstream error strings.
        display_name: record.native_model.clone(),
        account_count,
        verified_account_count,
        verification_status: status.into(),
        last_verified_at,
        ignored: store.ignored_candidate_ids.contains(&record.id),
        capabilities: capabilities(&record.source_id, status == "verified", &accounts),
    }
}
fn make_view(store: &StoredRegistry, discovery: &Discovery) -> ModelRegistryView {
    ModelRegistryView {
        schema_version: store.schema_version,
        revision: store.revision,
        sources: discovery.sources.clone(),
        candidates: store.candidates.iter().map(|record| candidate_view(record, store, discovery)).collect(),
        published_models: store.published_models.clone(),
        notices: vec![
            "Discovery never publishes new models after the one-time legacy migration.".into(),
            "Verification proves basic text only; reasoning, vision and tools are not verified.".into(),
            "Legacy publications retain their original billing IDs without fabricated verification evidence.".into(),
        ],
    }
}

/// Initialize once from the real legacy catalog. Subsequent calls only add candidate identities;
/// publication, disablement, ignored IDs and explicit source bindings remain administrator-owned.
pub fn view(
    legacy_ids: &[String],
    mapping: &HashMap<String, String>,
) -> Result<ModelRegistryView, String> {
    let path = registry_path()?;
    let mut cache = CACHE.lock().map_err(|_| "Model registry unavailable")?;
    let previous = load(&path, &mut cache)?;
    let mut discovery = discover()?;
    let mut store = if let Some(previous) = previous.as_ref() {
        catalog_routes(legacy_ids, mapping, &mut discovery)?;
        previous.store.clone()
    } else {
        let initialized = initialize(&path, legacy_ids, mapping, &mut discovery, &mut cache)?;
        return Ok(make_view(&initialized, &discovery));
    };
    if merge_candidates(&mut store, &discovery)? {
        next_revision(&mut store)?;
        persist(&path, &store, previous.as_deref(), &mut cache)?;
    }
    Ok(make_view(&store, &discovery))
}

pub fn save(
    request: SaveModelRegistryRequest,
    legacy_ids: &[String],
    mapping: &HashMap<String, String>,
) -> Result<ModelRegistryView, String> {
    if request.published_models.len() > MAX_PUBLISHED_MODELS
        || request.ignored_candidate_ids.len() > MAX_CANDIDATES
    {
        return Err("Model registry collection limit exceeded".into());
    }
    let path = registry_path()?;
    let mut cache = CACHE.lock().map_err(|_| "Model registry unavailable")?;
    let mut previous = load(&path, &mut cache)?;
    let mut discovery = discover()?;
    if previous.is_none() {
        initialize(&path, legacy_ids, mapping, &mut discovery, &mut cache)?;
        previous = load(&path, &mut cache)?;
    }
    let previous = previous.ok_or("Model registry initialization failed")?;
    if request.revision != previous.store.revision {
        return Err(format!(
            "Model registry revision conflict: expected {}, current {}; reload and retry",
            request.revision, previous.store.revision
        ));
    }
    catalog_routes(legacy_ids, mapping, &mut discovery)?;
    let mut store = previous.store.clone();
    merge_candidates(&mut store, &discovery)?;
    let candidate_views: HashMap<_, _> = store
        .candidates
        .iter()
        .map(|record| {
            (
                record.id.clone(),
                candidate_view(record, &store, &discovery),
            )
        })
        .collect();
    let existing: HashMap<_, _> = previous
        .store
        .published_models
        .iter()
        .map(|model| (model.id.as_str(), model))
        .collect();
    let mut publications = request.published_models;
    for model in &mut publications {
        model.public_id = normalized_public_id(&model.public_id).to_string();
        validate_native(&model.source_id, &model.native_model)?;
        let candidate = candidate_views
            .get(&candidate_id(&model.source_id, &model.native_model))
            .ok_or("Unknown source/native candidate; discover it before publishing")?;
        let old = existing.get(model.id.as_str()).copied();
        let same_binding = old.is_some_and(|old| {
            old.public_id == model.public_id
                && old.source_id == model.source_id
                && old.native_model == model.native_model
        });
        let grandfathered = same_binding && old.is_some_and(|old| old.legacy);
        if model.legacy && !grandfathered {
            return Err(
                "Legacy publication status cannot be created or transferred to a new binding"
                    .into(),
            );
        }
        let newly_exposed = !same_binding || old.is_some_and(|old| !old.enabled && model.enabled);
        if newly_exposed && !grandfathered {
            if candidate.verification_status == "unavailable" {
                return Err("Definitively unavailable candidates cannot be newly published".into());
            }
            if candidate.verification_status != "verified" && !request.allow_unverified {
                return Err("Publishing an unverified model requires explicit allow_unverified confirmation".into());
            }
        }
    }
    store.published_models = publications;
    store.ignored_candidate_ids = request.ignored_candidate_ids;
    validate_store(&store)?;
    store
        .published_models
        .sort_by(|a, b| a.public_id.cmp(&b.public_id));
    store.ignored_candidate_ids.sort();
    next_revision(&mut store)?;
    persist(&path, &store, Some(&previous), &mut cache)?;
    Ok(make_view(&store, &discovery))
}

/// Lookup only. Missing/corrupt data is an error, never an empty auto-initialized allowlist.
/// Disabled publications are deliberately indistinguishable from unpublished IDs at admission.
pub fn resolve_existing(public_id: &str) -> Result<Option<PublishedModel>, String> {
    let public_id = normalized_public_id(public_id);
    if !valid_id(public_id, MAX_ID_BYTES) {
        return Err("Invalid public model ID".into());
    }
    let path = registry_path()?;
    let mut cache = CACHE.lock().map_err(|_| "Model registry unavailable")?;
    let snapshot = load(&path, &mut cache)?
        .ok_or("Model registry is not initialized; load the legacy catalog first")?;
    Ok(snapshot
        .published_index
        .get(public_id)
        .map(|index| &snapshot.store.published_models[*index])
        .filter(|model| model.enabled)
        .cloned())
}

pub fn published_ids() -> Result<Vec<String>, String> {
    let path = registry_path()?;
    let mut cache = CACHE.lock().map_err(|_| "Model registry unavailable")?;
    let snapshot = load(&path, &mut cache)?
        .ok_or("Model registry is not initialized; load the legacy catalog first")?;
    let mut ids: Vec<_> = snapshot
        .store
        .published_models
        .iter()
        .filter(|model| model.enabled)
        .map(|model| model.public_id.clone())
        .collect();
    ids.sort();
    Ok(ids)
}
pub fn record_verification(result: ModelVerificationResult) -> Result<(), String> {
    let result = sanitize_verification_result(result)?;
    let path = registry_path()?;
    let mut cache = CACHE.lock().map_err(|_| "Model registry unavailable")?;
    let previous = load(&path, &mut cache)?
        .ok_or("Model registry is not initialized; discover candidates first")?;
    let candidate = previous
        .store
        .candidates
        .iter()
        .find(|record| record.id == result.candidate_id)
        .ok_or("Unknown verification candidate ID")?;
    if let Some(id) = &result.account_id {
        let discovery = discover()?;
        if !discovery
            .all_accounts
            .get(&candidate.source_id)
            .is_some_and(|ids| ids.contains(id))
        {
            return Err("Verification account does not belong to the candidate source".into());
        }
    }
    let mut store = previous.store.clone();
    if let Some(old) = store.verification_results.iter_mut().find(|probe| {
        probe.candidate_id == result.candidate_id && probe.account_id == result.account_id
    }) {
        if result.checked_at < old.checked_at {
            return Err("Stale model verification result".into());
        }
        *old = result;
    } else {
        if store.verification_results.len() >= MAX_VERIFICATION_RESULTS {
            return Err("Model verification evidence limit exceeded".into());
        }
        store.verification_results.push(result);
    }
    next_revision(&mut store)?;
    persist(&path, &store, Some(&previous), &mut cache)
}

/// Use this for the verification report too: never return the raw upstream probe message.
pub fn sanitize_verification_result(
    mut result: ModelVerificationResult,
) -> Result<ModelVerificationResult, String> {
    result.message = verification_message(&result.status)?.into();
    validate_result(&result)?;
    Ok(result)
}

pub fn validate_refresh_request(request: &RefreshModelCandidatesRequest) -> Result<(), String> {
    if let Some(source) = request.source_id.as_deref() {
        provider(source)?;
    }
    Ok(())
}

/// The runtime owner must enforce the timeout and one upstream attempt per selected candidate.
/// This helper validates consent/selection without ever initiating an upstream request.
pub fn validate_verification_request(
    request: &VerifyModelCandidatesRequest,
    registry: &ModelRegistryView,
) -> Result<(), String> {
    if !request.confirm_upstream_usage {
        return Err("Explicit upstream quota/cost confirmation is required".into());
    }
    if request.candidate_ids.is_empty() || request.candidate_ids.len() > MAX_VERIFY_CANDIDATES {
        return Err("Select between 1 and 5 candidates for basic text verification".into());
    }
    let mut selected = HashSet::new();
    for id in &request.candidate_ids {
        if !selected.insert(id)
            || !registry
                .candidates
                .iter()
                .any(|candidate| &candidate.id == id)
        {
            return Err("Unknown or duplicate verification candidate ID".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
