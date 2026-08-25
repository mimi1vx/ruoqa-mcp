//! ruoqa-mcp library: module wiring shared between the binary and integration tests.

pub mod audit;
pub mod cli;
pub mod config;
pub mod dotenv;
pub(crate) mod error;
pub mod form;
pub mod heartbeat;
pub mod http;
pub(crate) mod otel;
pub mod query;
pub mod server;
pub mod servers;
pub mod summary;
pub mod tools;

pub use cli::Cli;
pub use config::{EnvConfig, build_client};
pub use server::OpenQaServer;
pub use servers::{ServerConfigError, ServerRegistry};
