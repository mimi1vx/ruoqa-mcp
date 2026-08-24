//! CLI argument parsing and transport/readonly selection (port of the
//! `argparse` logic in `__main__.py`'s `build_parser`/`main`). Split out of
//! `main.rs` so integration tests can drive it without spawning the binary.

use std::fmt;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use crate::audit::Transport;

/// Run the openQA MCP server over stdio (default) or HTTP.
#[derive(Parser, Debug, Clone)]
#[command(name = "ruoqa-mcp", version)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a CLI is a flat bag of flags, not a state machine"
)]
pub struct Cli {
    /// Transport to serve on. Defaults to the `OPENQA_MCP_TRANSPORT`
    /// environment variable, else stdio.
    #[arg(long, value_enum, conflicts_with_all = ["http", "stdio"])]
    pub transport: Option<Transport>,

    /// Deprecated: use `--transport http`.
    #[arg(long, conflicts_with = "stdio")]
    pub http: bool,

    /// Deprecated: use `--transport stdio`.
    #[arg(long)]
    pub stdio: bool,

    /// HTTP bind host.
    #[arg(long = "server", env = "OPENQA_MCP_HOST", default_value = "127.0.0.1")]
    pub host: String,

    /// HTTP bind port.
    #[arg(long, env = "OPENQA_MCP_PORT", default_value_t = 8000)]
    pub port: u16,

    /// Disable all mutating tools (default: `OPENQA_READONLY`).
    #[arg(long)]
    pub readonly: bool,

    /// Serve HTTP without authentication. Only for a trusted, isolated
    /// network: every caller gets the full write scope.
    ///
    /// Tokens are never taken from the command line (argv is world-readable);
    /// set `OPENQA_MCP_HTTP_TOKEN`/`OPENQA_MCP_HTTP_READ_TOKEN` in the
    /// environment or in `~/.env` instead.
    #[arg(long)]
    pub insecure_no_auth: bool,

    /// Public authority accepted in the `Host` header, e.g.
    /// `mcp.example.com` or `mcp.example.com:8000`. Repeatable; loopback
    /// names are always accepted.
    #[arg(
        long = "allowed-host",
        value_name = "HOST",
        env = "OPENQA_MCP_ALLOWED_HOSTS",
        value_delimiter = ','
    )]
    pub allowed_hosts: Vec<String>,

    /// Path to the audit-stream TOML configuration. Auditing is off when unset.
    #[arg(
        long = "audit-config",
        value_name = "PATH",
        env = "OPENQA_MCP_AUDIT_CONFIG"
    )]
    pub audit_config: Option<PathBuf>,
}

/// Interpret an environment variable as a boolean toggle. Deliberately not
/// clap's `env` attribute on a bool flag: clap treats mere *presence* of the
/// variable as true, but this must require a truthy value.
#[must_use]
pub fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .is_ok_and(|v| ["1", "true", "yes", "on"].contains(&v.trim().to_lowercase().as_str()))
}

/// `OPENQA_MCP_TRANSPORT` is set to a value that isn't `stdio` or `http`.
#[derive(Debug)]
pub struct InvalidTransport {
    raw: String,
}

impl fmt::Display for InvalidTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OPENQA_MCP_TRANSPORT is set to {:?}, which is not a valid transport \
             (expected \"stdio\" or \"http\")",
            self.raw
        )
    }
}

impl std::error::Error for InvalidTransport {}

impl Cli {
    /// Whether mutating tools should be disabled: the `--readonly` flag OR a
    /// truthy `OPENQA_READONLY`.
    #[must_use]
    pub fn readonly(&self) -> bool {
        self.readonly || env_flag("OPENQA_READONLY")
    }

    /// Resolve the transport to serve on. Precedence: `--transport` >
    /// `--http`/`--stdio` > `OPENQA_MCP_TRANSPORT` > stdio.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidTransport`] if `OPENQA_MCP_TRANSPORT` is set to
    /// anything other than `stdio` or `http` (case-insensitive); an unset or
    /// empty value falls through to the default instead of erroring.
    pub fn transport(&self) -> Result<Transport, InvalidTransport> {
        if let Some(transport) = self.transport {
            return Ok(transport);
        }
        if self.stdio {
            return Ok(Transport::Stdio);
        }
        if self.http {
            return Ok(Transport::Http);
        }
        match std::env::var("OPENQA_MCP_TRANSPORT") {
            Ok(raw) if !raw.trim().is_empty() => {
                Transport::from_str(&raw, true).map_err(|_| InvalidTransport { raw })
            }
            _ => Ok(Transport::Stdio),
        }
    }
}

#[cfg(test)]
#[allow(unsafe_code)] // edition 2024 requires unsafe for std::env::set_var
mod tests {
    use super::*;

