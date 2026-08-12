//! HTTP transport: bearer authentication, per-request scopes, and the `Host`
//! allowlist.
//!
//! Authorization is a property of the caller, not of the process: a request
//! authenticates with one of two bearer tokens, and the resolved [`Scope`] is
//! attached to the request so the tool handler can reject a mutating call from
//! a read-only principal.

use std::fmt;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{StatusCode, request::Parts};
use axum::middleware::Next;
use axum::response::Response;
use rmcp::RoleServer;
use rmcp::service::RequestContext;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

use crate::OpenQaServer;

/// Shortest accepted token. 32 characters of `openssl rand -hex 32` output is
/// well past guessable; anything shorter is likely a passphrase.
const MIN_TOKEN_LEN: usize = 32;

/// rmcp's own default; kept even when public authorities are configured, so a
/// deployment never loses loopback access by naming one hostname.
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// What a request's principal is allowed to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Read-only tools only.
    Read,
    /// Every tool, including the mutating ones.
    Write,
}

/// The HTTP-auth environment, collected up front so the resolution logic stays
/// pure and testable without mutating `std::env`.
#[derive(Debug, Default, Clone)]
pub struct HttpEnv {
    pub token: Option<String>,
    pub read_token: Option<String>,
}

impl HttpEnv {
    /// Read the token variables. `~/.env` has already been folded into the
    /// environment by then (see [`crate::dotenv`]); an empty value counts as
    /// unset, which is how a variable is cleared in a unit file.
    #[must_use]
    pub fn from_env() -> Self {
        let lookup = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Self {
            token: lookup("OPENQA_MCP_HTTP_TOKEN"),
            read_token: lookup("OPENQA_MCP_HTTP_READ_TOKEN"),
        }
    }

    /// Whether any token variable is set at all (used to report ignored
    /// credentials under stdio).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.token.is_none() && self.read_token.is_none()
    }
}

/// Why HTTP authentication could not be configured. Every variant is a startup
/// error: a half-configured credential must never degrade into open access.
#[derive(Debug)]
pub enum AuthConfigError {
    /// `--http` without any token and without `--insecure-no-auth`.
    NoToken,
    /// `--insecure-no-auth` combined with a configured token.
    InsecureWithToken,
    /// A token is shorter than [`MIN_TOKEN_LEN`].
    TooShort { var: &'static str },
    /// A token contains whitespace or non-printable ASCII.
    NotPrintable { var: &'static str },
    /// The read token is the write token.
    TokensEqual,
    /// The `--allowed-host` flag given without `--http`.
    AllowedHostsWithoutHttp,
}

impl fmt::Display for AuthConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoToken => write!(
                f,
                "--http requires a bearer token: set OPENQA_MCP_HTTP_TOKEN in the environment \
                 or in ~/.env, or pass --insecure-no-auth to serve without authentication"
            ),
            Self::InsecureWithToken => write!(
                f,
                "--insecure-no-auth conflicts with a configured HTTP token; drop one of them"
            ),
            Self::TooShort { var } => write!(
                f,
                "the token in {var} is shorter than {MIN_TOKEN_LEN} characters"
            ),
            Self::NotPrintable { var } => write!(
                f,
                "the token in {var} must consist of printable ASCII without spaces"
            ),
            Self::TokensEqual => write!(
                f,
                "the read token and the write token are identical; scopes would be meaningless"
            ),
            Self::AllowedHostsWithoutHttp => write!(f, "--allowed-host requires --http"),
        }
    }
}

impl std::error::Error for AuthConfigError {}

/// The resolved HTTP credentials.
pub struct HttpAuth {
    write: Option<String>,
    read: Option<String>,
    insecure: bool,
}

impl HttpAuth {
    /// Resolve the credentials from `env` and the `--insecure-no-auth` flag.
    ///
    /// # Errors
    ///
    /// Returns an [`AuthConfigError`] for any misconfiguration listed there;
    /// none of them is recoverable, and all are reported before a socket is
    /// bound.
    pub fn resolve(env: &HttpEnv, insecure: bool) -> Result<Self, AuthConfigError> {
        let write = validate("OPENQA_MCP_HTTP_TOKEN", env.token.as_deref())?;
        let read = validate("OPENQA_MCP_HTTP_READ_TOKEN", env.read_token.as_deref())?;
        if let (Some(w), Some(r)) = (&write, &read)
            && w == r
        {
            return Err(AuthConfigError::TokensEqual);
        }
        match (insecure, write.is_none() && read.is_none()) {
            (true, false) => Err(AuthConfigError::InsecureWithToken),
            (false, true) => Err(AuthConfigError::NoToken),
            _ => Ok(Self {
                write,
                read,
                insecure,
            }),
        }
    }

