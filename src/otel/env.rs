//! `OTEL_*` environment resolution, per the OTLP exporter specification.
//!
//! `resolve` is a pure function over a lookup closure, not a reader of
//! `std::env`: the crate already has a documented parallel-test race around
//! process-environment mutation (see `cli.rs`), and a closure lets the
//! table-driven tests below run without touching it.
//!
//! Two deliberate deviations from the spec, both load-bearing:
//!
//! - **No default endpoint.** The spec defaults the base endpoint to
//!   `http://localhost:4318`; this crate does not. Unset means off, and a
//!   spec-conformant default would have every stdio review shift trying to
//!   reach a collector nobody configured.
//! - **Unsupported variables that would change the wire are startup
//!   errors, not warnings**: any `_COMPRESSION` other than `none`, and any
//!   `_CERTIFICATE` / `_CLIENT_KEY` / `_CLIENT_CERTIFICATE`. Silently
//!   exporting differently than the operator asked is worse than refusing
//!   to start. `OTEL_RESOURCE_ATTRIBUTES` is out of scope and ignored: it
//!   adds attributes, it does not change delivery.

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use url::Url;

const DEFAULT_SERVICE_NAME: &str = "ruoqa-mcp";
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_QUEUE_SIZE: usize = 2048;
const DEFAULT_MAX_EXPORT_BATCH_SIZE: usize = 512;
const DEFAULT_SCHEDULE_DELAY_MS: u64 = 5000;

/// A `get`-style lookup, as `std::env::var(name).ok()` would produce.
type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The three OTLP signals this crate exports. `Metrics` has no caller before
/// phase F; the type stays signal-generic so that phase adds a call site,
/// not a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Logs,
    Traces,
    Metrics,
}

impl Signal {
    /// The `OTEL_EXPORTER_OTLP_<INFIX>_*` variable infix.
    fn env_infix(self) -> &'static str {
        match self {
            Signal::Logs => "_LOGS",
            Signal::Traces => "_TRACES",
            Signal::Metrics => "_METRICS",
        }
    }

    /// The path segment appended to a base endpoint.
    fn path(self) -> &'static str {
        match self {
            Signal::Logs => "v1/logs",
            Signal::Traces => "v1/traces",
            Signal::Metrics => "v1/metrics",
        }
    }
}

/// A credential-bearing header list. No `#[derive(Debug)]`: the hand-written
/// impl below never prints a key or a value, because `*_HEADERS` is a
/// credential (environment-only, no CLI flag, no audit-config key).
#[derive(Clone, Default)]
pub(crate) struct Headers(Vec<(HeaderName, HeaderValue)>);

impl fmt::Debug for Headers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Headers")
            .field("len", &self.0.len())
            .finish()
    }
}

impl Headers {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &(HeaderName, HeaderValue)> {
        self.0.iter()
    }

    pub(crate) fn to_header_map(&self) -> reqwest::header::HeaderMap {
        self.iter().cloned().collect()
    }

    /// Parses `k=v,k2=v2`; values are percent-decoded. Construction failures
    /// report `var` (the variable name) only — never the raw content, which
    /// may carry a credential.
    fn parse(var: &str, raw: &str) -> Result<Self, EnvError> {
        let mut headers = Vec::new();
        for pair in raw.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let (name, value) = pair
                .split_once('=')
                .ok_or_else(|| EnvError::invalid_header(var))?;
            let name = HeaderName::from_bytes(name.trim().as_bytes())
                .map_err(|_| EnvError::invalid_header(var))?;
            let decoded = percent_encoding::percent_decode_str(value.trim())
                .decode_utf8()
                .map_err(|_| EnvError::invalid_header(var))?;
            let value =
                HeaderValue::from_str(&decoded).map_err(|_| EnvError::invalid_header(var))?;
            headers.push((name, value));
        }
        Ok(Self(headers))
    }
}

/// Bounded-queue sizing, shared by every signal's export task. Read from the
/// OTLP `BatchLogRecordProcessor` variables rather than inventing
/// `OPENQA_MCP_*` ones.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueConfig {
    pub(crate) max_queue_size: usize,
    pub(crate) max_export_batch_size: usize,
    pub(crate) schedule_delay: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct SignalConfig {
    pub(crate) endpoint: Url,
    pub(crate) headers: Headers,
    pub(crate) timeout: Duration,
}

