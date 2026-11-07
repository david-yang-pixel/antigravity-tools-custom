//! ChatGPT subscription ("Codex OAuth") protocol rules. Everything here is pure: identity
//! claims, client identity headers, quota headers, request payload rules and error
//! classification. Networking lives with the callers.
#[path = "chatgpt/errors.rs"]
pub mod errors;
#[path = "chatgpt/headers.rs"]
pub mod headers;
#[path = "chatgpt/identity.rs"]
pub mod identity;
#[path = "chatgpt/payload.rs"]
pub mod payload;
#[path = "chatgpt/quota.rs"]
pub mod quota;

#[cfg(test)]
#[path = "chatgpt/tests.rs"]
mod tests;
