//! env → `ruoqa::ClientBuilder` (port of `client.py`'s `get_client`).

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use reqwest::Certificate;
use ruoqa::{Client, ClientBuilder, Error, Result, Timeouts, TlsMode};

const FALSE_TOKENS: [&str; 3] = ["0", "false", "no"];
const TRUE_TOKENS: [&str; 3] = ["1", "true", "yes"];
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Stand-in for "no timeout": httpx's `Timeout(None)` has no Rust equivalent
/// here since `Timeouts::total` is a plain `Duration`, not `Option<Duration>`.
const DISABLED_TIMEOUT: Duration = Duration::from_hours(876_000);

/// An environment variable holding a duration in seconds could not be
/// interpreted. Always a startup error: a value that reaches this point is
/// neither unset/empty (treated as the default) nor a well-formed number.
#[derive(Debug)]
pub struct InvalidDuration {
    var: &'static str,
    raw: String,
    reason: &'static str,
}

impl fmt::Display for InvalidDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is set to {:?}, which is not a valid duration ({})",
            self.var, self.raw, self.reason
        )
    }
}

impl std::error::Error for InvalidDuration {}

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

/// Parse an environment variable holding a duration in seconds. Unset, empty,
/// or whitespace-only yields `default`; `<= 0` yields `Ok(None)` ("disabled");
/// anything else must be a finite, in-range number of seconds or startup
/// aborts with [`InvalidDuration`] naming `var`.
///
/// # Errors
///
/// Returns [`InvalidDuration`] if `raw` is set to text that doesn't parse as
/// an `f64`, or to `NaN`, `±inf`, or a finite value too large for a
/// [`Duration`].
pub fn parse_duration_secs(
    var: &'static str,
    raw: Option<&str>,
    default: Duration,
) -> std::result::Result<Option<Duration>, InvalidDuration> {
    let trimmed = raw.map_or("", str::trim);
    if trimmed.is_empty() {
        return Ok(Some(default));
    }
    let Ok(v) = trimmed.parse::<f64>() else {
        return Err(InvalidDuration {
            var,
            raw: trimmed.to_string(),
            reason: "not a number",
        });
    };
    if !v.is_finite() {
        return Err(InvalidDuration {
            var,
            raw: trimmed.to_string(),
            reason: "must be finite",
        });
    }
    if v <= 0.0 {
        return Ok(None);
    }
    Duration::try_from_secs_f64(v)
        .map(Some)
        .map_err(|_| InvalidDuration {
            var,
            raw: trimmed.to_string(),
            reason: "out of range",
        })
}

/// Map `OPENQA_MCP_TIMEOUT` to a [`Timeouts`]. Default `total` is 30s;
/// `<= 0` disables it.
///
/// # Errors
///
/// Returns [`InvalidDuration`] if the variable is set to an unparseable,
/// non-finite, or out-of-range value.
pub fn parse_timeout(raw: Option<&str>) -> std::result::Result<Timeouts, InvalidDuration> {
    let total = parse_duration_secs("OPENQA_MCP_TIMEOUT", raw, DEFAULT_TIMEOUT)?
        .unwrap_or(DISABLED_TIMEOUT);
    Ok(Timeouts::default().total(total))
}

const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_mins(5);

/// Whole-tool-call deadline, independent of the per-HTTP-request
/// `OPENQA_MCP_TIMEOUT`; bounds fan-out (e.g. `restart_jobs`) and slow
/// upstreams together. Default 300s; `<= 0` disables it (`Ok(None)`).
///
/// # Errors
///
/// Returns [`InvalidDuration`] if `OPENQA_MCP_CALL_TIMEOUT` is set to an
/// unparseable, non-finite, or out-of-range value.
pub fn call_timeout() -> std::result::Result<Option<Duration>, InvalidDuration> {
    parse_duration_secs(
        "OPENQA_MCP_CALL_TIMEOUT",
        std::env::var("OPENQA_MCP_CALL_TIMEOUT").ok().as_deref(),
        DEFAULT_CALL_TIMEOUT,
    )
}

/// The environment variables this server reads, collected up front so the
/// parsing logic stays pure and testable without mutating `std::env`.
pub struct EnvConfig {
    pub server: Option<String>,
    pub verify: Option<String>,
    pub timeout: Option<String>,
    /// Override for `ClientBuilder::config_paths`. `None` (the production
    /// default) leaves ruoqa's `client.conf` discovery untouched; tests pass
    /// `Some(vec![])` so they never read the developer's real config file.
    pub config_paths: Option<Vec<PathBuf>>,
    /// Whether `$OPENQA_API_KEY` is set to a non-empty value. Read here
    /// (rather than inside `servers::build_registry`) so every env-derived
    /// input stays collected in one place. Must stay in sync with
    /// `ruoqa::config::API_KEY_ENV`, which is `pub(crate)` there and not
    /// importable (`ruoqa-0.2.0/src/config.rs`).
    pub api_key_set: bool,
    /// Same as `api_key_set`, for `$OPENQA_API_SECRET` /
    /// `ruoqa::config::API_SECRET_ENV`.
    pub api_secret_set: bool,
}

impl EnvConfig {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            server: std::env::var("OPENQA_SERVER").ok(),
            verify: std::env::var("OPENQA_VERIFY").ok(),
            timeout: std::env::var("OPENQA_MCP_TIMEOUT").ok(),
            config_paths: None,
            api_key_set: std::env::var("OPENQA_API_KEY").is_ok_and(|v| !v.is_empty()),
            api_secret_set: std::env::var("OPENQA_API_SECRET").is_ok_and(|v| !v.is_empty()),
        }
    }
}

