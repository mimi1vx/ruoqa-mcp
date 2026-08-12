//! `~/.env`: environment defaults for a long-running server.
//!
//! A daemon's configuration — and especially its secrets — is awkward to keep
//! in a shell profile, so every variable this server reads may instead live in
//! `~/.env`. Only that fixed path is consulted, never a `.env` in the working
//! directory: picking up a credential from wherever the process happened to be
//! started is a footgun, not a feature.

use std::path::PathBuf;

const FILE_NAME: &str = ".env";

/// The pairs in `~/.env`, or nothing when it is absent or unreadable. The file
/// is a convenience, so neither case is an error: a variable that never arrives
/// fails later, where its absence actually means something.
#[must_use]
pub fn read_home_env() -> Vec<(String, String)> {
    let Some(path) = path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => parse(&contents),
        Err(e) => {
            tracing::debug!("not reading {}: {e}", path.display());
            Vec::new()
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(FILE_NAME))
}

/// `KEY=value` per line. Blank lines, `#` comments and lines without a `=` are
/// skipped; the key and value are trimmed and one layer of matching quotes is
/// stripped from the value.
#[must_use]
pub fn parse(contents: &str) -> Vec<(String, String)> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            let value = value.trim();
            let unquoted = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            (key.trim().to_string(), unquoted.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_quotes_and_padding() {
        let parsed = parse(
            "# a comment\n\nOPENQA_SERVER=openqa.example.com\n  \
             OPENQA_MCP_HTTP_TOKEN = \"abc\" \nSINGLE='def'\nEQUALS=a=b\nEMPTY=\n\
             not a variable\n",
        );
        assert_eq!(
            parsed,
            [
                ("OPENQA_SERVER", "openqa.example.com"),
                ("OPENQA_MCP_HTTP_TOKEN", "abc"),
                ("SINGLE", "def"),
                ("EQUALS", "a=b"),
                ("EMPTY", ""),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
        );
    }

    #[test]
    fn an_empty_file_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("# only a comment\n").is_empty());
    }
}