    // OPENQA_MCP_HOST/PORT/TRANSPORT and OPENQA_READONLY are process-global,
    // and cargo runs tests in parallel threads within one binary, so every
    // case (env-dependent or not) lives in one #[test] fn to avoid a race.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "every env-touching case must stay in this one test fn, see comment above"
    )]
    fn cli_parsing_and_env_precedence() {
        let cli = Cli::parse_from(["ruoqa-mcp"]);
        assert!(!cli.http);
        assert!(!cli.stdio);
        assert!(cli.transport.is_none());
        assert_eq!(cli.host, "127.0.0.1");
        assert_eq!(cli.port, 8000);
        assert!(!cli.readonly());
        assert!(matches!(cli.transport().unwrap(), Transport::Stdio));
        assert!(!cli.insecure_no_auth);
        assert!(cli.allowed_hosts.is_empty());
        assert!(cli.audit_config.is_none());

        let cli = Cli::parse_from([
            "ruoqa-mcp",
            "--insecure-no-auth",
            "--allowed-host",
            "mcp.example.com",
            "--allowed-host",
            "mcp2.example.com:8000",
        ]);
        assert!(cli.insecure_no_auth);
        assert_eq!(
            cli.allowed_hosts,
            ["mcp.example.com", "mcp2.example.com:8000"]
        );

        let cli = Cli::parse_from(["ruoqa-mcp", "--server", "0.0.0.0", "--port", "9001"]);
        assert_eq!(cli.host, "0.0.0.0");
        assert_eq!(cli.port, 9001);

        let err = Cli::try_parse_from(["ruoqa-mcp", "--http", "--stdio"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        let err = Cli::try_parse_from(["ruoqa-mcp", "--transport", "http", "--stdio"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let err = Cli::try_parse_from(["ruoqa-mcp", "--transport", "stdio", "--http"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        let err = Cli::try_parse_from(["ruoqa-mcp", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));

        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp", "--stdio"])
                .transport()
                .unwrap(),
            Transport::Stdio
        ));
        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp", "--http"])
                .transport()
                .unwrap(),
            Transport::Http
        ));
        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp", "--transport", "http"])
                .transport()
                .unwrap(),
            Transport::Http
        ));
        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp", "--transport", "stdio"])
                .transport()
                .unwrap(),
            Transport::Stdio
        ));
        // clap's ValueEnum matching is case-sensitive by default.
        let err = Cli::try_parse_from(["ruoqa-mcp", "--transport", "HTTP"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
        assert!(Cli::parse_from(["ruoqa-mcp", "--readonly"]).readonly());

        // SAFETY: no other test in this binary mutates these variables.
        unsafe {
            std::env::set_var("OPENQA_MCP_HOST", "10.0.0.1");
            std::env::set_var("OPENQA_MCP_PORT", "7000");
        }
        let cli = Cli::parse_from(["ruoqa-mcp"]);
        assert_eq!(cli.host, "10.0.0.1");
        assert_eq!(cli.port, 7000);
        // A flag still overrides the env-supplied default.
        let cli = Cli::parse_from(["ruoqa-mcp", "--server", "192.168.0.1"]);
        assert_eq!(cli.host, "192.168.0.1");
        unsafe {
            std::env::remove_var("OPENQA_MCP_HOST");
            std::env::remove_var("OPENQA_MCP_PORT");
        }

        for value in ["1", "true", "TRUE", "yes", "on"] {
            unsafe { std::env::set_var("OPENQA_READONLY", value) };
            assert!(
                Cli::parse_from(["ruoqa-mcp"]).readonly(),
                "{value} should be truthy"
            );
        }
        for value in ["0", "false", "no", "", "off"] {
            unsafe { std::env::set_var("OPENQA_READONLY", value) };
            assert!(
                !Cli::parse_from(["ruoqa-mcp"]).readonly(),
                "{value} should be falsy"
            );
        }
        unsafe { std::env::remove_var("OPENQA_READONLY") };

        unsafe {
            std::env::set_var(
                "OPENQA_MCP_ALLOWED_HOSTS",
                "a.example.com,b.example.com:9000",
            );
        }
        assert_eq!(
            Cli::parse_from(["ruoqa-mcp"]).allowed_hosts,
            ["a.example.com", "b.example.com:9000"]
        );
        unsafe { std::env::remove_var("OPENQA_MCP_ALLOWED_HOSTS") };

        unsafe { std::env::set_var("OPENQA_MCP_TRANSPORT", "http") };
        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp"]).transport().unwrap(),
            Transport::Http
        ));
        // --stdio beats OPENQA_MCP_TRANSPORT=http.
        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp", "--stdio"])
                .transport()
                .unwrap(),
            Transport::Stdio
        ));
        // --transport stdio beats OPENQA_MCP_TRANSPORT=http.
        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp", "--transport", "stdio"])
                .transport()
                .unwrap(),
            Transport::Stdio
        ));
        unsafe { std::env::remove_var("OPENQA_MCP_TRANSPORT") };
        // --transport http with the var unset still resolves to Http.
        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp", "--transport", "http"])
                .transport()
                .unwrap(),
            Transport::Http
        ));

        unsafe { std::env::set_var("OPENQA_MCP_TRANSPORT", "") };
        assert!(matches!(
            Cli::parse_from(["ruoqa-mcp"]).transport().unwrap(),
            Transport::Stdio
        ));
        unsafe { std::env::set_var("OPENQA_MCP_TRANSPORT", "tcp") };
        assert!(Cli::parse_from(["ruoqa-mcp"]).transport().is_err());
        unsafe { std::env::remove_var("OPENQA_MCP_TRANSPORT") };
    }
}