/// `Some` only when at least one signal resolved; a caller that gets `None`
/// builds nothing at all — no client, no task, no allocation.
#[derive(Debug, Clone)]
pub(crate) struct OtelConfig {
    pub(crate) service_name: String,
    pub(crate) logs: Option<SignalConfig>,
    pub(crate) traces: Option<SignalConfig>,
    #[allow(dead_code, reason = "the metrics signal has no caller before phase F")]
    pub(crate) metrics: Option<SignalConfig>,
    pub(crate) queue: QueueConfig,
    pub(crate) sampler: Sampler,
}

/// `OTEL_TRACES_SAMPLER`. Ratio samplers are out of scope; these two cover
/// "always record" and "respect an inbound `traceparent`'s sampled bit".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sampler {
    AlwaysOn,
    ParentBasedAlwaysOn,
}

/// An `OTEL_*` variable could not be resolved into a valid configuration.
/// Every variant carries the variable name, never the value where that value
/// might be a credential (`Header`).
#[derive(Debug)]
pub(crate) enum EnvError {
    InvalidUrl {
        var: String,
        value: String,
        source: url::ParseError,
    },
    UnsupportedProtocol {
        var: String,
        value: String,
    },
    UnsupportedCompression {
        var: String,
        value: String,
    },
    UnsupportedExporter {
        var: String,
        value: String,
    },
    UnsupportedSampler {
        value: String,
    },
    UnsupportedTlsOption {
        var: String,
    },
    InvalidHeader {
        var: String,
    },
    InvalidNumber {
        var: String,
        value: String,
    },
}

impl EnvError {
    fn invalid_header(var: &str) -> Self {
        EnvError::InvalidHeader {
            var: var.to_string(),
        }
    }
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvError::InvalidUrl { var, value, source } => {
                write!(
                    f,
                    "{var} is set to {value:?}, which is not a valid URL ({source})"
                )
            }
            EnvError::UnsupportedProtocol { var, value } => write!(
                f,
                "{var} is set to {value:?}, but only \"http/protobuf\" is supported"
            ),
            EnvError::UnsupportedCompression { var, value } => write!(
                f,
                "{var} is set to {value:?}, but compression is not supported (use \"none\")"
            ),
            EnvError::UnsupportedExporter { var, value } => write!(
                f,
                "{var} is set to {value:?}, but only \"otlp\" and \"none\" are supported"
            ),
            EnvError::UnsupportedSampler { value } => write!(
                f,
                "OTEL_TRACES_SAMPLER is set to {value:?}, but only \"always_on\" and \
                 \"parentbased_always_on\" are supported"
            ),
            EnvError::UnsupportedTlsOption { var } => {
                write!(f, "{var} is set, but custom TLS material is not supported")
            }
            EnvError::InvalidHeader { var } => {
                write!(f, "{var} is not a valid \"k=v,k2=v2\" header list")
            }
            EnvError::InvalidNumber { var, value } => {
                write!(f, "{var} is set to {value:?}, which is not a valid number")
            }
        }
    }
}

