# ruoqa-mcp

<img src="https://raw.githubusercontent.com/mimi1vx/ruoqa-mcp/main/docs/assets/logo.svg"
     align="right" width="130" alt="ruoqa-mcp logo">

[![CI](https://github.com/mimi1vx/ruoqa-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/mimi1vx/ruoqa-mcp/actions/workflows/ci.yml)

An [MCP](https://modelcontextprotocol.io) server that exposes curated,
typed tools over the [openQA](https://open.qa) REST API. It is built on
[rmcp](https://github.com/modelcontextprotocol/rust-sdk) and the
[ruoqa](https://crates.io/crates/ruoqa) openQA client.

Read tools work anonymously; mutating tools require API credentials and
return `403` without them.

## Install

```sh
cargo install ruoqa-mcp
```

or from a checkout:

```sh
cargo install --path .
```

or build and run directly:

```sh
cargo build --release
./target/release/ruoqa-mcp
```

## Configuration

The server reads its configuration from environment variables, falling back
to the openQA client config file for credentials.

### Environment variables

| Variable | Default | Purpose |
| --- | --- | --- |
| `OPENQA_SERVER` | *(unset)* | openQA host (e.g. `openqa.opensuse.org`). Empty falls back to ruoqa's `client.conf` discovery. |
| `OPENQA_API_KEY` | *(unset)* | API key, read by `ruoqa` itself; overrides the config file when set. |
| `OPENQA_API_SECRET` | *(unset)* | API secret, read by `ruoqa` itself; overrides the config file when set. |
| `OPENQA_VERIFY` | `true` | TLS verification: `true`/`false`, or a path to a PEM CA bundle. See the warning below before using `false`. |
| `OPENQA_MCP_TIMEOUT` | `30.0` | Per-request HTTP timeout (seconds) for openQA calls; raise for slow queries like large `latest=1` failed-job lists. `<=0` disables the timeout. Empty/unset uses the default; unparseable, `NaN`, infinite, or out-of-range values abort startup. |
| `OPENQA_MCP_CALL_TIMEOUT` | `300.0` | Whole-tool-call deadline (seconds), independent of `OPENQA_MCP_TIMEOUT`; bounds a slow upstream regardless of which tool is waiting on it. `<=0` disables it. Empty/unset uses the default; unparseable, `NaN`, infinite, or out-of-range values abort startup. |

`OPENQA_API_KEY` and `OPENQA_API_SECRET` must both be set together; setting
only one is a startup error.

`OPENQA_VERIFY` set to a path loads that file as the **only** trusted CA
bundle (it replaces the platform trust store rather than merging with it),
matching httpx's `verify=<path>` semantics. If a deployment relies on merging
a custom CA with the platform roots, this is stricter than before.

> **`OPENQA_VERIFY=false` disables certificate verification for every openQA
> request.** Any certificate is then accepted, so anything able to intercept
> the connection can read and replay the API key, the signed request, and
> every response. Prefer pointing `OPENQA_VERIFY` at your company or
> self-signed CA bundle instead — that keeps verification on while trusting
> your own root. Treat `false` as a last resort for throwaway debugging on a
> network you control, never as a deployment setting.
>
> The server deliberately does not refuse to start in this mode: whether it
> is acceptable is the operator's call. `ruoqa` separately logs a warning
> when credentials are sent over plaintext `http://` to a non-loopback host
> — run with `RUST_LOG=warn` to see it, since the default filter passes only
> errors.

### `~/.env`

Keeping a long-running server's configuration in a shell profile is awkward, so
**every** variable in this README — the ones above, the HTTP ones below, and
`RUST_LOG` — may instead live in `~/.env`, read once at startup:

```sh
umask 077
cat >> ~/.env <<'EOF'
# comments and blank lines are ignored
OPENQA_SERVER=openqa.opensuse.org
OPENQA_MCP_HTTP_TOKEN="a-token-may-be-quoted"
EOF
```

A variable that is already exported always wins over the file, so `~/.env`
supplies defaults rather than overrides. Only this fixed path is read — never a
`.env` in the working directory, because a daemon must not pick up credentials
from wherever it happened to be started. A missing or unreadable `~/.env` is not
an error. The file usually holds secrets, so keep it mode `0600`.

### Config file

If the env credentials are not set, ruoqa falls back to a tiered
`client.conf` lookup: `$OPENQA_CONFIG` first, then
`$XDG_CONFIG_HOME/openqa` (or `~/.config/openqa` if that's unset), then
`/etc/openqa` and `/usr/etc/openqa`. The first tier that has any file (a
`client.conf` and/or `client.conf.d/*.conf` drop-ins) wins outright — a user
config *replaces* `/etc/openqa/client.conf` rather than merging with it. An
empty tier (no files at all) falls through to the next one, so an unset or
empty `$OPENQA_CONFIG` directory does not exclude `/etc`.

Generate a key/secret from the *API keys* page of your openQA instance and
add a section keyed by the host:

```ini
[openqa.opensuse.org]
key = YOUR_API_KEY
secret = YOUR_API_SECRET
```

Without any credentials the server is GET-only (read tools succeed, mutating
tools get `403`).

## Tools

### Read tools

| Tool | Description |
| --- | --- |
| `list_jobs` | List jobs matching the given filters. Pass `summary=true` for a compact triage breakdown. |
| `list_jobs_overview` | List a condensed jobs overview matching the given filters. Pass `summary=true` for a compact triage breakdown. |
| `get_job` | Get full details for a single job. |
| `get_job_comments` | List comments on a job. |
| `list_machines` | List configured worker machines. |
| `list_test_suites` | List configured test suites. |
| `list_products` | List configured products (mediums). |
| `find_jobs_by_setting` | Find jobs whose setting `key` equals `list_value`. |
| `get_job_details` | Get a single job with full test-module/step details. |
| `get_job_status` | Get a lightweight job status (id, state, result, blocked_by_id). |
| `list_job_groups` | List job groups. |
| `get_job_group` | Get a single job group. |
| `list_job_group_jobs` | List jobs belonging to a job group. |
| `get_job_group_build_results` | Get aggregated build results for a job group. |
| `list_parent_groups` | List parent job groups. |
| `get_parent_group` | Get a single parent job group. |
| `list_assets` | List assets known to the system. |
| `get_asset` | Get a single asset by id. |
| `list_workers` | List registered worker instances. |
| `list_bugs` | List tracked bugs referenced by jobs. |
| `search` | Full-text search across jobs, groups, and test modules. |
| `get_scheduled_product` | Get a scheduled product (result of a prior ISO trigger). |
| `get_iso_job_stats` | Get job statistics for scheduled products. |
| `list_group_comments` | List comments on a job group. |
| `list_parent_group_comments` | List comments on a parent job group. |

`list_jobs` and `list_jobs_overview` accept the same optional filters:
`state`, `result`, `distri`, `version`, `build`, `test`, `arch`, `machine`,
`groupid`, `group`, `latest`, `limit`, `ids`. `list_jobs` additionally accepts
`offset` for pagination (the overview endpoint returns only the latest job per
scenario and is not paginated). Unset filters are dropped from the request.
`ids` accepts at most 500 entries (each becomes a repeated `ids=` query
parameter; more would risk a `414` from nginx's default request-line limit).

Both also accept `summary` (default `false`). The default full result can be
very large (~1.5 MB / 150+ jobs for a populated build) and may be truncated by
MCP clients. Pass `summary=true` for a compact per-result breakdown:

```json
{
  "total": 156,
  "by_result": {"passed": 57, "softfailed": 61, "failed": 7, "...": 0},
  "by_state":  {"done": 136, "cancelled": 20},
  "by_arch":   {"x86_64": 78, "aarch64": 39, "s390x": 39},
  "jobs": {"failed": [{"id": 1, "test": "install", "arch": "x86_64"}], "...": []}
}
```

Jobs bucket by `result`; in-progress jobs (result `none`) bucket by `state`
(e.g. `running`, `scheduled`). To work with the full data instead, save it to
a temporary file and process it with `jq`, e.g.
`jq '.jobs[] | select(.result=="failed")'`.

### Mutating tools (require credentials)

| Tool | Description |
| --- | --- |
| `restart_jobs` | Restart the given jobs in one bulk request. |
| `cancel_job` | Cancel a running or scheduled job. |
| `add_job_comment` | Add a comment to a job. |
| `trigger_isos` | Trigger ISO test scheduling for a product. |
| `delete_job` | Delete a job. |
| `duplicate_job` | Duplicate (clone) a job. |
| `set_job_priority` | Set the priority of a job. |
| `cancel_jobs` | Cancel jobs matching the given filters; at least one filter is required. |
| `add_group_comment` | Add a comment to a job group. |
| `add_parent_group_comment` | Add a comment to a parent job group. |
| `update_job_comment` | Update an existing job comment. |
| `delete_job_comment` | Delete a job comment. |
| `create_bug` | Create a tracked bug reference. |
| `cancel_scheduled_product` | Cancel a scheduled product / ISO by name. |

Mutating tools carry `destructiveHint`/`readOnlyHint` MCP annotations so
clients can gate them behind confirmation. To drop them entirely, start the
server in read-only mode with `--readonly` (or `OPENQA_READONLY=true`): the
mutating tools are never registered, so clients see only the read tools.

`restart_jobs` sends a single bulk request to openQA regardless of how many
ids are given, so `job_ids` is capped at 1-500 entries. Partial success (e.g.
one id missing its assets) is reported by openQA itself in the response's
`result`/`errors`/`warnings` fields rather than as an MCP error. `trigger_isos`'s
`extra` map is capped at 100 entries (each becomes a scheduled-product/job-settings
row); individual values stay unbounded to allow an inline
`SCENARIO_DEFINITIONS_YAML` document. `extra` keys may not collide,
case-insensitively, with `distri`/`version`/`flavor`/`arch` or with each other.

### Errors

A tool call fails one of two ways:

- **The tool ran and openQA (or the network) said no.** The MCP call still
  succeeds (`isError: true`), with a caller-visible payload:
  `{"error": {"kind", "status"?, "message", "body"?}}`. `body` is openQA's
  response body, truncated to 512 bytes. `kind` is one of: `unauthorized`,
  `forbidden`, `not_found`, `rate_limited`, `bad_request`, `server_error`,
  `connection`, `timeout`, `response_too_large`, `invalid_response`.
- **The server itself is misconfigured or refused to route the request**
  (bad `client.conf`, TLS setup failure, incomplete credentials, a
  cross-origin or outside-base-URL request). This is a JSON-RPC
  `internal_error`, which most MCP clients render opaquely.

`OPENQA_MCP_CALL_TIMEOUT` firing is reported as a `kind: "timeout"` tool
error, not a protocol error: the tool call may have reached openQA, so an
in-flight write may already have been applied.

## Running

### stdio (default)

Most local MCP clients spawn the server over stdio. Wire it in with:

```sh
ruoqa-mcp
```

Example MCP client configuration:

```json
{
  "mcpServers": {
    "openqa": {
      "command": "ruoqa-mcp",
      "env": {
        "OPENQA_SERVER": "openqa.opensuse.org"
      }
    }
  }
}
```

### HTTP (optional)

For remote or shared deployments, run over HTTP with `--http`. HTTP callers
authenticate with a bearer token, so generate one first:

```sh
export OPENQA_MCP_HTTP_TOKEN=$(openssl rand -hex 32)
ruoqa-mcp --http --server 127.0.0.1 --port 8000
```

The MCP endpoint is mounted at `/mcp`; clients send
`Authorization: Bearer <token>` with every request.

#### Authentication and scopes

Unlike stdio — where the client already owns the process — HTTP exposes the
server's single openQA credential to anyone who can reach the port, so
authentication is mandatory and deny-by-default. Two tokens define two scopes:

| Token | Scope | Tools |
| --- | --- | --- |
| `OPENQA_MCP_HTTP_TOKEN` | write | all 39 read + mutating tools |
| `OPENQA_MCP_HTTP_READ_TOKEN` | read | the 25 read tools only |

Either may be set alone. A read-scope caller sees only the read tools in
`tools/list` and gets an MCP error — with no openQA request made — if it calls a
mutating tool anyway; the split is derived from each tool's `readOnlyHint`
annotation, so it cannot drift from the tool registry. Because the advertised
tool set depends on the credential, a client that caches `tools/list` across
tokens will show a stale list.

Tokens are never accepted as command-line flags: argv is world-readable via
`ps`. Like every other variable, they may come from [`~/.env`](#env) instead of
the environment:

```sh
umask 077
printf 'OPENQA_MCP_HTTP_TOKEN=%s\n' "$(openssl rand -hex 32)" >> ~/.env
```

The server refuses to start (before binding the port) when:

- `--http` is given with no token and no `--insecure-no-auth`;
- `--insecure-no-auth` is combined with a token;
- a token is shorter than 32 characters, or contains anything but printable
  non-space ASCII;
- the read token equals the write token;
- the `--allowed-host` flag is given without `--http` (the same value from the
  environment or `~/.env` is simply ignored by a stdio run).

Tokens set while running over stdio are ignored.

> **The transport is plaintext HTTP.** A bearer token sent over it is readable
> by anything on the path, so never expose the port beyond a trusted network
> without terminating TLS in front of it (reverse proxy, service mesh, or an
> SSH tunnel).
>
> Static bearer tokens are not MCP's OAuth 2.1 authorization flow. Clients that
> only implement the spec's `401` → resource-metadata → OAuth dance will not
> authenticate; use a client that lets you set a header.

#### `Host` allowlist

To block DNS rebinding, requests are accepted only for a known authority:
`localhost`, `127.0.0.1` and `::1` always, plus every `--allowed-host` value.
Anything else gets `403`. Name the public authority explicitly when the server
is not reached over loopback — the bind address is deliberately not treated as
an identity, so binding `0.0.0.0` allows nothing extra:

```sh
ruoqa-mcp --http --server 0.0.0.0 --allowed-host mcp.example.com:8000
```

| Flag | Default | Purpose |
| --- | --- | --- |
| `--http` | off | Serve over HTTP instead of stdio. |
| `--stdio` | on | Serve over stdio; overrides `OPENQA_MCP_TRANSPORT=http`. |
| `--server` | `127.0.0.1` | HTTP bind host. |
| `--port` | `8000` | HTTP bind port. |
| `--allowed-host` | *(none)* | Extra authority accepted in the `Host` header; repeatable. |
| `--insecure-no-auth` | off | Serve HTTP with no authentication at all; prints a warning on start. |
| `--readonly` | off | Unregister all mutating tools (read-only server). |
| `--version` | — | Print version and exit. |

Flags override the environment, which supplies the defaults (and which
`~/.env` in turn supplies defaults for):

| Variable | Default | Purpose |
| --- | --- | --- |
| `OPENQA_MCP_TRANSPORT` | `stdio` | Set to `http` to serve over HTTP. |
| `OPENQA_MCP_HOST` | `127.0.0.1` | Default HTTP bind host. |
| `OPENQA_MCP_PORT` | `8000` | Default HTTP bind port. |
| `OPENQA_MCP_HTTP_TOKEN` | *(unset)* | Bearer token granting the write scope. |
| `OPENQA_MCP_HTTP_READ_TOKEN` | *(unset)* | Bearer token granting the read scope. |
| `OPENQA_MCP_ALLOWED_HOSTS` | *(unset)* | Comma-separated default for `--allowed-host`. |
| `OPENQA_READONLY` | `false` | Set truthy (`1`/`true`/`yes`/`on`) to disable mutating tools. |
| `OPENQA_MCP_HEARTBEAT_INTERVAL` | `15.0` | Seconds between progress "heartbeat" pings sent while a tool waits on a slow openQA call, so MCP clients see liveness instead of timing out. Set `<=0` to disable. Pings are a no-op unless the client sent a `progressToken`. Empty/unset uses the default; unparseable, `NaN`, infinite, or out-of-range values abort startup. |

`--readonly` and the read token are different levers: `--readonly` is
process-wide and unregisters the mutating tools for every caller, including
stdio; the read token restricts one HTTP principal while others keep write
access.

Press `Ctrl-C` to stop; the server shuts down cleanly on both transports.

## Development

```sh
cargo test                                  # run the test suite
cargo clippy --all-targets -- -D warnings   # lint
cargo fmt --check                           # format check
```
