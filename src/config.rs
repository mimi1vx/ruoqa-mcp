//! env → `ruoqa::ClientBuilder` (port of `client.py`'s `get_client`).

use std::path::PathBuf;
use std::time::Duration;

use reqwest::Certificate;
use ruoqa::{ApiKey, ApiSecret, Client, ClientBuilder, Error, Result, Timeouts, TlsMode};

const FALSE_TOKENS: [&str; 3] = ["0", "false", "no"];
const TRUE_TOKENS: [&str; 3] = ["1", "true", "yes"];
const DEFAULT_TIMEOUT_SECS: f64 = 30.0;
/// Stand-in for "no timeout": httpx's `Timeout(None)` has no Rust equivalent
/// here since `Timeouts::total` is a plain `Duration`, not `Option<Duration>`.
const DISABLED_TIMEOUT: Duration = Duration::from_hours(876_000);

/// Map `OPENQA_VERIFY` to a [`TlsMode`]. Bool-ish tokens toggle verification;
/// any other non-empty value is a path to a CA bundle. Unset/empty defaults
/// to platform verification.
///
/// # Errors
///
/// Returns [`Error::Config`] if `raw` names a CA bundle path that cannot be
/// read or parsed as PEM certificates.
#[allow(
    clippy::result_large_err,
    reason = "propagates ruoqa::Error as-is, same as ClientBuilder::build"
)]
pub fn parse_verify(raw: Option<&str>) -> Result<TlsMode> {
    let token = raw.map_or("", str::trim);
    let lowered = token.to_lowercase();
    if FALSE_TOKENS.contains(&lowered.as_str()) {
        return Ok(TlsMode::danger_accept_invalid_certs());
    }
    if token.is_empty() || TRUE_TOKENS.contains(&lowered.as_str()) {
        return Ok(TlsMode::PlatformVerifier);
    }
    let bundle = std::fs::read(token).map_err(|e| Error::Config(Box::new(e)))?;
    let certs = Certificate::from_pem_bundle(&bundle).map_err(|e| Error::Config(Box::new(e)))?;
    Ok(TlsMode::CustomCa {
        certs,
        replace_roots: true,
    })
}

/// Map `OPENQA_MCP_TIMEOUT` to a [`Timeouts`]. Default `total` is 30s;
/// `<= 0` disables it; unparseable values fall back to the default.
pub fn parse_timeout(raw: Option<&str>) -> Timeouts {
    let default = Timeouts::default().total(Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS));
    match raw.map(str::parse::<f64>) {
        None | Some(Err(_)) => default,
        Some(Ok(v)) if v <= 0.0 => default.total(DISABLED_TIMEOUT),
        Some(Ok(v)) => default.total(Duration::from_secs_f64(v)),
    }
}

/// The environment variables this server reads, collected up front so the
/// parsing logic stays pure and testable without mutating `std::env`.
pub struct EnvConfig {
    pub server: Option<String>,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub verify: Option<String>,
    pub timeout: Option<String>,
    /// Override for `ClientBuilder::config_paths`. `None` (the production
    /// default) leaves ruoqa's `client.conf` discovery untouched; tests pass
    /// `Some(vec![])` so they never read the developer's real config file.
    pub config_paths: Option<Vec<PathBuf>>,
}

impl EnvConfig {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            server: std::env::var("OPENQA_SERVER").ok(),
            api_key: std::env::var("OPENQA_API_KEY").ok(),
            api_secret: std::env::var("OPENQA_API_SECRET").ok(),
            verify: std::env::var("OPENQA_VERIFY").ok(),
            timeout: std::env::var("OPENQA_MCP_TIMEOUT").ok(),
            config_paths: None,
        }
    }
}

/// Build a `ruoqa::Client` from `env`. `OPENQA_API_KEY`/`OPENQA_API_SECRET`
/// only take effect when **both** are set, so the client is never
/// half-configured; otherwise `client.conf` discovery applies.
///
/// # Errors
///
/// Returns [`Error::Config`] if verify parsing or `ClientBuilder::build`
/// fails.
#[allow(
    clippy::result_large_err,
    reason = "propagates ruoqa::Error as-is, same as ClientBuilder::build"
)]
pub fn build_client(env: &EnvConfig) -> Result<Client> {
    let tls = parse_verify(env.verify.as_deref())?;
    let timeouts = parse_timeout(env.timeout.as_deref());
    let mut builder = ClientBuilder::new()
        .server(env.server.clone().unwrap_or_default())
        .tls(tls)
        .timeouts(timeouts);
    if let (Some(key), Some(secret)) = (&env.api_key, &env.api_secret) {
        builder = builder
            .api_key(ApiKey::new(key.as_str()))
            .api_secret(ApiSecret::new(secret.as_str()));
    }
    if let Some(paths) = env.config_paths.clone() {
        builder = builder.config_paths(paths);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_false_tokens_disable_verification() {
        for token in ["0", "false", "no", "FALSE", "  false  "] {
            assert!(matches!(
                parse_verify(Some(token)),
                Ok(TlsMode::DangerAcceptInvalid)
            ));
        }
    }

    #[test]
    fn verify_unset_or_true_tokens_use_platform_verifier() {
        for token in [
            None,
            Some("1"),
            Some("true"),
            Some("yes"),
            Some(""),
            Some("TRUE"),
        ] {
            assert!(matches!(parse_verify(token), Ok(TlsMode::PlatformVerifier)));
        }
    }

    #[test]
    fn verify_path_reads_ca_bundle() {
        // A nonexistent path surfaces as Error::Config, not a panic.
        let err = parse_verify(Some("/nonexistent/ca-bundle.pem")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn timeout_defaults_to_30s() {
        assert_eq!(parse_timeout(None).total, Duration::from_secs(30));
    }

    #[test]
    fn timeout_from_value() {
        assert_eq!(parse_timeout(Some("60")).total, Duration::from_mins(1));
    }

    #[test]
    fn timeout_zero_or_negative_disables() {
        assert_eq!(parse_timeout(Some("0")).total, DISABLED_TIMEOUT);
        assert_eq!(parse_timeout(Some("-1")).total, DISABLED_TIMEOUT);
    }

    #[test]
    fn timeout_malformed_falls_back_to_default() {
        assert_eq!(parse_timeout(Some("abc")).total, Duration::from_secs(30));
    }

    #[test]
    fn lone_api_key_is_ignored() {
        let env = EnvConfig {
            server: Some("openqa.example.com".to_string()),
            api_key: Some("DEADBEEF".to_string()),
            api_secret: None,
            verify: None,
            timeout: None,
            config_paths: Some(vec![]), // never touch the developer's real client.conf
        };
        let client = build_client(&env).unwrap();
        // A lone OPENQA_API_KEY must not reach the client's credentials.
        assert!(format!("{client:?}").contains("api_key: None"));
    }
}
