//! `Telemetry` handle: resolves `OTEL_*`, probes the collector, and owns the
//! per-signal export pipelines for the lifetime of the process.

pub(crate) mod env;
pub(crate) mod logs;
pub(crate) mod metrics;
pub(crate) mod pipeline;
pub(crate) mod proto;
pub(crate) mod traces;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use env::{OtelConfig, SignalConfig};

/// Bounds the shutdown flush, matching the umbrella plan's "5 s budget" —
/// telemetry about a failing run is the most valuable kind, so this runs
/// unconditionally, not only on a clean exit.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Holds one [`pipeline::Exporter`] per configured signal, plus the metrics
/// signal's own registry and periodic reader.
pub(crate) struct Telemetry {
    logs: Option<pipeline::Exporter>,
    /// A second, independent export task onto the same `/v1/logs` endpoint
    /// as `logs`, for the audit stream only. `None` unless both the logs
    /// signal is configured and the caller asked for it (`--audit-config`
    /// is set) — with no audit config, no second task exists at all.
    audit: Option<pipeline::Exporter>,
    traces: Option<pipeline::Exporter>,
    metrics: Option<metrics::MetricsPipeline>,
    sampler: env::Sampler,
}

impl Telemetry {
    /// Builds the shared HTTP client, probes the collector for every
    /// configured signal, and starts that signal's export task. Each probe
    /// is awaited and fatal: a startup error here means `run()` returns
    /// before any socket is bound, on both transports.
    ///
    /// `audit_stream` starts a second, independent export task onto the
    /// same logs endpoint when `true` and the logs signal is configured —
    /// `true` exactly when `--audit-config` is set. No second probe: the one
    /// above already proved this endpoint, these headers and this encoder
    /// are accepted, and posting an identical startup record again would
    /// double a startup cost to prove nothing new.
    ///
    /// # Errors
    ///
    /// Returns an error if the `reqwest::Client` cannot be built, or if a
    /// configured signal's startup probe fails after its bounded retries.
    pub(crate) async fn init(cfg: &OtelConfig, audit_stream: bool) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .context("failed to build the OTLP HTTP client")?;

