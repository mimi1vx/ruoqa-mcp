//! ruoqa-mcp library: module wiring shared between the binary and integration tests.

use anyhow::Context;

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
pub(crate) mod schema;
pub mod server;
pub mod servers;
pub mod summary;
pub mod tools;

pub use cli::Cli;
pub use config::{EnvConfig, build_client};
pub use server::OpenQaServer;
pub use servers::{ServerConfigError, ServerRegistry};

/// Opaque handle over the `OTEL_*`-configured export pipeline. Everything it
/// wraps (`OtelConfig`, the pipeline, the wire encoders) stays crate-private;
/// this is the one public surface a binary needs to start and flush it.
pub struct Telemetry(otel::Telemetry);

impl Telemetry {
    /// Resolves `OTEL_*` and, if at least one signal is configured, probes
    /// the collector and starts its export task. `Ok(None)` when telemetry
    /// is off: no client is built, no task is spawned, no allocation is made.
    ///
    /// # Errors
    ///
    /// Returns an error if an `OTEL_*` variable is invalid, or if the
    /// startup probe fails after its bounded retries — fatal by design, so
    /// this must run before a socket is bound or a stdio session is served.
    pub async fn init() -> anyhow::Result<Option<Self>> {
        match otel::env::from_env().context("invalid OTEL_* configuration")? {
            Some(cfg) => {
                let telemetry = otel::Telemetry::init(&cfg)
                    .await
                    .context("OTLP startup probe failed")?;
                Ok(Some(Self(telemetry)))
            }
            None => Ok(None),
        }
    }

    /// Flushes the export pipeline, bounded by an internal budget. Call this
    /// unconditionally on the way out, including on a failing run: telemetry
    /// about a failure is the most valuable kind.
    pub async fn shutdown(self) {
        self.0.shutdown().await;
    }
}