    /// Whether authentication is disabled entirely.
    #[must_use]
    pub fn is_insecure(&self) -> bool {
        self.insecure
    }

    /// The scope granted by an `Authorization` header value, or `None` when the
    /// header is missing, malformed, or carries an unknown token.
    #[must_use]
    pub fn scope_for(&self, header: Option<&str>) -> Option<Scope> {
        let token = header.and_then(bearer_token)?;
        // Both comparisons always run: an early return on the write token would
        // leak, by timing, which credential a guess matched.
        let is_write = matches(self.write.as_deref(), token);
        let is_read = matches(self.read.as_deref(), token);
        match (is_write, is_read) {
            (true, _) => Some(Scope::Write),
            (false, true) => Some(Scope::Read),
            (false, false) => None,
        }
    }
}

/// Constant-time comparison against a configured token. A token that is not
/// configured never matches.
fn matches(configured: Option<&str>, candidate: &str) -> bool {
    configured.is_some_and(|token| token.as_bytes().ct_eq(candidate.as_bytes()).into())
}

/// Extract the credential from an `Authorization` header value. The scheme is
/// case-insensitive per RFC 9110; any other scheme yields `None`.
fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("Bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Reject a token that could not be sent as a bearer credential, or that is
/// short enough to guess.
fn validate(var: &'static str, token: Option<&str>) -> Result<Option<String>, AuthConfigError> {
    let Some(token) = token else {
        return Ok(None);
    };
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AuthConfigError::NotPrintable { var });
    }
    if token.len() < MIN_TOKEN_LEN {
        return Err(AuthConfigError::TooShort { var });
    }
    Ok(Some(token.to_string()))
}

/// The authorities accepted in a request's `Host` header: rmcp's loopback
/// default plus whatever the operator configured.
///
/// The bind address is deliberately not part of this: binding to `0.0.0.0` (or
/// to a routable address) says nothing about which name clients should use, and
/// deriving an identity from it would silently widen the DNS-rebinding surface.
#[must_use]
pub fn allowed_hosts(configured: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|&h| h.to_string()).collect();
    for host in configured {
        if !hosts.iter().any(|h| h == host) {
            hosts.push(host.clone());
        }
    }
    hosts
}

/// Build the HTTP router: the MCP service at `/mcp` behind bearer auth.
///
/// Returned rather than served so tests can drive it over an ephemeral port.
pub fn router(
    server: OpenQaServer,
    auth: Arc<HttpAuth>,
    allowed_hosts: Vec<String>,
    ct: &CancellationToken,
) -> axum::Router {
    let service: StreamableHttpService<OpenQaServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::default(),
            StreamableHttpServerConfig::default()
                .with_cancellation_token(ct.child_token())
                .with_allowed_hosts(allowed_hosts),
        );
    axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(move |req, next| {
            authenticate(Arc::clone(&auth), req, next)
        }))
}

/// Reject unauthenticated requests; tag authenticated ones with their scope.
async fn authenticate(auth: Arc<HttpAuth>, mut req: Request, next: Next) -> Response {
    if auth.is_insecure() {
        return next.run(req).await;
    }
    let header = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match auth.scope_for(header) {
        Some(scope) => {
            req.extensions_mut().insert(scope);
            next.run(req).await
        }
        // No body and no detail: which token was tried is not the caller's business.
        None => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(WWW_AUTHENTICATE, "Bearer")
            .body(Body::empty())
            .expect("static 401 response is well-formed"),
    }
}

