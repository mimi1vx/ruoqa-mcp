//! `Telemetry` handle: resolves `OTEL_*`, probes the collector, and owns the
//! per-signal export pipelines for the lifetime of the process.

pub(crate) mod env;
pub(crate) mod logs;
pub(crate) mod pipeline;
pub(crate) mod proto;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use env::{OtelConfig, SignalConfig};

/// Bounds the shutdown flush, matching the umbrella plan's "5 s budget" —
/// telemetry about a failing run is the most valuable kind, so this runs
/// unconditionally, not only on a clean exit.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Holds one [`pipeline::Exporter`] per configured signal. `traces` and
/// `metrics` arrive in phases E/F; only `logs` has a caller here.
pub(crate) struct Telemetry {
    logs: Option<pipeline::Exporter>,
}

impl Telemetry {
    /// Builds the shared HTTP client, probes the collector for every
    /// configured signal, and starts that signal's export task. The probe is
    /// awaited and fatal: a startup error here means `run()` returns before
    /// any socket is bound, on both transports.
    ///
    /// # Errors
    ///
    /// Returns an error if the `reqwest::Client` cannot be built, or if the
    /// startup probe fails after its bounded retries.
    pub(crate) async fn init(cfg: &OtelConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .context("failed to build the OTLP HTTP client")?;

        let logs = match &cfg.logs {
            Some(signal) => {
                probe(&client, &cfg.service_name, signal)
                    .await
                    .context("OTLP startup probe failed")?;
                Some(start_logs_exporter(
                    client.clone(),
                    &cfg.service_name,
                    signal,
                    cfg.queue,
                ))
            }
            None => None,
        };

        Ok(Self { logs })
    }

    /// Flushes every configured exporter, bounded by [`SHUTDOWN_BUDGET`].
    /// Called unconditionally on the way out, including on a failing run:
    /// the stdio path's `process::exit(0)` runs no destructors, so this is
    /// the only flush that happens.
    pub(crate) async fn shutdown(self) {
        if let Some(logs) = self.logs {
            logs.shutdown(SHUTDOWN_BUDGET).await;
        }
    }
}

fn resource_attrs(service_name: &str) -> Vec<(String, String)> {
    vec![
        ("service.name".to_string(), service_name.to_string()),
        (
            "service.version".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
    ]
}

fn scope() -> (String, String) {
    (
        "ruoqa-mcp".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    )
}

fn start_logs_exporter(
    client: reqwest::Client,
    service_name: &str,
    signal: &SignalConfig,
    queue: env::QueueConfig,
) -> pipeline::Exporter {
    let resource = resource_attrs(service_name);
    let scope = scope();
    let encode_batch: pipeline::EncodeBatch = Arc::new(move |records: &[Vec<u8>]| {
        let resource_refs: Vec<(&str, proto::Value<'_>)> = resource
            .iter()
            .map(|(k, v)| (k.as_str(), proto::Value::Str(v.as_str())))
            .collect();
        logs::encode_request(
            &resource_refs,
            (scope.0.as_str(), scope.1.as_str()),
            records,
        )
    });
    pipeline::Exporter::start(
        client,
        signal.endpoint.clone(),
        signal.headers.to_header_map(),
        signal.timeout,
        queue,
        encode_batch,
    )
}

/// Posts one `LogRecord` — severity INFO, body `"ruoqa-mcp startup probe"` —
/// with the same bounded retry the pipeline uses. Proves the collector is
/// reachable and that the encoder produces a request it accepts, before any
/// tool call runs.
async fn probe(
    client: &reqwest::Client,
    service_name: &str,
    signal: &SignalConfig,
) -> anyhow::Result<()> {
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    let record = logs::encode_record(
        now_nanos,
        logs::Severity::Info,
        "ruoqa-mcp startup probe",
        &[
            ("ruoqa.stream", proto::Value::Str("diagnostics")),
            ("event", proto::Value::Str("startup_probe")),
        ],
    );
    let resource = resource_attrs(service_name);
    let resource_refs: Vec<(&str, proto::Value<'_>)> = resource
        .iter()
        .map(|(k, v)| (k.as_str(), proto::Value::Str(v.as_str())))
        .collect();
    let (scope_name, scope_version) = scope();
    let body = logs::encode_request(&resource_refs, (&scope_name, &scope_version), &[record]);

    pipeline::send_with_retry(
        client,
        &signal.endpoint,
        &signal.headers.to_header_map(),
        signal.timeout,
        body,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
}
