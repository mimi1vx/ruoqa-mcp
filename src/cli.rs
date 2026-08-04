//! CLI argument parsing and transport/readonly selection (port of the
//! `argparse` logic in `__main__.py`'s `build_parser`/`main`). Split out of
//! `main.rs` so integration tests can drive it without spawning the binary.

use clap::Parser;

/// Run the openQA MCP server over stdio (default) or HTTP.
#[derive(Parser, Debug, Clone)]
#[command(name = "ruoqa-mcp", version)]
pub struct Cli {
    /// Serve over HTTP instead of stdio.
    #[arg(long, conflicts_with = "stdio")]
    pub http: bool,

    /// Serve over stdio (default; overrides `OPENQA_MCP_TRANSPORT=http`).
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
}

/// Interpret an environment variable as a boolean toggle. Deliberately not
/// clap's `env` attribute on a bool flag: clap treats mere *presence* of the
/// variable as true, but this must require a truthy value.
#[must_use]
pub fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .is_ok_and(|v| ["1", "true", "yes", "on"].contains(&v.trim().to_lowercase().as_str()))
}

impl Cli {
    /// Whether mutating tools should be disabled: the `--readonly` flag OR a
    /// truthy `OPENQA_READONLY`.
    #[must_use]
    pub fn readonly(&self) -> bool {
        self.readonly || env_flag("OPENQA_READONLY")
    }

    /// Whether to serve over HTTP. An explicit `--stdio` wins; otherwise
    /// `--http` or `OPENQA_MCP_TRANSPORT=http` selects HTTP.
    #[must_use]
    pub fn use_http(&self) -> bool {
        !self.stdio && (self.http || std::env::var("OPENQA_MCP_TRANSPORT").as_deref() == Ok("http"))
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
    fn cli_parsing_and_env_precedence() {
        let cli = Cli::parse_from(["ruoqa-mcp"]);
        assert!(!cli.http);
        assert!(!cli.stdio);
        assert_eq!(cli.host, "127.0.0.1");
        assert_eq!(cli.port, 8000);
        assert!(!cli.readonly());
        assert!(!cli.use_http());

        let cli = Cli::parse_from(["ruoqa-mcp", "--server", "0.0.0.0", "--port", "9001"]);
        assert_eq!(cli.host, "0.0.0.0");
        assert_eq!(cli.port, 9001);

        let err = Cli::try_parse_from(["ruoqa-mcp", "--http", "--stdio"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        let err = Cli::try_parse_from(["ruoqa-mcp", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));

        assert!(!Cli::parse_from(["ruoqa-mcp", "--stdio"]).use_http());
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

        unsafe { std::env::set_var("OPENQA_MCP_TRANSPORT", "http") };
        assert!(Cli::parse_from(["ruoqa-mcp"]).use_http());
        // --stdio beats OPENQA_MCP_TRANSPORT=http.
        assert!(!Cli::parse_from(["ruoqa-mcp", "--stdio"]).use_http());
        unsafe { std::env::remove_var("OPENQA_MCP_TRANSPORT") };
    }
}
