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

    /// A send handle onto the logs pipeline, for [`audit::Auditor::with_otlp`]
    /// to piggyback the audit stream on. `None` when the logs signal is not
    /// configured.
    #[must_use]
    pub fn log_producer(&self) -> Option<LogProducer> {
        self.0.log_producer().map(LogProducer)
    }

    /// A send handle onto the traces pipeline plus the resolved
    /// `OTEL_TRACES_SAMPLER`, for [`server::OpenQaServer::with_traces`].
    /// `None` when the traces signal is not configured.
    #[must_use]
    pub fn trace_producer(&self) -> Option<TraceProducer> {
        self.0
            .trace_producer()
            .map(|(producer, sampler)| TraceProducer { producer, sampler })
    }

    /// A handle onto the metrics registry, for
    /// [`server::OpenQaServer::with_metrics`] to record one `record_call`
    /// per completed tool call. `None` when the metrics signal is not
    /// configured: no registry exists, so a tool call does no encoding, no
    /// allocation, and takes no lock.
    #[must_use]
    pub fn metric_recorder(&self) -> Option<MetricRecorder> {
        self.0.metric_recorder().map(MetricRecorder)
    }

    /// The diagnostics `tracing` `Layer`: one `LogRecord` per non-excluded
    /// event, filtered by `RUST_LOG` (default INFO) and the pipeline's own
    /// target exclusion. `None` when the logs signal is not configured —
    /// `Option<Layer>` is itself a `Layer`, so composing it into a
    /// `tracing_subscriber::registry()` needs no `cfg` and no boxing.
    #[must_use]
    pub fn diagnostics_layer<S>(
        &self,
    ) -> Option<impl tracing_subscriber::Layer<S> + Send + Sync + 'static>
    where
        S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    {
        self.0.diagnostics_layer()
    }
}

/// Opaque send handle onto the logs export pipeline, for the audit stream to
/// piggyback on. No `Debug`: the records it carries may include tool
/// arguments.
#[derive(Clone)]
pub struct LogProducer(otel::pipeline::Producer);

impl LogProducer {
    pub(crate) fn enqueue(&self, encoded_record: Vec<u8>) {
        self.0.enqueue(encoded_record);
    }
}

/// Opaque send handle onto the traces export pipeline, carrying the
/// resolved `OTEL_TRACES_SAMPLER` alongside it — the one place a tool call
/// needs it, so `server.rs` need not otherwise reach into `otel::env`.
#[derive(Clone)]
pub struct TraceProducer {
    producer: otel::pipeline::Producer,
    sampler: otel::env::Sampler,
}

impl TraceProducer {
    pub(crate) fn enqueue(&self, encoded_span: Vec<u8>) {
        self.producer.enqueue(encoded_span);
    }

    pub(crate) fn sampler(&self) -> otel::env::Sampler {
        self.sampler
    }
}

/// Opaque handle onto the metrics registry, for a tool call to record its
/// outcome and duration. `Clone`, like `LogProducer`/`TraceProducer`: cheap
/// (one `Arc` bump), and every completed tool call needs its own.
#[derive(Clone)]
pub struct MetricRecorder(std::sync::Arc<otel::metrics::Registry>);

impl MetricRecorder {
    pub(crate) fn record_call(
        &self,
        tool: &str,
        server: Option<&str>,
        outcome: &'static str,
        error_kind: Option<&str>,
        duration_ms: u64,
    ) {
        self.0
            .record_call(tool, server, outcome, error_kind, duration_ms);
    }
}