impl std::error::Error for EnvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EnvError::InvalidUrl { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// `get(name)`, filtered so an empty string counts as unset everywhere.
fn lookup(get: Lookup<'_>, name: &str) -> Option<String> {
    get(name).filter(|v| !v.is_empty())
}

fn var_name(signal: Option<Signal>, suffix: &str) -> String {
    let infix = signal.map(Signal::env_infix).unwrap_or_default();
    format!("OTEL_EXPORTER_OTLP{infix}{suffix}")
}

fn validate_protocol(get: Lookup<'_>, signal: Option<Signal>) -> Result<(), EnvError> {
    let var = var_name(signal, "_PROTOCOL");
    if let Some(value) = lookup(get, &var)
        && value != "http/protobuf"
    {
        return Err(EnvError::UnsupportedProtocol { var, value });
    }
    Ok(())
}

fn validate_compression(get: Lookup<'_>, signal: Option<Signal>) -> Result<(), EnvError> {
    let var = var_name(signal, "_COMPRESSION");
    if let Some(value) = lookup(get, &var)
        && value != "none"
    {
        return Err(EnvError::UnsupportedCompression { var, value });
    }
    Ok(())
}

fn validate_no_tls_material(get: Lookup<'_>, signal: Option<Signal>) -> Result<(), EnvError> {
    for suffix in ["_CERTIFICATE", "_CLIENT_KEY", "_CLIENT_CERTIFICATE"] {
        let var = var_name(signal, suffix);
        if lookup(get, &var).is_some() {
            return Err(EnvError::UnsupportedTlsOption { var });
        }
    }
    Ok(())
}

/// Appends `path` onto `base` with exactly one `/` between them. **Not**
/// `Url::join`: joining `v1/logs` onto `http://host:4318/otlp` (no trailing
/// slash) replaces the last segment, yielding `http://host:4318/v1/logs`
/// instead of the spec's `http://host:4318/otlp/v1/logs`.
fn append_path(base: &str, path: &str) -> String {
    format!("{}/{path}", base.trim_end_matches('/'))
}

fn resolve_endpoint(
    get: Lookup<'_>,
    signal: Signal,
    base_endpoint: Option<&str>,
) -> Result<Option<Url>, EnvError> {
    let signal_var = var_name(Some(signal), "_ENDPOINT");
    if let Some(value) = lookup(get, &signal_var) {
        return Url::parse(&value)
            .map(Some)
            .map_err(|source| EnvError::InvalidUrl {
                var: signal_var,
                value,
                source,
            });
    }
    let Some(base) = base_endpoint else {
        return Ok(None);
    };
    let full = append_path(base, signal.path());
    Url::parse(&full)
        .map(Some)
        .map_err(|source| EnvError::InvalidUrl {
            var: var_name(None, "_ENDPOINT"),
            value: full,
            source,
        })
}

fn resolve_headers(get: Lookup<'_>, signal: Signal) -> Result<Headers, EnvError> {
    let signal_var = var_name(Some(signal), "_HEADERS");
    if let Some(raw) = lookup(get, &signal_var) {
        return Headers::parse(&signal_var, &raw);
    }
    let base_var = var_name(None, "_HEADERS");
    match lookup(get, &base_var) {
        Some(raw) => Headers::parse(&base_var, &raw),
        None => Ok(Headers::default()),
    }
}

fn parse_millis(get: Lookup<'_>, var: &str, default: u64) -> Result<u64, EnvError> {
    match lookup(get, var) {
        Some(value) => value.parse().map_err(|_| EnvError::InvalidNumber {
            var: var.to_string(),
            value,
        }),
        None => Ok(default),
    }
}

fn resolve_timeout(get: Lookup<'_>, signal: Signal) -> Result<Duration, EnvError> {
    let signal_var = var_name(Some(signal), "_TIMEOUT");
    if lookup(get, &signal_var).is_some() {
        return Ok(Duration::from_millis(parse_millis(
            get,
            &signal_var,
            DEFAULT_TIMEOUT_MS,
        )?));
    }
    let base_var = var_name(None, "_TIMEOUT");
    Ok(Duration::from_millis(parse_millis(
        get,
        &base_var,
        DEFAULT_TIMEOUT_MS,
    )?))
}

/// `OTEL_EXPORTER_OTLP_<SIGNAL>_EXPORTER`: `otlp` (default) or `none`. `none`
/// is the per-signal off switch, checked before any endpoint work — an
/// explicit opt-out costs no URL parse and no probe.
fn resolve_exporter(get: Lookup<'_>, signal: Signal) -> Result<bool, EnvError> {
    let var = var_name(Some(signal), "_EXPORTER");
    match lookup(get, &var).as_deref() {
        None | Some("otlp") => Ok(true),
        Some("none") => Ok(false),
        Some(other) => Err(EnvError::UnsupportedExporter {
            var,
            value: other.to_string(),
        }),
    }
}

fn resolve_signal(
    get: Lookup<'_>,
    signal: Signal,
    base_endpoint: Option<&str>,
) -> Result<Option<SignalConfig>, EnvError> {
    if !resolve_exporter(get, signal)? {
        return Ok(None);
    }
    validate_protocol(get, Some(signal))?;
    validate_compression(get, Some(signal))?;
    validate_no_tls_material(get, Some(signal))?;

    let Some(endpoint) = resolve_endpoint(get, signal, base_endpoint)? else {
        return Ok(None);
    };
    let headers = resolve_headers(get, signal)?;
    let timeout = resolve_timeout(get, signal)?;
    Ok(Some(SignalConfig {
        endpoint,
        headers,
        timeout,
    }))
}

fn resolve_queue(get: Lookup<'_>) -> Result<QueueConfig, EnvError> {
    let max_queue_size = match lookup(get, "OTEL_BLRP_MAX_QUEUE_SIZE") {
        Some(v) => v.parse().map_err(|_| EnvError::InvalidNumber {
            var: "OTEL_BLRP_MAX_QUEUE_SIZE".to_string(),
            value: v,
        })?,
        None => DEFAULT_MAX_QUEUE_SIZE,
    };
    let max_export_batch_size = match lookup(get, "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE") {
        Some(v) => v.parse().map_err(|_| EnvError::InvalidNumber {
            var: "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE".to_string(),
            value: v,
        })?,
        None => DEFAULT_MAX_EXPORT_BATCH_SIZE,
    };
    let schedule_delay = Duration::from_millis(parse_millis(
        get,
        "OTEL_BLRP_SCHEDULE_DELAY",
        DEFAULT_SCHEDULE_DELAY_MS,
    )?);
    Ok(QueueConfig {
        max_queue_size,
        max_export_batch_size,
        schedule_delay,
    })
}

/// `OTEL_TRACES_SAMPLER`: `always_on` (default) or `parentbased_always_on`.
/// Validated unconditionally, like the other base variables above, whether
/// or not the traces signal ends up configured — a typo here is a startup
/// error either way, not a silently ignored one.
fn resolve_sampler(get: Lookup<'_>) -> Result<Sampler, EnvError> {
    match lookup(get, "OTEL_TRACES_SAMPLER").as_deref() {
        None | Some("always_on") => Ok(Sampler::AlwaysOn),
        Some("parentbased_always_on") => Ok(Sampler::ParentBasedAlwaysOn),
        Some(other) => Err(EnvError::UnsupportedSampler {
            value: other.to_string(),
        }),
    }
}

/// Resolves the full `OTEL_*` configuration from a lookup closure. `Ok(None)`
/// when no signal is configured (or `OTEL_SDK_DISABLED=true`) — the off
/// switch is either state, and the caller must build nothing at all.
pub(crate) fn resolve(get: Lookup<'_>) -> Result<Option<OtelConfig>, EnvError> {
    if lookup(get, "OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return Ok(None);
    }

    validate_protocol(get, None)?;
    validate_compression(get, None)?;
    validate_no_tls_material(get, None)?;
    let sampler = resolve_sampler(get)?;

    let base_endpoint = lookup(get, "OTEL_EXPORTER_OTLP_ENDPOINT");
    let logs = resolve_signal(get, Signal::Logs, base_endpoint.as_deref())?;
    let traces = resolve_signal(get, Signal::Traces, base_endpoint.as_deref())?;
    let metrics = resolve_signal(get, Signal::Metrics, base_endpoint.as_deref())?;

    if logs.is_none() && traces.is_none() && metrics.is_none() {
        return Ok(None);
    }

    let service_name =
        lookup(get, "OTEL_SERVICE_NAME").unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string());
    let queue = resolve_queue(get)?;

