//! Command-line entry point for the openQA MCP server (port of `__main__.py`).
//!
//! Selects the transport and, for HTTP, the bind address and credentials.
//! Flags override the environment; the environment
//! (`OPENQA_MCP_TRANSPORT`/`HOST`/`PORT`) supplies the defaults, and `~/.env`
//! supplies defaults for the environment.

use std::io;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tokio_util::sync::CancellationToken;

use ruoqa_mcp::audit::{AuditConfig, Auditor, Transport};
use ruoqa_mcp::http::{AuthConfigError, HttpAuth, HttpEnv, allowed_hosts, router};
use ruoqa_mcp::servers::build_registry;
use ruoqa_mcp::{Cli, EnvConfig, OpenQaServer, Telemetry};

fn main() -> anyhow::Result<()> {
    // Before the runtime, and therefore before any other thread exists.
    load_home_env();

    // Not `#[tokio::main]`: the runtime must be built *after* the environment
    // is populated, so that `set_var` above is provably single-threaded.
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the tokio runtime")?
        .block_on(serve());

    // `rmcp::transport::stdio()` reads the real stdin fd on a blocking-pool
    // thread; if the peer never closes its end (e.g. it was killed rather
    // than exiting), that thread stays parked in `read()` forever, and the
    // `Runtime` would hang on drop waiting for it. Exit directly instead of
    // returning through `main` so a still-open stdin never turns a clean
    // Ctrl-C into a hang.
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Fold `~/.env` into the environment so every variable this server reads —
/// including the ones consumed inside `clap`, `ruoqa` and `tracing`, which no
/// call site of ours can intercept — can be configured there. A variable that
/// is already set always wins.
#[allow(
    unsafe_code,
    reason = "the only sound place to call set_var: main's first statement"
)]
fn load_home_env() {
    for (key, value) in ruoqa_mcp::dotenv::read_home_env() {
        if std::env::var_os(&key).is_none() {
            // SAFETY: this runs as the first statement of `main`, before the
            // tokio runtime is built and before anything else spawns a thread,
            // so no concurrent environment access is possible.
            unsafe { std::env::set_var(key, value) };
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    // `--help`/`--version` must work even with a dead collector configured,
    // so this comes before telemetry. Preflight, fatal, still before any
    // socket: resolve `OTEL_*`, probe the collector, start its export task.
    let cli = Cli::parse();
    let telemetry = Telemetry::init().await?;
    init_tracing(telemetry.as_ref());
    run(cli, telemetry).await
}

/// Composes the stderr `fmt` layer (unchanged: `RUST_LOG`, ERROR default)
/// with the OTLP diagnostics layer (`RUST_LOG`, INFO default) when telemetry
/// is configured. `Option<Layer>` is itself a `Layer`, so "off means off"
/// needs no `cfg` and no boxing: with no `OTEL_*` set, the registry holds
/// exactly the one fmt layer.
fn init_tracing(telemetry: Option<&Telemetry>) {
    use tracing_subscriber::prelude::*;

    // Logging must go to stderr: anything on stdout corrupts the stdio JSON-RPC stream.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
        )
        .with(telemetry.and_then(Telemetry::diagnostics_layer))
        .init();
}

async fn run(cli: Cli, telemetry: Option<Telemetry>) -> anyhow::Result<()> {
    let readonly = cli.readonly();
    let transport = cli.transport().context("invalid OPENQA_MCP_TRANSPORT")?;
    if cli.http || cli.stdio {
        // Straight to stderr, not `tracing::warn!`: with RUST_LOG unset the
        // subscriber only passes ERROR, and this banner must never be silent.
        eprintln!("WARNING: --http/--stdio are deprecated; use --transport http|stdio");
    }
    let is_http = matches!(transport, Transport::Http);
    let http_env = HttpEnv::from_env();
    if !is_http {
        // Passing the flag to a stdio run is a mistake worth stopping for. The
        // same value arriving from the environment or `~/.env` is not: that is
        // daemon configuration, and an ad-hoc stdio run just ignores it, the
        // way it ignores the tokens sitting next to it.
        if std::env::args().any(|arg| arg.starts_with("--allowed-host")) {
            return Err(AuthConfigError::AllowedHostsWithoutHttp.into());
        }
        if !cli.allowed_hosts.is_empty() || !http_env.is_empty() {
            tracing::debug!("HTTP settings are configured but the transport is stdio; ignoring");
        }
    }
    // Resolve credentials before anything else: a misconfiguration must never
    // reach the point where a socket is bound.
    let auth = is_http
        .then(|| HttpAuth::resolve(&http_env, cli.insecure_no_auth))
        .transpose()?;

    // Config -> sink, before the registry and well before a socket is bound.
    // Telemetry is already up (resolved and probed in `serve()`, before this
    // function even started), so the sink can bridge onto the OTLP pipeline
    // from the moment it opens.
    let log_producer = telemetry.as_ref().and_then(Telemetry::log_producer);
    let trace_producer = telemetry.as_ref().and_then(Telemetry::trace_producer);
    let audit = build_auditor(cli.audit_config.as_deref(), log_producer)?;

    let servers =
        build_registry(&EnvConfig::from_env()).context("failed to build openQA server registry")?;
    let server_ids = servers.identifiers().join(",");
    ruoqa_mcp::heartbeat::interval().context("invalid OPENQA_MCP_HEARTBEAT_INTERVAL")?;
    let call_timeout =
        ruoqa_mcp::config::call_timeout().context("invalid OPENQA_MCP_CALL_TIMEOUT")?;
    let server = OpenQaServer::new(servers, readonly)
        .with_call_timeout(call_timeout)
        .with_audit(audit.clone())
        .with_transport(transport)
        .with_traces(trace_producer);
    // One event, not five: everything an operator with a default `RUST_LOG`
    // needs to confirm the process came up the way they configured it.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        transport = ?transport,
        tool_count = server.tool_count(),
        servers = %server_ids,
        audit = audit.is_some(),
        "ruoqa-mcp starting"
    );
    let result = match auth {
        Some(auth) => run_http(server, auth, &cli).await,
        None => run_stdio(server).await,
    };
    tracing::info!(ok = result.is_ok(), "ruoqa-mcp shutting down");
    if result.is_ok()
        && let Some(audit) = &audit
    {
        audit.shutdown(audit.process_session(), transport);
    }
    // Unconditional, unlike the audit shutdown above: telemetry about a
    // failing run is the most valuable kind, and the 5 s budget bounds the
    // cost. The stdio path's `process::exit(0)` runs no destructors, so this
    // is the only flush that ever happens.
    if let Some(telemetry) = telemetry {
        telemetry.shutdown().await;
    }
    result
}

