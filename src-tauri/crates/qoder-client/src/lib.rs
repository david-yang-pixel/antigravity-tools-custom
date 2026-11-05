//! Qoder CLI protocol compatibility, isolated from the gateway's other transports.
//! The pinned upstream WASM has no filesystem, network or process imports.
pub mod identity;
pub mod releases;
pub mod signing;
pub mod transport;

pub const CLI_VERSION: &str = "1.1.83";
pub type Error = Box<dyn std::error::Error + Send + Sync>;