        let logs = match &cfg.logs {
            Some(signal) => {
                probe_logs(&client, &cfg.service_name, signal)
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

        let audit = match (&cfg.logs, audit_stream) {
            (Some(signal), true) => Some(start_logs_exporter(
                client.clone(),
                &cfg.service_name,
                signal,
                cfg.queue,
            )),
            _ => None,
        };

        let traces = match &cfg.traces {
            Some(signal) => {
                probe_traces(&client, &cfg.service_name, signal)
                    .await
                    .context("OTLP startup probe failed")?;
                Some(start_traces_exporter(
                    client.clone(),
                    &cfg.service_name,
                    signal,
                    cfg.queue,
                ))
            }
            None => None,
        };

        let metrics = match &cfg.metrics {
            Some(signal) => {
                probe_metrics(&client, &cfg.service_name, signal)
                    .await
                    .context("OTLP startup probe failed")?;
                let exporter =
                    start_metrics_exporter(client.clone(), &cfg.service_name, signal, cfg.queue);
                let registry = Arc::new(metrics::Registry::new(logs::now_unix_nanos()));
                Some(metrics::MetricsPipeline::start(
                    registry,
                    exporter,
                    cfg.metric_export_interval,
                ))
            }
            None => None,
        };

        Ok(Self {
            logs,
            audit,
            traces,
            metrics,
            sampler: cfg.sampler,
        })
    }

    /// Flushes every configured signal concurrently, each bounded by
    /// [`SHUTDOWN_BUDGET`]: sequential `.await`s would make the budget
    /// `SHUTDOWN_BUDGET` *per signal* on the way out of a stdio session.
    /// Called unconditionally on the way out, including on a failing run:
    /// the stdio path's `process::exit(0)` runs no destructors, so this is
    /// the only flush that happens. The audit exporter is a peer here, not
    /// flushed before or after the rest.
    pub(crate) async fn shutdown(self) {
        tokio::join!(
            maybe_shutdown_exporter(self.logs),
            maybe_shutdown_exporter(self.audit),
            maybe_shutdown_exporter(self.traces),
            maybe_shutdown_metrics(self.metrics),
        );
    }

    /// A send handle onto the logs pipeline, for the audit stream to
    /// piggyback on. `None` when the logs signal is not configured.
    pub(crate) fn log_producer(&self) -> Option<pipeline::Producer> {
        self.logs.as_ref().map(pipeline::Exporter::producer)
    }

    /// A send handle onto the audit stream's own export task, plus its
    /// delivery health for the fail-closed gate. `None` unless
    /// `audit_stream` was `true` at [`Telemetry::init`] and the logs signal
    /// is configured.
    pub(crate) fn audit_producer(&self) -> Option<(pipeline::Producer, Arc<pipeline::Health>)> {
        self.audit
            .as_ref()
            .map(|exporter| (exporter.producer(), exporter.health()))
    }

    /// A send handle onto the traces pipeline plus the resolved
    /// `OTEL_TRACES_SAMPLER`, for `OpenQaServer` to build one span per tool
    /// call. `None` when the traces signal is not configured.
    pub(crate) fn trace_producer(&self) -> Option<(pipeline::Producer, env::Sampler)> {
        self.traces
            .as_ref()
            .map(|exporter| (exporter.producer(), self.sampler))
    }

    /// A handle onto the metrics registry, for `OpenQaServer` to record one
    /// `record_call` per completed tool call. `None` when the metrics signal
    /// is not configured: no registry exists, so a tool call does no
    /// encoding, no allocation, and takes no lock.
    pub(crate) fn metric_recorder(&self) -> Option<Arc<metrics::Registry>> {
        self.metrics
            .as_ref()
            .map(metrics::MetricsPipeline::registry)
    }

    /// The diagnostics `tracing` `Layer`, filtered so `RUST_LOG` (defaulting
    /// to INFO, unlike the stderr layer's ERROR default) and the pipeline's
    /// own target exclusion both gate it. `None` when the logs signal is not
    /// configured: `Option<Layer>` is itself a `Layer`, so "off means off"
    /// costs the caller no `cfg` and no boxing.
    pub(crate) fn diagnostics_layer<S>(
        &self,
    ) -> Option<impl tracing_subscriber::Layer<S> + Send + Sync + 'static>
    where
        S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    {
        use tracing_subscriber::layer::Layer;

        self.logs.as_ref().map(|exporter| {
            let layer = logs::DiagnosticsLayer::new(exporter.producer());
            // Two independent predicates, both required: the target
            // exclusion (retiring `excluded`'s only remaining purpose) and a
            // `RUST_LOG`-driven level filter whose *default* deliberately
            // differs from the stderr layer's ERROR-only default. Documented
            // here and in the README: a collector configured with no
            // `--audit-config` sees lifecycle events, warnings and errors
            // only at the default `RUST_LOG`.
            let target_filter =
                tracing_subscriber::filter::filter_fn(|meta: &tracing::Metadata<'_>| {
                    !pipeline::excluded(meta.target())
                });
            let level_filter = tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
                .from_env_lossy();
            // Explicit turbofish on `S`: `DiagnosticsLayer` and `FilterFn`
            // both implement `Layer<S>`/`Filter<S>` generically over every
            // `S`, so plain method-call syntax leaves `S` unresolved inside
            // this closure — the eventual `impl Layer<S>` return type is not
            // fed back into the closure body during inference.
            let with_level = Layer::<S>::with_filter(layer, level_filter);
            Layer::<S>::with_filter(with_level, target_filter)
        })
    }
}

/// Flushes one logs/traces `Exporter`, if configured. Shared by both call
/// sites in `Telemetry::shutdown`: `logs` and `traces` are the same
/// `pipeline::Exporter` type, unlike `metrics`.
async fn maybe_shutdown_exporter(exporter: Option<pipeline::Exporter>) {
    if let Some(exporter) = exporter {
        exporter.shutdown(SHUTDOWN_BUDGET).await;
    }
}

