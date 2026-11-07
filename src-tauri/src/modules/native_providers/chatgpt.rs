//! ChatGPT subscription ("Codex OAuth") protocol rules. Everything here is pure: identity
//! claims, client identity headers, quota headers, request payload rules and error
//! classification, plus the network client (`auth`, `models`, `client`) bound to the
//! allowed hosts in `endpoints`.
#[path = "chatgpt/auth.rs"]
pub mod auth;
#[path = "chatgpt/client.rs"]
pub mod client;
#[path = "chatgpt/endpoints.rs"]
pub mod endpoints;
#[path = "chatgpt/errors.rs"]
pub mod errors;
#[path = "chatgpt/headers.rs"]
pub mod headers;
#[path = "chatgpt/identity.rs"]
pub mod identity;
#[path = "chatgpt/models.rs"]
pub mod models;
#[path = "chatgpt/payload.rs"]
pub mod payload;
#[path = "chatgpt/quota.rs"]
pub mod quota;
#[path = "chatgpt/release.rs"]
pub mod release;

#[cfg(test)]
#[path = "chatgpt/tests.rs"]
mod tests;
