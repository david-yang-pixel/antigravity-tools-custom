use super::*;
use crate::modules::{billing, model_verification};

fn account_with_candidate() -> crate::models::Account {
    let mut account = account::upsert_account(
        "catalog@example.test".into(),
        None,
        crate::models::TokenData::new(
            "access".into(),
            "refresh".into(),
            3600,
            None,
            Some("project".into()),
            None,
            false,
            None,
        ),
    )
    .unwrap();
    let mut quota = crate::models::QuotaData::new();
    quota.models.push(
        serde_json::from_value(
            serde_json::json!({"name":"candidate-model","percentage":100,"reset_time":""}),
        )
        .unwrap(),
    );
    account.quota = Some(quota);
    account::save_account(&account).unwrap();
    account
}

#[test]
fn model_catalog_requires_success_then_explicit_add_and_keeps_unpriced_models_manageable() {
    let _data = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    crate::modules::user_token_db::init_db().unwrap();
    let account = account_with_candidate();
    let rows = candidates(Provider::Antigravity).unwrap();
    assert_eq!(rows[0].status, "untested");
    assert!(!rows[0].added);
    assert!(edit(
        Provider::Antigravity,
        vec!["candidate-model".into()],
        vec![]
    )
    .is_err());
    model_verification::record(
        &account,
        "candidate-model",
        model_verification::Outcome::Verified,
    )
    .unwrap();
    assert_eq!(
        candidates(Provider::Antigravity).unwrap()[0].status,
        "verified"
    );
    assert!(
        managed_models().unwrap().is_empty(),
        "Successful tests never add models"
    );
    let available = HashSet::from(["candidate-model".to_string()]);
    assert!(published(available.clone(), &HashMap::new())
        .unwrap()
        .is_empty());
    edit(
        Provider::Antigravity,
        vec!["candidate-model".into()],
        vec![],
    )
    .unwrap();
    assert_eq!(managed_models().unwrap(), vec!["candidate-model"]);
    assert!(
        billing::load_prices().unwrap().is_empty(),
        "Adding never assigns a price"
    );
    assert_eq!(
        published(available, &HashMap::new()).unwrap(),
        vec!["candidate-model"]
    );
    assert!(
        published(HashSet::new(), &HashMap::new())
            .unwrap()
            .is_empty(),
        "Lost capability hides a model but retains admin pricing choices"
    );
    assert_eq!(managed_models().unwrap(), vec!["candidate-model"]);
    edit(
        Provider::Antigravity,
        vec![],
        vec!["candidate-model".into()],
    )
    .unwrap();
    assert!(managed_models().unwrap().is_empty());
}

#[test]
fn model_catalog_selection_does_not_switch_provider_implicitly() {
    let _data = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
    crate::modules::user_token_db::init_db().unwrap();
    let account = account_with_candidate();
    model_verification::record(
        &account,
        "candidate-model",
        model_verification::Outcome::Verified,
    )
    .unwrap();
    seed_selection(&[("candidate-model", Provider::Workbuddy)]);
    assert!(edit(
        Provider::Antigravity,
        vec!["candidate-model".into()],
        vec![]
    )
    .is_err());
    edit(
        Provider::Antigravity,
        vec![],
        vec!["candidate-model".into()],
    )
    .unwrap();
    assert_eq!(
        selected_provider("candidate-model"),
        Some(Provider::Workbuddy)
    );
    assert!(
        published(HashSet::from(["candidate-model".into()]), &HashMap::new())
            .unwrap()
            .is_empty(),
        "Antigravity proof cannot establish WorkBuddy support"
    );
}