/// Flushes the metrics pipeline, if configured — cancels its reader, does
/// one final collection, then flushes its own `Exporter` (see
/// [`metrics::MetricsPipeline::shutdown`] for why that ordering matters).
async fn maybe_shutdown_metrics(metrics: Option<metrics::MetricsPipeline>) {
    if let Some(metrics) = metrics {
        metrics.shutdown(SHUTDOWN_BUDGET).await;
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

fn start_traces_exporter(
    client: reqwest::Client,
    service_name: &str,
    signal: &SignalConfig,
    queue: env::QueueConfig,
) -> pipeline::Exporter {
    let resource = resource_attrs(service_name);
    let scope = scope();
    let encode_batch: pipeline::EncodeBatch = Arc::new(move |spans: &[Vec<u8>]| {
        let resource_refs: Vec<(&str, proto::Value<'_>)> = resource
            .iter()
            .map(|(k, v)| (k.as_str(), proto::Value::Str(v.as_str())))
            .collect();
        traces::encode_request(&resource_refs, (scope.0.as_str(), scope.1.as_str()), spans)
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
async fn probe_logs(
    client: &reqwest::Client,
    service_name: &str,
    signal: &SignalConfig,
) -> anyhow::Result<()> {
    let record = logs::encode_record(
        logs::now_unix_nanos(),
        logs::Severity::Info,
        "ruoqa-mcp startup probe",
        &[
            ("ruoqa.stream", proto::Value::Str("diagnostics")),
            ("event", proto::Value::Str("startup_probe")),
        ],
        None,
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

/// Posts one root INTERNAL `Span` named `"ruoqa-mcp startup probe"`. Proves
/// the collector accepts a real `Span` (not just an empty request), the same
/// argument [`probe_logs`] makes for the logs signal.
async fn probe_traces(
    client: &reqwest::Client,
    service_name: &str,
    signal: &SignalConfig,
) -> anyhow::Result<()> {
    let ctx = traces::SpanCtx::new_root()
        .ok_or_else(|| anyhow::anyhow!("failed to generate a random trace/span id"))?;
    let now = logs::now_unix_nanos();
    let span = traces::encode_span(
        ctx,
        None,
        "ruoqa-mcp startup probe",
        traces::SpanKind::Internal,
        now,
        now,
        &[("event", proto::Value::Str("startup_probe"))],
        None,
    );
    let resource = resource_attrs(service_name);
    let resource_refs: Vec<(&str, proto::Value<'_>)> = resource
        .iter()
        .map(|(k, v)| (k.as_str(), proto::Value::Str(v.as_str())))
        .collect();
    let (scope_name, scope_version) = scope();
    let body = traces::encode_request(&resource_refs, (&scope_name, &scope_version), &[span]);

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

fn start_metrics_exporter(
    client: reqwest::Client,
    service_name: &str,
    signal: &SignalConfig,
    queue: env::QueueConfig,
) -> pipeline::Exporter {
    let resource = resource_attrs(service_name);
    let scope = scope();
    let encode_batch: pipeline::EncodeBatch = Arc::new(move |bodies: &[Vec<u8>]| {
        let resource_refs: Vec<(&str, proto::Value<'_>)> = resource
            .iter()
            .map(|(k, v)| (k.as_str(), proto::Value::Str(v.as_str())))
            .collect();
        metrics::encode_request(&resource_refs, (scope.0.as_str(), scope.1.as_str()), bodies)
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

/// Posts one `ruoqa.startup` `Metric` (see
/// [`metrics::encode_startup_metric`]). Proves the collector accepts a real
/// `Metric` (not just an empty request), the same argument [`probe_logs`]
/// and [`probe_traces`] make for their own signals.
async fn probe_metrics(
    client: &reqwest::Client,
    service_name: &str,
    signal: &SignalConfig,
) -> anyhow::Result<()> {
    let metric = metrics::encode_startup_metric(logs::now_unix_nanos());
    let resource = resource_attrs(service_name);
    let resource_refs: Vec<(&str, proto::Value<'_>)> = resource
        .iter()
        .map(|(k, v)| (k.as_str(), proto::Value::Str(v.as_str())))
        .collect();
    let (scope_name, scope_version) = scope();
    let body = metrics::encode_request(&resource_refs, (&scope_name, &scope_version), &[metric]);

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
