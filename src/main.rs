//! Command-line entry point for the openQA MCP server (port of `__main__.py`).
//!
//! Selects the transport and, for HTTP, the bind address. Flags override the
//! environment; the environment (`OPENQA_MCP_TRANSPORT`/`HOST`/`PORT`)
//! supplies the defaults.

use std::io;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use rmcp::transport::{StreamableHttpServerConfig, stdio};
use tokio_util::sync::CancellationToken;

use ruoqa_mcp::{Cli, EnvConfig, OpenQaServer, build_client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logging must go to stderr: anything on stdout corrupts the stdio JSON-RPC stream.
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let result = run(cli).await;

    // `rmcp::transport::stdio()` reads the real stdin fd on a blocking-pool
    // thread; if the peer never closes its end (e.g. it was killed rather
    // than exiting), that thread stays parked in `read()` forever, and the
    // `Runtime` this macro builds would hang on drop waiting for it. Exit
    // directly instead of returning through `main` so a still-open stdin
    // never turns a clean Ctrl-C into a hang.
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let readonly = cli.readonly();
    let client = build_client(&EnvConfig::from_env()).context("failed to build openQA client")?;
    let server = OpenQaServer::new(client, readonly);
    if cli.use_http() {
        run_http(server, &cli.host, cli.port).await
    } else {
        run_stdio(server).await
    }
}

/// Cancel `ct` on Ctrl-C; a background task so callers can `select!`/await
/// the server's own completion independently.
fn cancel_on_ctrl_c(ct: CancellationToken) {
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        ct.cancel();
    });
}

async fn run_stdio(server: OpenQaServer) -> anyhow::Result<()> {
    let ct = CancellationToken::new();
    cancel_on_ctrl_c(ct.clone());
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

async fn run_http(server: OpenQaServer, host: &str, port: u16) -> anyhow::Result<()> {
    let ct = CancellationToken::new();
    let service: StreamableHttpService<OpenQaServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::default(),
            StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
        );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .with_context(|| format!("failed to bind {host}:{port}"))?;

    cancel_on_ctrl_c(ct.clone());
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { ct.cancelled().await })
        .await?;
    Ok(())
}
