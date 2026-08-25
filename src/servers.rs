//! `OPENQA_SERVER` list parsing and the `ServerRegistry` it builds.

use std::collections::HashMap;
use std::fmt;

use crate::config::{self, EnvConfig};

/// Well-known hosts additionally selectable by a short alias.
const ALIASES: &[(&str, &str)] = &[("openqa.suse.de", "osd"), ("openqa.opensuse.org", "o3")];

/// Splits `OPENQA_SERVER` into individual entries: trimmed, non-empty,
/// comma-or-semicolon-separated. An unset or blank variable, or one with no
/// non-empty entries after trimming (e.g. `","`), yields a single implicit
/// empty entry — preserving `ClientBuilder::server("")`'s existing "use
/// client.conf's first section, else localhost" fallback.
fn split_servers(raw: Option<&str>) -> Vec<String> {
    let entries: Vec<String> = raw
        .unwrap_or_default()
        .split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if entries.is_empty() {
        vec![String::new()]
    } else {
        entries
    }
}

/// `host[:port]`, no scheme — the id a `server` selector matches against.
fn canonical_id(url: &url::Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        (None, _) => String::new(), // unreachable for a resolved base_url
    }
}

/// `Some(alias)` when `url` is exactly one of the two well-known hosts on its
/// default port (see the umbrella plan's alias-matching note).
fn alias_for(url: &url::Url) -> Option<&'static str> {
    if url.port().is_some() {
        return None;
    }
    ALIASES
        .iter()
        .find(|(host, _)| url.host_str() == Some(host))
        .map(|(_, alias)| *alias)
}

/// A resolved set of `ruoqa::Client`s, keyed by every selector a tool's
/// `server` argument may name (canonical `host[:port]` plus any alias).
#[derive(Debug, Clone)]
pub struct ServerRegistry {
    clients: HashMap<String, ruoqa::Client>,
}

impl ServerRegistry {
    /// Build a registry directly from pre-resolved clients, bypassing
    /// `OPENQA_SERVER` parsing. Exists for test harnesses that need a fixed,
    /// known selector (e.g. a mock-backed client under `"test"`) rather than
    /// going through `build_registry`.
    #[doc(hidden)]
    #[must_use]
    pub fn from_map(clients: HashMap<String, ruoqa::Client>) -> Self {
        Self { clients }
    }

    #[must_use]
    pub fn resolve(&self, selector: &str) -> Option<&ruoqa::Client> {
        self.clients.get(selector)
    }

    /// Resolve `selector` to its canonical `host[:port]` id, for the audit
    /// stream: `selector` is whatever alias the caller used (e.g. `osd`), the
    /// audit record should carry the resolved host instead.
    #[must_use]
    pub fn resolve_id(&self, selector: &str) -> Option<String> {
        self.resolve(selector).map(|c| canonical_id(c.base_url()))
    }

    /// Sorted, for error messages and the `list_servers` tool.
    #[must_use]
    pub fn identifiers(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.clients.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    }
}

/// Why [`build_registry`] could not build a [`ServerRegistry`].
#[derive(Debug)]
pub enum ServerConfigError {
    /// `$OPENQA_API_KEY`/`$OPENQA_API_SECRET` set with 2+ servers configured.
    AmbiguousCredentials,
    /// Two entries (or an entry and an alias) resolved to the same id.
    DuplicateServerId(String),
    /// One entry's `ClientBuilder::build()` failed.
    Client {
        server: String,
        source: ruoqa::Error,
    },
}

impl fmt::Display for ServerConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AmbiguousCredentials => write!(
                f,
                "OPENQA_API_KEY/OPENQA_API_SECRET cannot be set when 2 or more servers are \
                 configured in OPENQA_SERVER; put per-host credentials in client.conf instead"
            ),
            Self::DuplicateServerId(id) => {
                write!(f, "two configured servers both resolve to {id:?}")
            }
            Self::Client { server, source } => {
                write!(
                    f,
                    "failed to build a client for server {server:?}: {source}"
                )
            }
        }
    }
}

impl std::error::Error for ServerConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client { source, .. } => Some(source),
            Self::AmbiguousCredentials | Self::DuplicateServerId(_) => None,
        }
    }
}

/// Build a [`ServerRegistry`] from `env`'s `OPENQA_SERVER` entries.
///
/// # Errors
///
/// Returns [`ServerConfigError::AmbiguousCredentials`] if 2+ servers are
/// configured alongside `$OPENQA_API_KEY`/`$OPENQA_API_SECRET`,
/// [`ServerConfigError::DuplicateServerId`] if two entries (or an entry and
/// an alias) resolve to the same id, or [`ServerConfigError::Client`] if any
/// entry's `ClientBuilder::build()` fails.
pub fn build_registry(env: &EnvConfig) -> std::result::Result<ServerRegistry, ServerConfigError> {
    let entries = split_servers(env.server.as_deref());
    if entries.len() > 1 && (env.api_key_set || env.api_secret_set) {
        return Err(ServerConfigError::AmbiguousCredentials);
    }
    let mut clients = HashMap::new();
    for server in &entries {
        let client =
            config::build_one(env, server).map_err(|source| ServerConfigError::Client {
                server: server.clone(),
                source,
            })?;
        let id = canonical_id(client.base_url());
        insert_unique(&mut clients, id, client.clone())?;
        if let Some(alias) = alias_for(client.base_url()) {
            insert_unique(&mut clients, alias.to_owned(), client)?;
        }
    }
    Ok(ServerRegistry { clients })
}