/// Build a `ruoqa::Client` from `env`. Credentials are resolved by
/// `ClientBuilder::build` itself, in order: builder-supplied, then the
/// process environment, then `client.conf` discovery.
///
/// # Errors
///
/// Returns [`Error::Config`] if verify parsing fails, or propagates
/// `ClientBuilder::build`'s errors, including
/// [`Error::IncompleteCredentials`] when the environment sets only one
/// half of a credential pair.
#[allow(
    clippy::result_large_err,
    reason = "propagates ruoqa::Error as-is, same as ClientBuilder::build"
)]
pub fn build_client(env: &EnvConfig) -> Result<Client> {
    build_one(env, env.server.as_deref().unwrap_or_default())
}

/// Build a single `Client` for `server`, sharing `env`'s tls/timeout/
/// config-path settings. Used by [`build_client`] for the single-server case
/// and by [`crate::servers::build_registry`] once per `OPENQA_SERVER` entry.
#[allow(
    clippy::result_large_err,
    reason = "propagates ruoqa::Error as-is, same as ClientBuilder::build"
)]
pub(crate) fn build_one(env: &EnvConfig, server: &str) -> Result<Client> {
    let tls = parse_verify(env.verify.as_deref())?;
    let timeouts = parse_timeout(env.timeout.as_deref()).map_err(|e| Error::Config(Box::new(e)))?;
    let mut builder = ClientBuilder::new()
        .server(server)
        .tls(tls)
        .timeouts(timeouts);
    if let Some(paths) = env.config_paths.clone() {
        builder = builder.config_paths(paths);
    }
    builder.build()
}

#[cfg(test)]
#[allow(unsafe_code)] // edition 2024 requires unsafe for std::env::set_var
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
        assert_eq!(parse_timeout(None).unwrap().total, Duration::from_secs(30));
    }

    #[test]
    fn timeout_from_value() {
        assert_eq!(
            parse_timeout(Some("60")).unwrap().total,
            Duration::from_mins(1)
        );
    }

    #[test]
    fn timeout_zero_or_negative_disables() {
        assert_eq!(parse_timeout(Some("0")).unwrap().total, DISABLED_TIMEOUT);
        assert_eq!(parse_timeout(Some("-1")).unwrap().total, DISABLED_TIMEOUT);
    }

    #[test]
    fn timeout_blank_is_unset() {
        for raw in ["", "   "] {
            assert_eq!(
                parse_timeout(Some(raw)).unwrap().total,
                Duration::from_secs(30)
            );
        }
    }

    #[test]
    fn timeout_rejects_invalid_values() {
        for raw in ["abc", "nan", "NaN", "inf", "-inf", "1e30"] {
            let err = parse_timeout(Some(raw)).unwrap_err();
            assert!(err.to_string().contains("OPENQA_MCP_TIMEOUT"));
        }
    }

    #[test]
    fn call_timeout_defaults_to_300s() {
        assert_eq!(
            parse_duration_secs("OPENQA_MCP_CALL_TIMEOUT", None, DEFAULT_CALL_TIMEOUT).unwrap(),
            Some(Duration::from_mins(5))
        );
    }

    #[test]
    fn call_timeout_zero_or_negative_disables() {
        for raw in ["0", "-1"] {
            assert_eq!(
                parse_duration_secs("OPENQA_MCP_CALL_TIMEOUT", Some(raw), DEFAULT_CALL_TIMEOUT)
                    .unwrap(),
                None
            );
        }
    }

    #[test]
    fn call_timeout_rejects_invalid_values() {
        for raw in ["abc", "nan", "NaN", "inf", "-inf", "1e300"] {
            let err =
                parse_duration_secs("OPENQA_MCP_CALL_TIMEOUT", Some(raw), DEFAULT_CALL_TIMEOUT)
                    .unwrap_err();
            assert!(err.to_string().contains("OPENQA_MCP_CALL_TIMEOUT"));
        }
    }

    #[test]
    fn build_client_uses_configured_server() {
        let env = EnvConfig {
            server: Some("openqa.example.com".to_string()),
            verify: None,
            timeout: None,
            config_paths: Some(vec![]), // never touch the developer's real client.conf
            api_key_set: false,
            api_secret_set: false,
        };
        let client = build_client(&env).unwrap();
        assert_eq!(client.base_url().host_str(), Some("openqa.example.com"));
    }

    #[test]
    fn env_config_reads_credential_presence() {
        // SAFETY: no other test in this binary mutates these variables.
        unsafe {
            std::env::remove_var("OPENQA_API_KEY");
            std::env::remove_var("OPENQA_API_SECRET");
        }
        assert!(!EnvConfig::from_env().api_key_set);
        assert!(!EnvConfig::from_env().api_secret_set);

        unsafe { std::env::set_var("OPENQA_API_KEY", "") };
        assert!(!EnvConfig::from_env().api_key_set, "empty is not set");

        unsafe {
            std::env::set_var("OPENQA_API_KEY", "k");
            std::env::set_var("OPENQA_API_SECRET", "s");
        }
        assert!(EnvConfig::from_env().api_key_set);
        assert!(EnvConfig::from_env().api_secret_set);

        unsafe {
            std::env::remove_var("OPENQA_API_KEY");
            std::env::remove_var("OPENQA_API_SECRET");
        }
    }
}