/// Parse `path` (if given) into an [`Auditor`], opening its sink and
/// bridging it onto `log_producer` when telemetry is configured. `None`
/// (for `path`) disables auditing entirely: no config was requested at all.
fn build_auditor(
    path: Option<&std::path::Path>,
    log_producer: Option<ruoqa_mcp::LogProducer>,
) -> anyhow::Result<Option<Arc<Auditor>>> {
    let Some(path) = path else { return Ok(None) };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read audit config {}", path.display()))?;
    let cfg = AuditConfig::parse(&text)?;
    let mut auditor = Auditor::open(&cfg).with_context(|| {
        format!(
            "failed to open the audit sink configured in {}",
            path.display()
        )
    })?;
    if let Some(producer) = log_producer {
        auditor = auditor.with_otlp(producer);
    }
    Ok(Some(Arc::new(auditor)))
}

/// Cancel `ct` on Ctrl-C or, on Unix, SIGTERM; a background task so callers
/// can `select!`/await the server's own completion independently. SIGTERM
/// handling matters in a container: this process is PID 1 there, and Linux
/// discards default-action signals for PID 1 when no handler is installed,
/// so `docker stop`/`container stop` would otherwise block for the full
/// timeout and end in SIGKILL instead of the graceful shutdown both
/// transports already implement.
fn cancel_on_signal(ct: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let Ok(mut sigterm) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                let _ = tokio::signal::ctrl_c().await;
                ct.cancel();
                return;
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        ct.cancel();
    });
}

async fn run_stdio(server: OpenQaServer) -> anyhow::Result<()> {
    let ct = CancellationToken::new();
    cancel_on_signal(ct.clone());
    // A client may never send `initialize` (e.g. Ctrl-C pressed before any
    // input arrives); race the handshake itself against cancellation too, or
    // this would otherwise ignore Ctrl-C until stdin closes.
    let result = tokio::select! {
        result = server.serve_with_ct(stdio(), ct.clone()) => {
            match result {
                Ok(running) => running.waiting().await.map(|_| ()).map_err(anyhow::Error::from),
                Err(e) => Err(anyhow::Error::from(e)),
            }
        }
        () = ct.cancelled() => Ok(()),
    };
    // Whichever branch won, a Ctrl-C-triggered cancellation is a clean exit,
    // not an error (e.g. `serve_with_ct` may itself surface the same
    // cancellation as `ServerInitializeError::Cancelled`).
    if ct.is_cancelled() { Ok(()) } else { result }
}

async fn run_http(server: OpenQaServer, auth: HttpAuth, cli: &Cli) -> anyhow::Result<()> {
    let (host, port) = (cli.host.as_str(), cli.port);
    let ct = CancellationToken::new();
    if auth.is_insecure() {
        // Straight to stderr, not `tracing::warn!`: with RUST_LOG unset the
        // subscriber only passes ERROR, and this banner must never be silent.
        eprintln!(
            "WARNING: --insecure-no-auth: HTTP is served without authentication; \
             every caller gets the full write scope"
        );
    }
    let auth_enforced = !auth.is_insecure();
    let server = server.with_scope_enforcement(auth_enforced);
    let router = router(
        server,
        Arc::new(auth),
        allowed_hosts(&cli.allowed_hosts),
        &ct,
    );
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .with_context(|| format!("failed to bind {host}:{port}"))?;
    tracing::info!(host, port, auth_enforced, "listening");

    cancel_on_signal(ct.clone());
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { ct.cancelled().await })
        .await?;
    Ok(())
}