fn insert_unique(
    map: &mut HashMap<String, ruoqa::Client>,
    id: String,
    client: ruoqa::Client,
) -> std::result::Result<(), ServerConfigError> {
    if map.contains_key(&id) {
        return Err(ServerConfigError::DuplicateServerId(id));
    }
    map.insert(id, client);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn split_servers_unset_or_blank_yields_one_empty_entry() {
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(split_servers(raw), vec![String::new()]);
        }
    }

    #[test]
    fn split_servers_single_entry() {
        assert_eq!(
            split_servers(Some("openqa.suse.de")),
            vec!["openqa.suse.de"]
        );
    }

    #[test]
    fn split_servers_comma_separated_trims_entries() {
        assert_eq!(
            split_servers(Some("openqa.suse.de, openqa.opensuse.org")),
            vec!["openqa.suse.de", "openqa.opensuse.org"]
        );
    }

    #[test]
    fn split_servers_semicolon_separated() {
        assert_eq!(
            split_servers(Some("openqa.suse.de; openqa.opensuse.org")),
            vec!["openqa.suse.de", "openqa.opensuse.org"]
        );
    }

    #[test]
    fn split_servers_drops_stray_empty_entries() {
        assert_eq!(
            split_servers(Some("openqa.suse.de,,openqa.opensuse.org")),
            vec!["openqa.suse.de", "openqa.opensuse.org"]
        );
    }

    #[test]
    fn canonical_id_host_only() {
        let url = Url::parse("https://openqa.suse.de/").unwrap();
        assert_eq!(canonical_id(&url), "openqa.suse.de");
    }

    #[test]
    fn canonical_id_includes_explicit_port() {
        let url = Url::parse("https://openqa.suse.de:8080/").unwrap();
        assert_eq!(canonical_id(&url), "openqa.suse.de:8080");
    }

    #[test]
    fn alias_for_matches_known_hosts_on_default_port() {
        let osd = Url::parse("https://openqa.suse.de/").unwrap();
        assert_eq!(alias_for(&osd), Some("osd"));
        let o3 = Url::parse("https://openqa.opensuse.org/").unwrap();
        assert_eq!(alias_for(&o3), Some("o3"));
    }

    #[test]
    fn alias_for_none_for_unknown_host() {
        let url = Url::parse("https://openqa.example.com/").unwrap();
        assert_eq!(alias_for(&url), None);
    }

    #[test]
    fn alias_for_none_when_explicit_non_default_port() {
        let url = Url::parse("https://openqa.suse.de:8080/").unwrap();
        assert_eq!(alias_for(&url), None);
    }

    fn env(server: &str) -> EnvConfig {
        EnvConfig {
            server: Some(server.to_string()),
            verify: None,
            timeout: None,
            config_paths: Some(vec![]), // never touch the developer's real client.conf
            api_key_set: false,
            api_secret_set: false,
        }
    }

    #[test]
    fn single_entry_no_alias_registers_one_id() {
        let registry = build_registry(&env("openqa.example.com")).unwrap();
        assert_eq!(registry.identifiers(), vec!["openqa.example.com"]);
        assert!(registry.resolve("openqa.example.com").is_some());
    }

    #[test]
    fn two_entries_register_canonical_ids_and_aliases() {
        let registry = build_registry(&env("openqa.suse.de,openqa.opensuse.org")).unwrap();
        let mut ids = registry.identifiers();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["o3", "openqa.opensuse.org", "openqa.suse.de", "osd"]
        );
        assert_eq!(
            registry.resolve("osd").unwrap().base_url().host_str(),
            Some("openqa.suse.de")
        );
        assert_eq!(
            registry.resolve("o3").unwrap().base_url().host_str(),
            Some("openqa.opensuse.org")
        );
    }

    #[test]
    fn exact_duplicate_entries_error() {
        let err = build_registry(&env("openqa.suse.de,openqa.suse.de")).unwrap_err();
        assert!(matches!(
            err,
            ServerConfigError::DuplicateServerId(ref id) if id == "openqa.suse.de"
        ));
    }

    #[test]
    fn textually_different_duplicate_entries_error() {
        let err = build_registry(&env("openqa.suse.de,https://openqa.suse.de/")).unwrap_err();
        assert!(matches!(err, ServerConfigError::DuplicateServerId(_)));
    }

    #[test]
    fn multiple_servers_with_api_key_set_is_ambiguous() {
        let mut e = env("openqa.suse.de,openqa.opensuse.org");
        e.api_key_set = true;
        assert!(matches!(
            build_registry(&e).unwrap_err(),
            ServerConfigError::AmbiguousCredentials
        ));
    }

    #[test]
    fn multiple_servers_with_api_secret_set_is_ambiguous() {
        let mut e = env("openqa.suse.de,openqa.opensuse.org");
        e.api_secret_set = true;
        assert!(matches!(
            build_registry(&e).unwrap_err(),
            ServerConfigError::AmbiguousCredentials
        ));
    }

    #[test]
    fn single_server_with_api_key_set_is_not_an_error() {
        let mut e = env("openqa.suse.de");
        e.api_key_set = true;
        e.api_secret_set = true;
        assert!(build_registry(&e).is_ok());
    }

    #[test]
    fn unresolvable_entry_names_the_offending_server() {
        let mut e = env("openqa.suse.de");
        e.verify = Some("/nonexistent/ca-bundle.pem".to_string());
        let err = build_registry(&e).unwrap_err();
        assert!(matches!(
            err,
            ServerConfigError::Client { ref server, .. } if server == "openqa.suse.de"
        ));
    }
}
