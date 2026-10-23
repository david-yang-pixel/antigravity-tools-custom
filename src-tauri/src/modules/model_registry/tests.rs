use super::*;
use crate::proxy::monitor::prompt_log_tests::TestDataDir;
use serde_json::json;

// Share the application's environment lock, and isolate provider overrides as well as data.
struct Sandbox {
    _dir: TestDataDir,
    enabled: Option<std::ffi::OsString>,
    models: Option<std::ffi::OsString>,
}
impl Sandbox {
    fn new() -> Self {
        let dir = TestDataDir::new();
        let enabled = std::env::var_os("ABV_WORKBUDDY_ENABLED");
        let models = std::env::var_os("ABV_WORKBUDDY_MODELS");
        std::env::remove_var("ABV_WORKBUDDY_ENABLED");
        std::env::remove_var("ABV_WORKBUDDY_MODELS");
        Self {
            _dir: dir,
            enabled,
            models,
        }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        for (key, value) in [
            ("ABV_WORKBUDDY_ENABLED", &self.enabled),
            ("ABV_WORKBUDDY_MODELS", &self.models),
        ] {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}
fn request(registry: &ModelRegistryView) -> SaveModelRegistryRequest {
    SaveModelRegistryRequest {
        revision: registry.revision,
        published_models: registry.published_models.clone(),
        ignored_candidate_ids: registry
            .candidates
            .iter()
            .filter(|c| c.ignored)
            .map(|c| c.id.clone())
            .collect(),
        allow_unverified: false,
    }
}
fn publication(public: &str, source: &str, native: &str) -> PublishedModel {
    PublishedModel {
        id: format!("new:{}", URL_SAFE_NO_PAD.encode(public)),
        public_id: public.into(),
        display_name: public.into(),
        source_id: source.into(),
        native_model: native.into(),
        enabled: true,
        legacy: false,
    }
}
fn settings(models: &[(&str, &str)]) {
    let settings = workbuddy::Settings {
        enabled: true,
        models: models
            .iter()
            .map(|(alias, native)| (alias.to_string(), native.to_string()))
            .collect(),
        preferred_account_id: None,
    };
    write_atomic(
        account::get_data_dir().unwrap().join("workbuddy.json"),
        &serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
}
fn seed_wb(uid: &str, natives: &[&str], verified: &[&str], unavailable: &[&str]) -> String {
    let mut account = workbuddy::accounts::parse_import(json!({
        "uid": uid, "nickname": "private-nickname", "accessToken": "private-access-secret",
        "refreshToken": "private-refresh-secret", "expiresAt": workbuddy::accounts::now() + 7200,
    }))
    .unwrap();
    account.models = natives
        .iter()
        .map(|native| workbuddy::accounts::Model {
            id: (*native).into(),
            name: "Do not expose account metadata".into(),
        })
        .collect();
    account.verified_models = verified
        .iter()
        .map(|native| ((*native).into(), 123))
        .collect();
    account.unavailable_models = unavailable.iter().map(|native| (*native).into()).collect();
    account.models_verified_at = if verified.is_empty() && unavailable.is_empty() {
        0
    } else {
        123
    };
    workbuddy::accounts::upsert(account).unwrap().id
}
fn seed_ag(natives: &[&str]) -> String {
    let token = crate::models::TokenData::new(
        "private-ag-access".into(),
        "private-ag-refresh".into(),
        7200,
        Some("private@example.test".into()),
        None,
        None,
        false,
        None,
    );
    let mut a = account::add_account(
        "private@example.test".into(),
        Some("private-name".into()),
        token,
    )
    .unwrap();
    let mut quota = crate::models::QuotaData::new();
    quota.models = natives
        .iter()
        .map(|native| {
            serde_json::from_value(json!({
                "name": native, "percentage": 100, "reset_time": "", "supports_images": true,
                "supports_thinking": true,
            }))
            .unwrap()
        })
        .collect();
    a.quota = Some(quota);
    account::save_account(&a).unwrap();
    a.id
}
fn find<'a>(registry: &'a ModelRegistryView, source: &str, native: &str) -> &'a CandidateModel {
    registry
        .candidates
        .iter()
        .find(|c| c.id == candidate_id(source, native))
        .unwrap()
}
fn stored() -> StoredRegistry {
    serde_json::from_slice(&std::fs::read(registry_path().unwrap()).unwrap()).unwrap()
}
fn record(
    native: &str,
    source: &str,
    status: &str,
    at: i64,
    account_id: Option<String>,
) -> ModelVerificationResult {
    ModelVerificationResult {
        candidate_id: candidate_id(source, native),
        status: status.into(),
        message: "private@example.test bearer private-secret prompt: private-prompt".into(),
        checked_at: at,
        account_id,
    }
}

#[test]
fn resolver_does_not_initialize_and_matches_only_exact_public_ids() {
    let _dir = Sandbox::new();
    assert!(resolve_existing("ExactCase").is_err());
    assert!(published_ids().is_err());
    assert!(!registry_path().unwrap().exists());
    let registry = view(&ids(&["ExactCase"]), &HashMap::new()).unwrap();
    assert_eq!(registry.revision, 1);
    assert!(resolve_existing("models/ExactCase").unwrap().is_some());
    assert!(resolve_existing("exactcase").unwrap().is_none());
    assert!(resolve_existing("ExactCase-extra").unwrap().is_none());
    assert!(resolve_existing("Exact*").is_err());
    assert!(!account::get_data_dir()
        .unwrap()
        .join("user_tokens.db")
        .exists());
}

#[test]
fn one_time_migration_uses_exact_keys_readonly_history_and_wb_precedence() {
    let _dir = Sandbox::new();
    settings(&[("Shared", "WB/NativeCase"), ("wb-alias", "WB/Other")]);
    let db_path = account::get_data_dir().unwrap().join("user_tokens.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE token_usage_logs(model TEXT, status INTEGER);
        INSERT INTO token_usage_logs VALUES ('HistoricalCase', 200), ('no-success', 500),
        ('models/HistoricalCase', 204), ('bad*request', 200);",
    )
    .unwrap();
    drop(conn);
    let before = std::fs::read(&db_path).unwrap();
    let mapping = HashMap::from([
        ("ExactAlias".into(), "NativeCase".into()),
        ("IndirectWB".into(), "wb-alias".into()),
        ("future-*".into(), "NativeFuture".into()),
        ("Shared".into(), "AG-must-not-win".into()),
    ]);
    let first = view(&ids(&["Shared", "CatalogOnly"]), &mapping).unwrap();
    assert!(first
        .published_models
        .iter()
        .all(|model| model.enabled && model.legacy));
    assert!(published_ids().unwrap().contains(&"HistoricalCase".into()));
    assert!(!published_ids().unwrap().contains(&"no-success".into()));
    assert!(!published_ids().unwrap().iter().any(|id| id.contains('*')));
    let shared = resolve_existing("Shared").unwrap().unwrap();
    assert_eq!(shared.source_id, WORKBUDDY_SOURCE);
    assert_eq!(shared.native_model, "WB/NativeCase");
    assert_eq!(
        resolve_existing("IndirectWB")
            .unwrap()
            .unwrap()
            .native_model,
        "WB/Other"
    );
    assert_eq!(
        resolve_existing("ExactAlias")
            .unwrap()
            .unwrap()
            .native_model,
        "NativeCase"
    );
    assert_eq!(std::fs::read(&db_path).unwrap(), before);
    let second = view(&ids(&["Shared", "CatalogOnly"]), &mapping).unwrap();
    assert_eq!(first.revision, second.revision);
    assert_eq!(first.published_models, second.published_models);
    let changed = view(&ids(&["Shared", "NewlyDiscovered"]), &HashMap::new()).unwrap();
    assert_eq!(changed.published_models, first.published_models);
    assert!(
        find(&changed, ANTIGRAVITY_SOURCE, "NewlyDiscovered").verification_status == "unverified"
    );
    assert!(resolve_existing("NewlyDiscovered").unwrap().is_none());
}

#[test]
fn same_native_names_have_separate_candidates_and_never_steal_bindings() {
    let _dir = Sandbox::new();
    seed_ag(&["SameNative"]);
    seed_wb("collision", &["SameNative"], &[], &[]);
    let first = view(&ids(&["SameNative"]), &HashMap::new()).unwrap();
    assert_ne!(
        candidate_id(ANTIGRAVITY_SOURCE, "SameNative"),
        candidate_id(WORKBUDDY_SOURCE, "SameNative")
    );
    assert_ne!(candidate_id("a:b", "c"), candidate_id("a", "b:c"));
    assert_ne!(
        candidate_id(ANTIGRAVITY_SOURCE, "Case"),
        candidate_id(ANTIGRAVITY_SOURCE, "case")
    );
    assert_eq!(
        find(&first, ANTIGRAVITY_SOURCE, "SameNative").account_count,
        1
    );
    assert_eq!(
        find(&first, WORKBUDDY_SOURCE, "SameNative").account_count,
        1
    );
    assert_eq!(
        resolve_existing("SameNative").unwrap().unwrap().source_id,
        ANTIGRAVITY_SOURCE
    );
    settings(&[("SameNative", "SameNative")]);
    view(&ids(&["SameNative"]), &HashMap::new()).unwrap();
    assert_eq!(
        resolve_existing("SameNative").unwrap().unwrap().source_id,
        ANTIGRAVITY_SOURCE
    );
}

#[test]
fn unverified_confirmation_ignore_disablement_and_unpublication_persist() {
    let _dir = Sandbox::new();
    view(&[], &HashMap::new()).unwrap();
    let registry = view(&ids(&["CandidateCase"]), &HashMap::new()).unwrap();
    let mut draft = request(&registry);
    draft.published_models.push(publication(
        "PublicCase",
        ANTIGRAVITY_SOURCE,
        "CandidateCase",
    ));
    draft
        .ignored_candidate_ids
        .push(candidate_id(ANTIGRAVITY_SOURCE, "CandidateCase"));
    assert!(save(draft.clone(), &[], &HashMap::new())
        .unwrap_err()
        .contains("allow_unverified"));
    assert_eq!(
        view(&[], &HashMap::new()).unwrap().revision,
        registry.revision
    );
    draft.allow_unverified = true;
    let published = save(draft, &[], &HashMap::new()).unwrap();
    assert!(find(&published, ANTIGRAVITY_SOURCE, "CandidateCase").ignored);
    assert_eq!(
        resolve_existing("models/PublicCase")
            .unwrap()
            .unwrap()
            .native_model,
        "CandidateCase"
    );
    *CACHE.lock().unwrap() = None;
    let loaded = view(&[], &HashMap::new()).unwrap();
    assert_eq!(loaded, published);
    let mut disable = request(&loaded);
    disable.published_models[0].enabled = false;
    let disabled = save(disable, &[], &HashMap::new()).unwrap();
    assert!(!disabled.published_models[0].enabled);
    assert!(resolve_existing("PublicCase").unwrap().is_none());
    assert!(published_ids().unwrap().is_empty());
    let mut reenable = request(&disabled);
    reenable.published_models[0].enabled = true;
    assert!(save(reenable, &[], &HashMap::new())
        .unwrap_err()
        .contains("allow_unverified"));
    let mut remove = request(&disabled);
    remove.published_models.clear();
    let removed = save(remove, &[], &HashMap::new()).unwrap();
    assert!(removed.published_models.is_empty());
    assert!(
        find(
            &view(&ids(&["CandidateCase"]), &HashMap::new()).unwrap(),
            ANTIGRAVITY_SOURCE,
            "CandidateCase"
        )
        .ignored
    );
    assert!(resolve_existing("PublicCase").unwrap().is_none());
}

#[test]
fn revision_conflict_preserves_the_successful_writer() {
    let _dir = Sandbox::new();
    let registry = view(&ids(&["Legacy"]), &HashMap::new()).unwrap();
    let stale = request(&registry);
    let mut first = stale.clone();
    first.published_models[0].enabled = false;
    let saved = save(first, &[], &HashMap::new()).unwrap();
    assert!(save(stale, &[], &HashMap::new())
        .unwrap_err()
        .contains("revision conflict"));
    assert_eq!(view(&[], &HashMap::new()).unwrap(), saved);
}

#[test]
fn invalid_mutations_are_atomic_and_legacy_status_cannot_be_forged() {
    let _dir = Sandbox::new();
    let registry = view(&ids(&["Legacy"]), &HashMap::new()).unwrap();
    let before = std::fs::read(registry_path().unwrap()).unwrap();
    let mut invalid = Vec::new();
    for id in [
        "",
        "bad space",
        "line\nbreak",
        "wild*",
        "wild?",
        "bracket[x]",
    ] {
        let mut draft = request(&registry);
        draft.published_models[0].public_id = id.into();
        draft.published_models[0].legacy = false;
        invalid.push(draft);
    }
    let mut collision = request(&registry);
    let mut other = collision.published_models[0].clone();
    other.id = "different-stable-id".into();
    other.public_id = "models/Legacy".into();
    other.legacy = false;
    collision.published_models.push(other);
    invalid.push(collision);
    let mut wrong_source = request(&registry);
    wrong_source.published_models[0].source_id = "unknown:source".into();
    invalid.push(wrong_source);
    let mut unknown_native = request(&registry);
    unknown_native.published_models[0].native_model = "UnknownTarget".into();
    unknown_native.published_models[0].legacy = false;
    invalid.push(unknown_native);
    let mut spoof = request(&registry);
    spoof.published_models.push(PublishedModel {
        legacy: true,
        ..publication("Forged", ANTIGRAVITY_SOURCE, "Legacy")
    });
    invalid.push(spoof);
    let mut wrong_ignore = request(&registry);
    wrong_ignore
        .ignored_candidate_ids
        .push("nonexistent".into());
    invalid.push(wrong_ignore);
    let mut duplicate_ignore = request(&registry);
    duplicate_ignore.ignored_candidate_ids = vec![candidate_id(ANTIGRAVITY_SOURCE, "Legacy"); 2];
    invalid.push(duplicate_ignore);
    let mut too_many = request(&registry);
    too_many.published_models =
        vec![registry.published_models[0].clone(); MAX_PUBLISHED_MODELS + 1];
    invalid.push(too_many);
    let mut too_many_ignored = request(&registry);
    too_many_ignored.ignored_candidate_ids = vec!["anything".into(); MAX_CANDIDATES + 1];
    invalid.push(too_many_ignored);
    for mut draft in invalid {
        draft.allow_unverified = true;
        assert!(save(draft, &[], &HashMap::new()).is_err());
        assert_eq!(std::fs::read(registry_path().unwrap()).unwrap(), before);
    }
}

#[test]
fn basic_text_evidence_does_not_verify_advanced_capabilities_or_publish() {
    let _dir = Sandbox::new();
    seed_ag(&["AGDeclared"]);
    seed_wb(
        "text-evidence",
        &["WBVerified", "WBDeclared"],
        &["WBVerified"],
        &[],
    );
    let registry = view(&[], &HashMap::new()).unwrap();
    assert!(registry.published_models.is_empty());
    let ag = find(&registry, ANTIGRAVITY_SOURCE, "AGDeclared");
    assert_eq!(ag.verification_status, "unverified");
    assert_eq!(ag.verified_account_count, 0);
    let wb = find(&registry, WORKBUDDY_SOURCE, "WBVerified");
    assert_eq!(wb.verification_status, "verified");
    assert_eq!(wb.verified_account_count, 1);
    assert_eq!(wb.last_verified_at, Some(123));
    assert_eq!(
        find(&registry, WORKBUDDY_SOURCE, "WBDeclared").verification_status,
        "unverified"
    );
    for candidate in [ag, wb] {
        for key in ["reasoning", "vision", "tools"] {
            assert_eq!(
                candidate
                    .capabilities
                    .iter()
                    .find(|c| c.key == key)
                    .unwrap()
                    .status,
                "unverified"
            );
        }
        for key in [
            "thinking_budget",
            "thinking_backfill",
            "image_cropping",
            "agent_compression",
        ] {
            let expected = if candidate.source_id == ANTIGRAVITY_SOURCE {
                "conditional"
            } else {
                "not_integrated"
            };
            assert_eq!(
                candidate
                    .capabilities
                    .iter()
                    .find(|c| c.key == key)
                    .unwrap()
                    .status,
                expected
            );
        }
        for key in ["aliases", "protocol_adaptation"] {
            assert_eq!(
                candidate
                    .capabilities
                    .iter()
                    .find(|c| c.key == key)
                    .unwrap()
                    .status,
                "supported"
            );
        }
    }
    let mut draft = request(&registry);
    draft
        .published_models
        .push(publication("VerifiedAlias", WORKBUDDY_SOURCE, "WBVerified"));
    assert!(save(draft, &[], &HashMap::new()).is_ok());
    let serialized = format!(
        "{}{}",
        serde_json::to_string(&registry).unwrap(),
        std::fs::read_to_string(registry_path().unwrap()).unwrap()
    );
    for secret in [
        "private@example.test",
        "private-nickname",
        "private-ag-access",
        "private-ag-refresh",
        "private-access-secret",
        "private-refresh-secret",
        "private-name",
    ] {
        assert!(!serialized.contains(secret), "leaked {secret}");
    }
}

#[test]
fn unavailable_candidates_are_not_publishable_even_with_confirmation() {
    let _dir = Sandbox::new();
    view(&[], &HashMap::new()).unwrap();
    seed_wb("unavailable", &["Denied"], &[], &["Denied"]);
    let registry = view(&[], &HashMap::new()).unwrap();
    assert_eq!(
        find(&registry, WORKBUDDY_SOURCE, "Denied").verification_status,
        "unavailable"
    );
    let mut draft = request(&registry);
    draft.allow_unverified = true;
    draft
        .published_models
        .push(publication("DeniedAlias", WORKBUDDY_SOURCE, "Denied"));
    assert!(save(draft, &[], &HashMap::new())
        .unwrap_err()
        .contains("unavailable"));
}

#[test]
fn a_published_binding_can_take_another_alias_but_not_a_used_name() {
    let _dir = Sandbox::new();
    view(&[], &HashMap::new()).unwrap();
    seed_wb("aliased", &["Flash"], &["Flash"], &[]);
    let registry = view(&[], &HashMap::new()).unwrap();
    let mut draft = request(&registry);
    draft
        .published_models
        .push(publication("Flash", WORKBUDDY_SOURCE, "Flash"));
    let registry = save(draft, &[], &HashMap::new()).unwrap();

    let mut duplicate = request(&registry);
    let mut reused = publication("Flash", WORKBUDDY_SOURCE, "Flash");
    reused.id = "new:second".into();
    duplicate.published_models.push(reused);
    assert!(save(duplicate, &[], &HashMap::new()).is_err());

    let mut alias = request(&registry);
    alias
        .published_models
        .push(publication("my-flash", WORKBUDDY_SOURCE, "Flash"));
    save(alias, &[], &HashMap::new()).unwrap();
    for public in ["Flash", "my-flash"] {
        let resolved = resolve_existing(public).unwrap().unwrap();
        assert_eq!(
            (resolved.source_id.as_str(), resolved.native_model.as_str()),
            (WORKBUDDY_SOURCE, "Flash")
        );
    }
}

#[test]
fn one_account_denial_does_not_prove_other_candidate_accounts_unavailable() {
    let _dir = Sandbox::new();
    seed_wb("denied", &["Mixed"], &[], &["Mixed"]);
    seed_wb("not-probed", &["Mixed"], &[], &[]);
    let registry = view(&[], &HashMap::new()).unwrap();
    let candidate = find(&registry, WORKBUDDY_SOURCE, "Mixed");
    assert_eq!(candidate.verification_status, "unverified");
    assert_eq!(candidate.account_count, 1);
    assert_eq!(candidate.verified_account_count, 0);
}

#[test]
fn verification_is_sanitized_persisted_source_checked_and_never_initializes() {
    let _dir = Sandbox::new();
    assert!(
        record_verification(record("Probe", ANTIGRAVITY_SOURCE, "verified", 100, None)).is_err()
    );
    assert!(!registry_path().unwrap().exists());
    let ag_account = seed_ag(&["Probe"]);
    let wb_account = seed_wb("other-source", &["Probe"], &[], &[]);
    let registry = view(&[], &HashMap::new()).unwrap();
    assert!(record_verification(record(
        "Probe",
        ANTIGRAVITY_SOURCE,
        "verified",
        100,
        Some(wb_account)
    ))
    .is_err());
    record_verification(record(
        "Probe",
        ANTIGRAVITY_SOURCE,
        "verified",
        100,
        Some(ag_account.clone()),
    ))
    .unwrap();
    let updated = view(&[], &HashMap::new()).unwrap();
    assert_eq!(updated.revision, registry.revision + 1);
    assert_eq!(
        find(&updated, ANTIGRAVITY_SOURCE, "Probe").verification_status,
        "verified"
    );
    assert_eq!(
        find(&updated, ANTIGRAVITY_SOURCE, "Probe").verified_account_count,
        1
    );
    assert!(updated.published_models.is_empty());
    record_verification(record("Probe", ANTIGRAVITY_SOURCE, "failed", 101, None)).unwrap();
    let with_failed_sample = view(&[], &HashMap::new()).unwrap();
    assert_eq!(
        find(&with_failed_sample, ANTIGRAVITY_SOURCE, "Probe").verification_status,
        "verified"
    );
    assert_eq!(
        find(&with_failed_sample, ANTIGRAVITY_SOURCE, "Probe").verified_account_count,
        1
    );
    let disk = stored();
    assert_eq!(
        disk.verification_results[0].message,
        verification_message("verified").unwrap()
    );
    assert!(!std::fs::read_to_string(registry_path().unwrap())
        .unwrap()
        .contains("private-secret"));
    assert!(record_verification(record(
        "Probe",
        ANTIGRAVITY_SOURCE,
        "failed",
        99,
        Some(ag_account)
    ))
    .unwrap_err()
    .contains("Stale"));
    assert!(record_verification(record(
        "Probe",
        ANTIGRAVITY_SOURCE,
        "not-a-status",
        101,
        None
    ))
    .is_err());
    assert!(
        record_verification(record("Missing", ANTIGRAVITY_SOURCE, "verified", 101, None)).is_err()
    );
}

#[test]
fn verification_consent_selection_and_limits_are_enforced_without_io() {
    let registry = ModelRegistryView {
        schema_version: 1,
        revision: 1,
        sources: Vec::new(),
        published_models: Vec::new(),
        notices: Vec::new(),
        candidates: (0..6)
            .map(|n| CandidateModel {
                id: n.to_string(),
                source_id: ANTIGRAVITY_SOURCE.into(),
                provider_type: "antigravity".into(),
                native_model: n.to_string(),
                display_name: n.to_string(),
                account_count: 0,
                verified_account_count: 0,
                verification_status: "unverified".into(),
                last_verified_at: None,
                ignored: false,
                capabilities: Vec::new(),
            })
            .collect(),
    };
    let mut request = VerifyModelCandidatesRequest {
        candidate_ids: ids(&["0"]),
        confirm_upstream_usage: false,
        account_id: None,
    };
    assert!(validate_verification_request(&request, &registry).is_err());
    request.confirm_upstream_usage = true;
    assert!(validate_verification_request(&request, &registry).is_ok());
    for selection in [
        vec![],
        ids(&["missing"]),
        ids(&["0", "0"]),
        ids(&["0", "1", "2", "3", "4", "5"]),
    ] {
        request.candidate_ids = selection;
        assert!(validate_verification_request(&request, &registry).is_err());
    }
    request.candidate_ids = ids(&["0", "1", "2", "3", "4"]);
    assert!(validate_verification_request(&request, &registry).is_ok());
    request.account_id = Some("account-1".into());
    assert!(validate_verification_request(&request, &registry).is_ok());
    request.account_id = Some(String::new());
    assert!(validate_verification_request(&request, &registry).is_err());
    assert_eq!(VERIFY_TIMEOUT_SECONDS, 30);
}

#[test]
fn cached_lookup_observes_external_replacement_deletion_and_corruption() {
    let _dir = Sandbox::new();
    view(&ids(&["External"]), &HashMap::new()).unwrap();
    assert!(resolve_existing("External").unwrap().is_some());
    let mut external = stored();
    external.revision += 1;
    external.published_models[0].enabled = false;
    write_atomic(
        registry_path().unwrap(),
        &serde_json::to_vec(&external).unwrap(),
    )
    .unwrap();
    assert!(resolve_existing("External").unwrap().is_none());
    assert!(published_ids().unwrap().is_empty());
    write_atomic(registry_path().unwrap(), b"{ broken secret").unwrap();
    assert!(resolve_existing("External").is_err());
    assert!(published_ids().is_err());
    assert!(view(&ids(&["External"]), &HashMap::new()).is_err());
    assert_eq!(
        std::fs::read(registry_path().unwrap()).unwrap(),
        b"{ broken secret"
    );
    std::fs::remove_file(registry_path().unwrap()).unwrap();
    assert!(resolve_existing("External").is_err());
    assert!(!registry_path().unwrap().exists());
}

#[test]
fn cache_is_scoped_by_the_active_data_directory() {
    let _outer = Sandbox::new();
    view(&ids(&["OuterOnly"]), &HashMap::new()).unwrap();
    assert!(resolve_existing("OuterOnly").unwrap().is_some());
    {
        let _inner = Sandbox::new();
        assert!(resolve_existing("OuterOnly").is_err());
        view(&ids(&["InnerOnly"]), &HashMap::new()).unwrap();
        assert!(resolve_existing("OuterOnly").unwrap().is_none());
        assert_eq!(published_ids().unwrap(), ids(&["InnerOnly"]));
    }
    assert_eq!(published_ids().unwrap(), ids(&["OuterOnly"]));
    assert!(resolve_existing("InnerOnly").unwrap().is_none());
}

#[test]
fn malformed_semantic_data_and_invalid_wb_configuration_fail_closed() {
    let _dir = Sandbox::new();
    write_atomic(
        account::get_data_dir().unwrap().join("workbuddy.json"),
        b"{invalid",
    )
    .unwrap();
    assert!(view(&ids(&["Legacy"]), &HashMap::new())
        .unwrap_err()
        .contains("WorkBuddy"));
    assert!(!registry_path().unwrap().exists());
    settings(&[]);
    view(&ids(&["Legacy"]), &HashMap::new()).unwrap();
    let valid = stored();
    let mut wrong_version = valid.clone();
    wrong_version.schema_version = 99;
    let mut broken_target = valid.clone();
    broken_target.published_models[0].native_model = "undiscovered".into();
    let mut wrong_candidate_id = valid.clone();
    wrong_candidate_id.candidates[0].id = "fake-pair".into();
    for invalid in [wrong_version, broken_target, wrong_candidate_id] {
        let bytes = serde_json::to_vec(&invalid).unwrap();
        write_atomic(registry_path().unwrap(), &bytes).unwrap();
        assert!(resolve_existing("Legacy").is_err());
        assert!(save(
            SaveModelRegistryRequest {
                revision: 1,
                published_models: vec![],
                ignored_candidate_ids: vec![],
                allow_unverified: true
            },
            &[],
            &HashMap::new()
        )
        .is_err());
        assert_eq!(std::fs::read(registry_path().unwrap()).unwrap(), bytes);
    }
    let mut unknown_secret = serde_json::to_value(valid).unwrap();
    unknown_secret["access_token"] = json!("must-not-be-accepted");
    write_atomic(
        registry_path().unwrap(),
        &serde_json::to_vec(&unknown_secret).unwrap(),
    )
    .unwrap();
    assert!(published_ids().is_err());
}

#[test]
fn external_write_between_snapshot_and_commit_returns_conflict() {
    let _dir = Sandbox::new();
    view(&ids(&["Legacy"]), &HashMap::new()).unwrap();
    let path = registry_path().unwrap();
    let mut cache = CACHE.lock().unwrap();
    let snapshot = load(&path, &mut cache).unwrap().unwrap();
    let mut proposed = snapshot.store.clone();
    next_revision(&mut proposed).unwrap();
    let mut external = snapshot.store.clone();
    next_revision(&mut external).unwrap();
    external.published_models[0].display_name = "External edit".into();
    let external_bytes = serde_json::to_vec(&external).unwrap();
    write_atomic(&path, &external_bytes).unwrap();
    assert!(persist(&path, &proposed, Some(&snapshot), &mut cache)
        .unwrap_err()
        .contains("conflict"));
    assert_eq!(std::fs::read(path).unwrap(), external_bytes);
}

#[test]
fn migration_keeps_exact_priced_and_free_models_that_were_never_called() {
    let _dir = Sandbox::new();
    let db_path = account::get_data_dir().unwrap().join("user_tokens.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE model_prices(pattern TEXT PRIMARY KEY);
        INSERT INTO model_prices VALUES ('PricedOnly-20250929'), ('priced-*');
        CREATE TABLE user_tokens(id TEXT, free_models TEXT);
        INSERT INTO user_tokens VALUES ('a', '[\"FreeOnly\",\"models/FreePrefixed\"]'),
        ('b', 'not json'), ('c', '[]');",
    )
    .unwrap();
    drop(conn);
    let before = std::fs::read(&db_path).unwrap();
    view(&ids(&["CatalogOnly"]), &HashMap::new()).unwrap();
    let published = published_ids().unwrap();
    for id in [
        "CatalogOnly",
        "PricedOnly-20250929",
        "FreeOnly",
        "FreePrefixed",
    ] {
        assert!(
            published.contains(&id.to_string()),
            "{id} must stay callable"
        );
    }
    // Wildcard prices are billing patterns, never public model names.
    assert!(!published.iter().any(|id| id.contains('*')));
    assert_eq!(
        resolve_existing("PricedOnly-20250929")
            .unwrap()
            .unwrap()
            .source_id,
        ANTIGRAVITY_SOURCE
    );
    assert_eq!(std::fs::read(&db_path).unwrap(), before);
}

#[test]
fn invalid_items_are_skipped_with_notices_instead_of_closing_the_registry() {
    let _dir = Sandbox::new();
    seed_ag(&["bad native", "GoodNative"]);
    let mapping = HashMap::from([
        ("bad key".into(), "Native".into()),
        ("BadTarget".into(), "has space".into()),
        ("Good".into(), "GoodNative".into()),
        ("models/Dual".into(), "AliasNative".into()),
    ]);
    let legacy = ids(&["Dual", "models/Dual", "Odd?"]);
    let registry = view(&legacy, &mapping).unwrap();
    let published = published_ids().unwrap();
    assert!(published.contains(&"Good".into()));
    assert!(published.contains(&"Dual".into()));
    for skipped in ["bad key", "BadTarget", "Odd?"] {
        assert!(
            !published.contains(&skipped.into()),
            "{skipped} must be skipped"
        );
    }
    // The bare ID keeps its own binding; the conflicting alias is reported.
    assert_ne!(
        resolve_existing("Dual").unwrap().unwrap().native_model,
        "AliasNative"
    );
    for expected in [
        "\"bad key\"",
        "\"has space\"",
        "\"Odd?\"",
        "\"bad native\"",
        "\"models/Dual\"",
    ] {
        assert!(
            registry
                .notices
                .iter()
                .any(|notice| notice.contains(expected)),
            "missing notice for {expected}"
        );
    }
    assert!(find(&registry, ANTIGRAVITY_SOURCE, "GoodNative").account_count == 1);
    // Later reads and saves see the same data and stay available.
    let reloaded = view(&legacy, &mapping).unwrap();
    assert_eq!(reloaded.revision, registry.revision);
    assert!(save(request(&reloaded), &legacy, &mapping).is_ok());
}

#[test]
fn absent_historical_database_is_not_created_and_empty_schema_is_not_migrated() {
    let _dir = Sandbox::new();
    let data_dir = account::get_data_dir().unwrap();
    assert!(historical_ids(&data_dir).unwrap().is_empty());
    assert!(!data_dir.join("user_tokens.db").exists());
    let conn = rusqlite::Connection::open(data_dir.join("user_tokens.db")).unwrap();
    conn.execute_batch("CREATE TABLE unrelated(value TEXT);")
        .unwrap();
    drop(conn);
    let before = std::fs::read(data_dir.join("user_tokens.db")).unwrap();
    assert!(historical_ids(&data_dir).unwrap().is_empty());
    assert_eq!(
        std::fs::read(data_dir.join("user_tokens.db")).unwrap(),
        before
    );
}

#[test]
fn serde_defaults_and_wire_field_names_match_the_contract() {
    let request: SaveModelRegistryRequest = serde_json::from_value(json!({
        "revision": 1, "published_models": [], "ignored_candidate_ids": [],
    }))
    .unwrap();
    assert!(!request.allow_unverified);
    let refresh: RefreshModelCandidatesRequest = serde_json::from_value(json!({})).unwrap();
    assert!(refresh.source_id.is_none());
    let discover: DiscoverUpstreamModelsRequest =
        serde_json::from_value(json!({"source_id": WORKBUDDY_SOURCE, "realm": "global"})).unwrap();
    assert_eq!(discover.realm, Some(workbuddy::accounts::Realm::Global));
    assert!(validate_discovery_request(&discover).is_ok());
    assert!(validate_discovery_request(&DiscoverUpstreamModelsRequest {
        source_id: ANTIGRAVITY_SOURCE.into(),
        realm: Some(workbuddy::accounts::Realm::Cn),
    })
    .is_err());
    assert!(serde_json::from_value::<DiscoverUpstreamModelsRequest>(
        json!({"source_id": WORKBUDDY_SOURCE, "realm": "moon"})
    )
    .is_err());
    assert!(
        serde_json::from_value::<VerifyModelCandidatesRequest>(json!({"candidate_ids": []}))
            .is_err()
    );
    assert!(serde_json::from_value::<SaveModelRegistryRequest>(json!({
        "revision": 1, "published_models": [], "ignored_candidate_ids": [], "token": "no",
    }))
    .is_err());
}

#[test]
fn model_registry_legacy_wildcard_rules_never_become_public_model_names() {
    let _dir = Sandbox::new();
    let mapping = HashMap::from([("claude-*".into(), "NativeCase".into())]);
    let legacy = ids(&["claude-*", "claude-active"]);
    let registry = view(&legacy, &mapping).unwrap();
    assert_eq!(published_ids().unwrap(), ids(&["claude-active"]));
    assert_eq!(
        resolve_existing("claude-active")
            .unwrap()
            .unwrap()
            .native_model,
        "NativeCase"
    );
    let reloaded = view(&legacy, &mapping).unwrap();
    assert_eq!(reloaded.revision, registry.revision);
    assert!(save(request(&reloaded), &legacy, &mapping).is_ok());
}