/// The scope attached by [`authenticate`], as seen from a tool handler. `None`
/// for any transport that carries no HTTP request (stdio) and for HTTP requests
/// that somehow bypassed the middleware.
#[must_use]
pub fn scope_of(context: &RequestContext<RoleServer>) -> Option<Scope> {
    context
        .extensions
        .get::<Parts>()?
        .extensions
        .get::<Scope>()
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITE: &str = "0123456789abcdef0123456789abcdef";
    const READ: &str = "fedcba9876543210fedcba9876543210";

    fn env(token: Option<&str>, read: Option<&str>) -> HttpEnv {
        HttpEnv {
            token: token.map(ToString::to_string),
            read_token: read.map(ToString::to_string),
        }
    }

    #[test]
    fn http_without_token_is_an_error() {
        assert!(matches!(
            HttpAuth::resolve(&HttpEnv::default(), false),
            Err(AuthConfigError::NoToken)
        ));
    }

    #[test]
    fn insecure_with_token_is_an_error() {
        assert!(matches!(
            HttpAuth::resolve(&env(Some(WRITE), None), true),
            Err(AuthConfigError::InsecureWithToken)
        ));
        assert!(matches!(
            HttpAuth::resolve(&env(None, Some(READ)), true),
            Err(AuthConfigError::InsecureWithToken)
        ));
    }

    #[test]
    fn insecure_without_token_is_allowed() {
        let auth = HttpAuth::resolve(&HttpEnv::default(), true).expect("resolve");
        assert!(auth.is_insecure());
        assert_eq!(auth.scope_for(None), None);
    }

    #[test]
    fn short_or_unprintable_tokens_are_errors() {
        assert!(matches!(
            HttpAuth::resolve(&env(Some("tooshort"), None), false),
            Err(AuthConfigError::TooShort { .. })
        ));
        for bad in [
            "0123456789abcdef 0123456789abcdef",
            "0123456789abcdef\t0123456789abcdef",
            "0123456789abcdef\n0123456789abcdef",
            "0123456789abcdef\u{e9}123456789abcdef",
        ] {
            assert!(
                matches!(
                    HttpAuth::resolve(&env(Some(bad), None), false),
                    Err(AuthConfigError::NotPrintable { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn identical_tokens_are_an_error() {
        assert!(matches!(
            HttpAuth::resolve(&env(Some(WRITE), Some(WRITE)), false),
            Err(AuthConfigError::TokensEqual)
        ));
    }

    #[test]
    fn scope_selection() {
        let auth = HttpAuth::resolve(&env(Some(WRITE), Some(READ)), false).expect("resolve");
        assert_eq!(
            auth.scope_for(Some(&format!("Bearer {WRITE}"))),
            Some(Scope::Write)
        );
        assert_eq!(
            auth.scope_for(Some(&format!("Bearer {READ}"))),
            Some(Scope::Read)
        );
        // The scheme is case-insensitive, the token is not.
        assert_eq!(
            auth.scope_for(Some(&format!("bearer {WRITE}"))),
            Some(Scope::Write)
        );
        assert_eq!(
            auth.scope_for(Some(&format!("Bearer {}", WRITE.to_uppercase()))),
            None
        );
    }

    #[test]
    fn read_token_alone_never_grants_write() {
        let auth = HttpAuth::resolve(&env(None, Some(READ)), false).expect("resolve");
        assert_eq!(
            auth.scope_for(Some(&format!("Bearer {READ}"))),
            Some(Scope::Read)
        );
        assert_eq!(auth.scope_for(Some(&format!("Bearer {WRITE}"))), None);
    }

    #[test]
    fn malformed_headers_yield_no_scope() {
        let auth = HttpAuth::resolve(&env(Some(WRITE), Some(READ)), false).expect("resolve");
        for header in [
            None,
            Some(""),
            Some("Bearer"),
            Some("Bearer "),
            Some(WRITE),
            Some("Basic dXNlcjpwYXNz"),
            Some("Bearer wrong-token-wrong-token-wrong"),
        ] {
            assert_eq!(
                auth.scope_for(header),
                None,
                "{header:?} must not authorize"
            );
        }
    }

    #[test]
    fn allowed_hosts_always_contain_loopback() {
        assert_eq!(allowed_hosts(&[]), LOOPBACK_HOSTS);
        assert_eq!(
            allowed_hosts(&["mcp.example.com".to_string()]),
            ["localhost", "127.0.0.1", "::1", "mcp.example.com"]
        );
        assert_eq!(
            allowed_hosts(&["mcp.example.com:8000".to_string()]),
            ["localhost", "127.0.0.1", "::1", "mcp.example.com:8000"]
        );
        // A configured loopback name is not duplicated.
        assert_eq!(allowed_hosts(&["localhost".to_string()]), LOOPBACK_HOSTS);
    }
}
