//! ruoqa-mcp library: module wiring shared between the binary and integration tests.

pub mod cli;
pub mod config;
pub mod form;
pub mod heartbeat;
pub mod query;
pub mod server;
pub mod summary;
pub mod tools;

pub use cli::Cli;
pub use config::{EnvConfig, build_client};
pub use server::OpenQaServer;