    Ok(Some(OtelConfig {
        service_name,
        logs,
        traces,
        metrics,
        queue,
        sampler,
    }))
}

/// The one thin wrapper reading the real process environment.
pub(crate) fn from_env() -> Result<Option<OtelConfig>, EnvError> {
    resolve(&|name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn lookup_from(vars: &HashMap<&str, &str>) -> impl Fn(&str) -> Option<String> + use<> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| vars.get(name).cloned()
    }

    #[test]
    fn no_variables_set_resolves_to_none() {
        let get = lookup_from(&HashMap::new());
        assert!(resolve(&get).unwrap().is_none());
    }

    #[test]
    fn sdk_disabled_wins_over_everything() {
        let vars = HashMap::from([
            ("OTEL_SDK_DISABLED", "true"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
        ]);
        let get = lookup_from(&vars);
        assert!(resolve(&get).unwrap().is_none());
    }

    #[test]
    fn sdk_disabled_is_case_insensitive_and_false_is_a_noop() {
        let vars = HashMap::from([
            ("OTEL_SDK_DISABLED", "FALSE"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
        ]);
        let get = lookup_from(&vars);
        assert!(resolve(&get).unwrap().is_some());
    }

    #[test]
    fn base_endpoint_path_table() {
        let cases = [
            ("http://host:4318", "http://host:4318/v1/logs"),
            ("http://host:4318/", "http://host:4318/v1/logs"),
            ("http://host:4318/otlp", "http://host:4318/otlp/v1/logs"),
            ("http://host:4318/otlp/", "http://host:4318/otlp/v1/logs"),
        ];
        for (base, expected) in cases {
            let vars = HashMap::from([("OTEL_EXPORTER_OTLP_ENDPOINT", base)]);
            let get = lookup_from(&vars);
            let cfg = resolve(&get).unwrap().expect("some config");
            let logs = cfg.logs.expect("logs enabled by base endpoint");
            assert_eq!(logs.endpoint.as_str(), expected, "base={base}");
        }
    }

    #[test]
    fn signal_endpoint_alone_enables_that_signal_only() {
        let vars = HashMap::from([("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "http://collector/logs")]);
        let get = lookup_from(&vars);
        let cfg = resolve(&get).unwrap().expect("some config");
        assert_eq!(cfg.logs.unwrap().endpoint.as_str(), "http://collector/logs");
        assert!(cfg.traces.is_none());
        assert!(cfg.metrics.is_none());
    }

    #[test]
    fn signal_endpoint_used_verbatim_no_path_appended() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://base:4318"),
            (
                "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
                "http://other:9999/custom",
            ),
        ]);
        let get = lookup_from(&vars);
        let cfg = resolve(&get).unwrap().unwrap();
        assert_eq!(
            cfg.logs.unwrap().endpoint.as_str(),
            "http://other:9999/custom"
        );
        // Base still lights up traces/metrics on its own path.
        assert_eq!(
            cfg.traces.unwrap().endpoint.as_str(),
            "http://base:4318/v1/traces"
        );
    }

    #[test]
    fn protocol_accepted_and_rejected_combinations() {
        for (base, logs) in [
            (Some("http/protobuf"), None),
            (None, Some("http/protobuf")),
            (Some("http/protobuf"), Some("http/protobuf")),
        ] {
            let mut vars = HashMap::from([("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318")]);
            if let Some(b) = base {
                vars.insert("OTEL_EXPORTER_OTLP_PROTOCOL", b);
            }
            if let Some(l) = logs {
                vars.insert("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", l);
            }
            let get = lookup_from(&vars);
            assert!(
                resolve(&get).unwrap().is_some(),
                "base={base:?} logs={logs:?}"
            );
        }

        for (var, value) in [
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
            ("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "grpc"),
        ] {
            let vars = HashMap::from([
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
                (var, value),
            ]);
            let get = lookup_from(&vars);
            let err = resolve(&get).unwrap_err();
            assert!(
                matches!(err, EnvError::UnsupportedProtocol { .. }),
                "{var}={value}"
            );
        }
    }

    #[test]
    fn compression_none_is_accepted_anything_else_rejected() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_EXPORTER_OTLP_COMPRESSION", "none"),
        ]);
        assert!(resolve(&lookup_from(&vars)).unwrap().is_some());

        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_EXPORTER_OTLP_COMPRESSION", "gzip"),
        ]);
        assert!(matches!(
            resolve(&lookup_from(&vars)).unwrap_err(),
            EnvError::UnsupportedCompression { .. }
        ));
    }

    #[test]
    fn tls_material_variables_are_rejected() {
        for suffix in ["_CERTIFICATE", "_CLIENT_KEY", "_CLIENT_CERTIFICATE"] {
            let var = format!("OTEL_EXPORTER_OTLP{suffix}");
            let vars = HashMap::from([
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
                (var.as_str(), "/path/to/thing"),
            ]);
            let err = resolve(&lookup_from(&vars)).unwrap_err();
            assert!(
                matches!(err, EnvError::UnsupportedTlsOption { .. }),
                "{var}"
            );
        }
    }

    #[test]
    fn per_signal_headers_shadow_base_not_merge() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "base=1"),
            ("OTEL_EXPORTER_OTLP_LOGS_HEADERS", "logs=2"),
        ]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        let logs_headers: Vec<_> = cfg.logs.unwrap().headers.iter().cloned().collect();
        assert_eq!(logs_headers.len(), 1);
        assert_eq!(logs_headers[0].0, HeaderName::from_static("logs"));

        let traces_headers: Vec<_> = cfg.traces.unwrap().headers.iter().cloned().collect();
        assert_eq!(traces_headers.len(), 1);
        assert_eq!(traces_headers[0].0, HeaderName::from_static("base"));
    }

    #[test]
    fn header_value_is_percent_decoded() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=api%20key"),
        ]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        let headers: Vec<_> = cfg.logs.unwrap().headers.iter().cloned().collect();
        assert_eq!(headers[0].1, HeaderValue::from_static("api key"));
    }

    #[test]
    fn empty_string_counts_as_unset_for_every_variable() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", ""),
            ("OTEL_SERVICE_NAME", ""),
            ("OTEL_EXPORTER_OTLP_HEADERS", ""),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", ""),
            ("OTEL_SDK_DISABLED", ""),
        ]);
        assert!(resolve(&lookup_from(&vars)).unwrap().is_none());
    }

    #[test]
    fn service_name_default_and_override() {
        // Off (no endpoint) never surfaces service_name, so light up a signal.
        let vars = HashMap::from([("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318")]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        assert_eq!(cfg.service_name, "ruoqa-mcp");

        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_SERVICE_NAME", "my-service"),
        ]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        assert_eq!(cfg.service_name, "my-service");
    }

    #[test]
    fn timeout_defaults_and_parses_milliseconds() {
        let vars = HashMap::from([("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318")]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        assert_eq!(cfg.logs.unwrap().timeout, Duration::from_millis(10_000));

        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_EXPORTER_OTLP_LOGS_TIMEOUT", "2500"),
        ]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        assert_eq!(cfg.logs.unwrap().timeout, Duration::from_millis(2500));
    }

    #[test]
    fn queue_sizing_defaults_and_overrides() {
        let vars = HashMap::from([("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318")]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        assert_eq!(cfg.queue.max_queue_size, 2048);
        assert_eq!(cfg.queue.max_export_batch_size, 512);
        assert_eq!(cfg.queue.schedule_delay, Duration::from_millis(5000));

        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_BLRP_MAX_QUEUE_SIZE", "10"),
            ("OTEL_BLRP_MAX_EXPORT_BATCH_SIZE", "5"),
            ("OTEL_BLRP_SCHEDULE_DELAY", "100"),
        ]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        assert_eq!(cfg.queue.max_queue_size, 10);
        assert_eq!(cfg.queue.max_export_batch_size, 5);
        assert_eq!(cfg.queue.schedule_delay, Duration::from_millis(100));
    }

    #[test]
    fn config_debug_never_contains_a_header_value() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                "authorization=super-secret-token",
            ),
        ]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("super-secret-token"));
    }

    #[test]
    fn exporter_default_and_otlp_are_the_same_as_unset() {
        for value in [None, Some("otlp")] {
            let mut vars = HashMap::from([("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318")]);
            if let Some(v) = value {
                vars.insert("OTEL_EXPORTER_OTLP_TRACES_EXPORTER", v);
            }
            let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
            assert!(cfg.traces.is_some(), "{value:?}");
        }
    }

    #[test]
    fn exporter_none_disables_only_that_signal() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_EXPORTER_OTLP_TRACES_EXPORTER", "none"),
        ]);
        let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
        assert!(cfg.traces.is_none());
        assert!(cfg.logs.is_some());
    }

    #[test]
    fn exporter_none_costs_no_url_parse_even_with_a_bad_signal_endpoint() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_EXPORTER_OTLP_TRACES_EXPORTER", "none"),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "not a url"),
        ]);
        assert!(resolve(&lookup_from(&vars)).unwrap().is_some());
    }

    #[test]
    fn exporter_invalid_value_is_an_error() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_EXPORTER_OTLP_TRACES_EXPORTER", "grpc"),
        ]);
        assert!(matches!(
            resolve(&lookup_from(&vars)).unwrap_err(),
            EnvError::UnsupportedExporter { .. }
        ));
    }

    #[test]
    fn sampler_default_and_explicit_values() {
        for (value, expected) in [
            (None, Sampler::AlwaysOn),
            (Some("always_on"), Sampler::AlwaysOn),
            (Some("parentbased_always_on"), Sampler::ParentBasedAlwaysOn),
        ] {
            let mut vars = HashMap::from([("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318")]);
            if let Some(v) = value {
                vars.insert("OTEL_TRACES_SAMPLER", v);
            }
            let cfg = resolve(&lookup_from(&vars)).unwrap().unwrap();
            assert_eq!(cfg.sampler, expected, "{value:?}");
        }
    }

    #[test]
    fn sampler_unknown_value_is_an_error() {
        let vars = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host:4318"),
            ("OTEL_TRACES_SAMPLER", "ratio"),
        ]);
        assert!(matches!(
            resolve(&lookup_from(&vars)).unwrap_err(),
            EnvError::UnsupportedSampler { .. }
        ));
    }

    #[test]
    fn sampler_is_validated_even_with_no_signal_configured() {
        let vars = HashMap::from([("OTEL_TRACES_SAMPLER", "ratio")]);
        assert!(matches!(
            resolve(&lookup_from(&vars)).unwrap_err(),
            EnvError::UnsupportedSampler { .. }
        ));
    }
}
