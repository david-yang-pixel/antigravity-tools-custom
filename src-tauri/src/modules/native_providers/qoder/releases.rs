//! One release check per gateway, independent of account activity and identity.
use qoder_client::releases::{ReleaseChecker, ReleaseStatus};
use serde::Serialize;
use std::sync::OnceLock;

#[derive(Serialize)]
pub struct ClientVersionView {
    #[serde(flatten)]
    pub release: ReleaseStatus,
    pub advertised_version: String,
    pub legacy_protocol: bool,
}

pub async fn check(refresh: bool) -> ClientVersionView {
    static CHECKER: OnceLock<ReleaseChecker> = OnceLock::new();
    // Registry metadata uses the general upstream proxy, never an account's route.
    let proxy = super::http::proxy_url(&crate::modules::egress::Exit::Off);
    ClientVersionView {
        release: CHECKER
            .get_or_init(Default::default)
            .check(&proxy, refresh)
            .await,
        advertised_version: super::sign::version(),
        legacy_protocol: super::sign::legacy(),
    }
}
