pub mod account;
pub mod account_service;
pub mod billing;
pub mod cache;
pub mod cloudflared;
pub mod config;
pub mod db;
pub mod device;
pub mod epay;
#[allow(dead_code)]
pub mod http_api;
pub mod i18n;
pub mod integration;
pub mod lightweight;
pub mod log_bridge;
pub mod logger;
pub mod migration;
pub mod oauth;
pub mod oauth_server;
pub mod payment;
pub mod portal_account;
pub mod portal_mail;
pub mod portal_settings;
pub mod process;
pub mod proxy_db;
pub mod quota;
pub mod scheduler;
pub mod security_db;
pub mod startup_quiet;
pub mod token_stats;
pub mod tray;
pub mod update_checker;
pub mod user_token_db;
pub mod version;
pub mod workbuddy;

use crate::models;

// Re-export commonly used functions to the top level of the modules namespace for easy external calling
pub use account::*;
pub use config::*;
#[allow(unused_imports)]
pub use logger::*;
#[allow(unused_imports)]
pub use quota::*;
// pub use device::*;

pub async fn fetch_quota(
    access_token: &str,
    email: &str,
    account_id: Option<&str>,
) -> crate::error::AppResult<(models::QuotaData, Option<String>)> {
    quota::fetch_quota(access_token, email, account_id).await
}
