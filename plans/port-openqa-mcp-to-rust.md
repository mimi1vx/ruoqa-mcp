# Port `mimi1vx/openqa-mcp` (Python) → `ruoqa-mcp` (Rust, rmcp + ruoqa)

Status: plan (not yet executed). Build mode required to apply.

## Current state

`ruoqa-mcp` is an empty cargo skeleton: `src/main.rs` is `hello world`, `Cargo.toml`
declares only `rmcp = "3.1.0"` and `ruoqa = "0.1.2"` (both already resolved in
`Cargo.lock`), edition 2024. Nothing else exists.

The Python original is ~825 LOC in three files:

| File | LOC | Contents |
| --- | --- | --- |
| `server.py` | 607 | 40 `@mcp.tool` fns (25 read, 15 tagged `mutating`), `_drop_none`, `_api`, `_summarize_jobs`, `_with_heartbeat`, `disable_mutating_tools` |
| `client.py` | 126 | env → `AsyncOpenQAClient` (`OPENQA_SERVER`/`API_KEY`/`API_SECRET`/`VERIFY`/`MCP_TIMEOUT`), fastmcp lifespan |
| `__main__.py` | 91 | argparse CLI: `--http`/`--stdio`/`--server`/`--port`/`--readonly` |

Every tool is a thin wrapper over `AsyncOpenQAClient.openqa_request(method, path,
params=…, data=…)`.

## Decisions taken (from the clarification round)

1. Full 1:1 port of all 40 tools in one pass.
2. Both transports: stdio + streamable HTTP, with CLI parity.
3. All env-var names preserved; pragmatic mapping onto ruoqa's different config model.
4. Keep the heartbeat; keep `--readonly`.
5. `clap` for the CLI; port the pytest suite onto `wiremock`.

## Impedance mismatches (the actual work)

These are the four places where `ruoqa` is *not* a drop-in for `openqa-async`.
Everything else is mechanical.

**A. No `params=`.** `openqa_request` took a params dict; httpx encoded it, expanding
list values into repeated keys (`ids=1&ids=2`). `ruoqa::Client::request(method, path,
body)` takes the query string **baked into `path`**. The port must build query strings
itself. This affects 6 tools (`list_jobs`, `list_jobs_overview`,
`find_jobs_by_setting`, `get_job_status`, `get_job_group_build_results`, `search`,
`cancel_jobs`).

**B. `data=` was form-encoded, not JSON.** Confirmed in `openqa_async/_base.py:163-169`
— `data` goes to `httpx.build_request(data=…)`, i.e.
`application/x-www-form-urlencoded`. So the 9 mutating tools that pass `data=` map to
`Client::request_form(method, path, &[(&str, &str)])`, **not** `Client::request` with a
JSON body. Getting this wrong makes every write silently 400/ignore its parameters.
`restart_jobs_bulk` relies on repeated `jobs=` keys, which `request_form`'s slice-of-pairs
signature handles naturally.

**C. `request_form` values are `&str`.** Numeric args (`prio`, `groupid`, job ids) need
owned `String`s kept alive across the call. Needs a tiny owning builder.

**D. 204 → `Value::Null`.** ruoqa returns `Value::Null` for `204 No Content`; Python
normalized the raw httpx response to `{}`. `delete_job` and `delete_job_comment` must
map `Null → {}` to preserve output shape.

## Target module layout

```
src/
  main.rs        CLI (clap) + tokio runtime + transport selection
  lib.rs         module wiring, re-exports for integration tests
  config.rs      env → ruoqa::ClientBuilder            (port of client.py)
  query.rs       Query builder: drop-none + repeated keys  (fills gap A)
  form.rs        owning Form builder for request_form       (fills gap C)
  summary.rs     summarize_jobs                             (port of _summarize_jobs)
  heartbeat.rs   with_heartbeat via Peer::notify_progress
  server.rs      OpenQaServer struct + ServerHandler impl
  tools/read.rs  25 read tools   (#[tool_router(router = read_tool_router)])
  tools/write.rs 15 write tools  (#[tool_router(router = write_tool_router)])
tests/
  tools.rs       wiremock-backed port of tests/test_tools.py
  config.rs      env-parsing unit tests (port of test_client.py)
  cli.rs         CLI parsing tests (port of test_cli.py)
```

`lib.rs` + a thin `main.rs` (rather than one binary crate) so integration tests can
construct `OpenQaServer` directly.

## Plan

1. **[small] `Cargo.toml`: dependencies, features, metadata.**
   Add `license = "GPL-3.0-or-later"` (ruoqa is GPL-3.0-or-later, so the binary is too),
   `description`, `[[bin]] name = "ruoqa-mcp"`.
   ```toml
   rmcp = { version = "3.1", features = [
       "server", "macros", "schemars",
       "transport-io", "transport-streamable-http-server",
   ] }
   ruoqa = "0.1.2"
   tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal", "time"] }
   serde = { version = "1", features = ["derive"] }
   serde_json = "1"
   schemars = "1"
   clap = { version = "4", features = ["derive"] }
   form_urlencoded = "1"
   reqwest = { version = "0.13", default-features = false }  # only for Certificate
   axum = "0.8"                                              # host for StreamableHttpService
   tracing = "0.1"
   tracing-subscriber = { version = "0.3", features = ["env-filter"] }
   anyhow = "1"
   [dev-dependencies]
   wiremock = "0.6"
   ```
   `reqwest` is a direct dep only because `TlsMode::CustomCa { certs: Vec<reqwest::Certificate> }`
   needs the type; cargo unifies features with ruoqa's copy, so TLS comes from there.
   — verify: `cargo build` succeeds on the untouched `main.rs`; `cargo tree -d` shows no
   duplicate `reqwest`/`rmcp`.

2. **[small] `src/query.rs` — query-string builder (gap A).**
   `Query::new()` with `push(k, Option<impl Display>)` (drops `None`, mirroring
   `_drop_none`) and `push_all(k, Option<&[i64]>)` (repeated keys, mirroring httpx list
   expansion). `finish(path) -> String` returns `"/api/v1/jobs"` when empty, else
   `"/api/v1/jobs?…"`, percent-encoded via `form_urlencoded::Serializer`.
   Also `fn api(path: &str) -> String` → `format!("/api/v1/{path}")` (port of `_api`;
   note the **leading slash** — ruoqa joins against the base URL, unlike openqa-async
   which normalized it).
   — verify: unit tests — `None` dropped; `ids=[1,2]` → `ids=1&ids=2`; `build` with a
   space/`+`/`&` round-trips; empty query yields no `?`.

3. **[trivial] `src/form.rs` — owning form builder (gap C).**
   `Form(Vec<(String, String)>)` with `push`, `push_opt`, `push_all`, and
   `pairs(&self) -> Vec<(&str, &str)>` for `Client::request_form`.
   — verify: unit test that `pairs()` borrows correctly and preserves insertion order
   (needed for stable HMAC-signed bodies in test assertions).

4. **[small] `src/config.rs` — env → `ruoqa::ClientBuilder` (port of `client.py`).**
   Pure functions so they're testable without env mutation:
   - `parse_verify(Option<&str>) -> TlsMode`: `0/false/no` → `TlsMode::danger_accept_invalid_certs()`;
     `1/true/yes` or unset/empty → `PlatformVerifier`; anything else → read the file and
     `CustomCa { certs: Certificate::from_pem_bundle(..)?, replace_roots: true }`
     (`true` because httpx `verify=<path>` trusts *only* that bundle).
   - `parse_timeout(Option<&str>) -> Timeouts`: default `total = 30s`; `<= 0` disables
     the total timeout; unparseable falls back to the default. Leave ruoqa's
     `connect`/`read`/`pool_idle` at their defaults.
   - `build_client(env) -> ruoqa::Result<Client>`: `.server(OPENQA_SERVER)`,
     `.tls(..)`, `.timeouts(..)`, and `.api_key()/.api_secret()` **only when both**
     `OPENQA_API_KEY` and `OPENQA_API_SECRET` are set (preserving the "never
     half-configured" rule). No credentials → ruoqa's `client.conf` discovery applies,
     matching the Python fallback.
   — verify: unit tests for each mapping; a test asserting a lone `OPENQA_API_KEY` is
   ignored.

5. **[small] `src/summary.rs` — `summarize_jobs` (port of `_summarize_jobs`).**
   Straight transcription, incl. the bucketing rule: key = `result` when truthy and not
   `"none"`, else `state`, else `"unknown"`; per-job `{id, test, arch}` with `arch` from
   `settings.ARCH`. Use `serde_json::Map` (insertion-ordered? **no** — `serde_json`'s
   default is `BTreeMap`) — enable `serde_json/preserve_order` **only if** test
   assertions need Python's insertion order; otherwise assert on parsed values, not
   serialized strings.
   — verify: port the Python summary tests verbatim; assert counts and bucket keys.

6. **[medium] `src/heartbeat.rs` — progress pings.**
   `async fn with_heartbeat<F: Future>(peer: &Peer<RoleServer>, token: Option<ProgressToken>, fut: F) -> F::Output`.
   Reads `OPENQA_MCP_HEARTBEAT_INTERVAL` per call (default `15.0`, `<= 0` disables,
   malformed → default) so tests can tweak it. If the interval is `<= 0` or there is no
   progress token, just `.await` the future.
   Otherwise `tokio::select!` the future against a ticker loop that increments a counter
   and calls `peer.notify_progress(ProgressNotificationParam { progress_token, progress,
   total: None, message: Some("working…") })`, swallowing send errors. `select!` drops
   the losing branch, so no task is leaked (simpler than Python's `create_task` +
   `finally: cancel`).
   The progress token comes from the tool's `RequestContext<RoleServer>` meta; if rmcp
   3.1 doesn't surface it there, fall back to always-ticking and letting
   `notify_progress` no-op.
   — verify: unit test with a 10 ms interval and a 50 ms sleep, asserting ≥3 pings on a
   mock peer; and a test that interval `0` produces none.

7. **[medium] `src/server.rs` — the `OpenQaServer` handler and the request funnel.**
   ```rust
   #[derive(Clone)]
   pub struct OpenQaServer { client: ruoqa::Client, tool_router: ToolRouter<Self> }
   ```
   - `fn new(client, readonly: bool)` merges `Self::read_tool_router()` and, unless
     `readonly`, `Self::write_tool_router()`.
   - `impl ServerHandler` with `get_info()` carrying the same `instructions` string as
     `FastMCP("openQA", instructions=…)`, `#[tool_handler]`.
   - Two private funnels replacing `_request`:
     `get_json(&self, ctx, path) -> Result<CallToolResult, ErrorData>` (uses
     `Client::request(GET, path, None)`) and
     `post_form(&self, ctx, path, form) -> …` (uses `Client::request_form`), both wrapped
     in `with_heartbeat`.
   - `fn ok(value: Value) -> CallToolResult`: `Value::Null → json!({})` (gap D), then
     `CallToolResult::success(vec![Content::json(value)?])`.
   - `fn err(e: ruoqa::Error) -> ErrorData`: `ErrorData::internal_error` with the
     display string. `ruoqa::Error` redacts credentials in `Display`, and
     `Error::Request` carries the status, so an unauthenticated write still surfaces a
     legible `403`.
   — verify: `cargo build`; a unit test asserting `ok(Value::Null)` yields `{}`.

   > **Deviation from the answered choice, flagged:** `--readonly` is implemented by
   > *not merging* the write router rather than by `ToolRouter::disable_route`. Reason:
   > `disable_route` needs a hand-maintained `&[&str]` of the 15 mutating tool names
   > that silently rots when a tool is renamed; two routers give the same result with a
   > compile-time guarantee and no name list. `disable_route`/`remove_route` remain
   > available if runtime toggling is wanted later. Say the word to switch.

8. **[large] `src/tools/read.rs` — the 25 read tools.**
   One `#[tool_router(router = read_tool_router, vis = pub)] impl OpenQaServer` block.
   Each tool: `#[tool(description = "…")]` carrying the Python one-line docstring
   verbatim, plus `annotations(read_only_hint = true)`.
   Parameter structs derive `Deserialize + JsonSchema` with `Option<T>` +
   `#[serde(default)]` and `#[schemars(description = …)]`.
   - `ListJobsArgs` (13 filters + `offset` + `summary`) and `ListJobsOverviewArgs`
     (13 filters + `summary`, no `offset`) are written out in full — **no
     `#[serde(flatten)]`**, because flatten emits `allOf` schemas that several MCP
     clients mis-render.
   - `summary = true` post-processes via `summarize_jobs`, keeping the two different
     shapes the Python has: `list_jobs` only summarizes when the body is an object;
     `list_jobs_overview` also accepts a bare array.
   - Endpoint paths transcribed exactly, including the `experimental/` ones
     (`experimental/jobs/{id}/status`, `experimental/search`).
   — verify: a test asserting `read_tool_router().list_all().len() == 25` and that the
   name set matches the README table exactly.

9. **[large] `src/tools/write.rs` — the 15 mutating tools.**
   One `#[tool_router(router = write_tool_router, vis = pub)]` block, each with
   `annotations(read_only_hint = false, destructive_hint = true)` (the MCP-native
   replacement for the `mutating` tag; `add_*_comment` and `trigger_isos` get
   `destructive_hint = false`, `idempotent_hint = false`).
   Routing per Python's kwarg:
   - `data=` → `Client::request_form` (POST/PUT): `add_job_comment`, `trigger_isos`,
     `duplicate_job`, `set_job_priority`, `restart_jobs_bulk`, `add_group_comment`,
     `add_parent_group_comment`, `update_job_comment`, `create_bug`.
   - `params=` → query string + `Client::request` with `None` body: `cancel_jobs`.
   - no body → `Client::request` with `None`: `restart_jobs` (loop, collecting a JSON
     array), `cancel_job`, `delete_job`, `delete_job_comment`,
     `cancel_scheduled_product`.
   - `restart_jobs_bulk` pushes one `jobs=<id>` pair per id.
   - `trigger_isos` merges `extra: Option<HashMap<String, String>>` over the four
     required `DISTRI`/`VERSION`/`FLAVOR`/`ARCH` keys, same as the Python `dict.update`.
   - `delete_job` / `delete_job_comment` rely on the `Null → {}` normalization.
   — verify: `write_tool_router().list_all().len() == 15`, names match the README table.

10. **[small] `src/lib.rs`.** Declare the modules, re-export `OpenQaServer`,
    `build_client`, and the arg structs for tests.
    — verify: `cargo build --lib`.

11. **[medium] `src/main.rs` — CLI and transports (port of `__main__.py`).**
    clap derive `Cli`:
    - `--http` / `--stdio`, `conflicts_with` each other;
    - `--server <HOST>` → field `host`, default from `OPENQA_MCP_HOST` else `127.0.0.1`
      (keep the confusing name: `--server` is the *MCP bind host*, `OPENQA_SERVER` is
      the *openQA host*; parity beats clarity here);
    - `--port` default from `OPENQA_MCP_PORT` else `8000`;
    - `--readonly`, OR-ed with a manual `env_flag("OPENQA_READONLY")` that accepts
      `1/true/yes/on`. **Do not** use clap's `env` attribute for this flag — clap treats
      mere presence of the variable as true, whereas Python required a truthy value.

    Transport selection, verbatim from Python: explicit `--stdio` wins; otherwise
    `--http` or `OPENQA_MCP_TRANSPORT == "http"` selects HTTP.

    - **Logging must go to stderr** (`tracing_subscriber::fmt().with_writer(std::io::stderr)`).
      Anything on stdout corrupts the stdio JSON-RPC stream. This is the single most
      likely way to ship a broken binary.
    - stdio: `OpenQaServer::new(..).serve(rmcp::transport::stdio()).await?.waiting().await?`.
    - HTTP: `StreamableHttpService::new(move || Ok(server.clone()), Default::default())`
      mounted on an `axum::Router` at `/mcp`, served with
      `.with_graceful_shutdown(tokio::signal::ctrl_c())`.
    - Ctrl-C exits 0 with no traceback/panic, matching the Python `except KeyboardInterrupt: pass`.
    — verify: `ruoqa-mcp --help` matches the README flag table; a stdio smoke test (below).

12. **[medium] `tests/tools.rs` — wiremock port of `test_tools.py`.**
    Helper `async fn fixture() -> (MockServer, OpenQaServer)` building the client with
    `ClientBuilder::new().server(mock.uri()).config_paths(vec![])`.
    **`config_paths(vec![])` is mandatory** — without it the suite reads the developer's
    real `~/.config/openqa/client.conf` and both leaks credentials into test traffic and
    makes results machine-dependent.
    Cover, at minimum:
    - `list_jobs` drops `None` filters and expands `ids`;
    - `list_jobs`/`list_jobs_overview` with `summary = true` produce the documented shape;
    - one form-encoded write asserting the request body is
      `application/x-www-form-urlencoded` with the expected pairs (guards gap B);
    - `restart_jobs_bulk` emits repeated `jobs=`;
    - `delete_job` on a `204` returns `{}` (guards gap D);
    - a `403` becomes an `ErrorData` whose message mentions 403 and contains no secret.
    — verify: `cargo test`.

13. **[small] `tests/cli.rs` + `tests/config.rs`.** Port `test_cli.py`
    (flag/env precedence, `--readonly` truthiness, `--stdio` beats
    `OPENQA_MCP_TRANSPORT=http`) and `test_client.py` (verify/timeout/credential
    parsing). Set env inside a serialized guard or, preferably, pass an explicit
    env map into the pure parsers so the tests need no `std::env::set_var`.
    — verify: `cargo test`.

14. **[small] `README.md`.** Rewrite for Rust: `cargo install` / `cargo run`, the same
    two env tables and two tool tables, the ruoqa `client.conf` semantics (note the
    `$OPENQA_CONFIG` override, which openqa-async did not have), and MCP client config
    using the binary instead of `uv run`.
    — verify: every flag and env var in the README exists in `--help` / `config.rs`.

## Files

- Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs`, `.gitignore` (add `/plans`? no — keep tracked)
- Create: `src/lib.rs`, `src/config.rs`, `src/query.rs`, `src/form.rs`, `src/summary.rs`,
  `src/heartbeat.rs`, `src/server.rs`, `src/tools/mod.rs`, `src/tools/read.rs`,
  `src/tools/write.rs`, `tests/tools.rs`, `tests/cli.rs`, `tests/config.rs`,
  `README.md`, `COPYING` (GPL-3.0 text, matching ruoqa)
- Delete: none

Complexity: **medium-large**. Steps 8 and 9 are ~700 lines of mechanical transcription;
everything else is small. Steps 2/3/9 carry the real risk.

## Risks

1. **Form vs JSON on writes (gap B) is silent.** A JSON body to
   `POST /api/v1/jobs/{id}/comments` may return 200 while ignoring the comment text.
   Mitigation: the step-12 test asserts on the recorded `Content-Type` and body, not just
   the status.
2. **`Content::json` vs `Json<Value>` output shape.** rmcp's `Json<T>` wrapper emits
   `structuredContent` plus an output schema; MCP requires `structuredContent` to be an
   object, and several read tools return a top-level array. Plan uses
   `Content::json(value)` (JSON-in-text), which matches FastMCP's default and avoids the
   problem. If structured output is wanted later, wrap arrays as `{"items": […]}` — but
   that changes the response shape, so it is out of scope here.
3. **Progress-token plumbing may not match the assumed rmcp 3.1 API.** rmcp is only 48%
   documented and `RequestContext` meta access is unverified. Fallback in step 6 keeps
   the build green; worst case the heartbeat degrades to a no-op, which is exactly what
   the Python does without a `progressToken`.
4. **Tests reading the real `client.conf`.** Called out in step 12; `config_paths(vec![])`
   everywhere.
5. **`OPENQA_VERIFY=<path>` semantics.** `replace_roots: true` is the faithful httpx
   mapping but is stricter than `false`; if an internal deployment relied on merging with
   platform roots it will now fail closed. Documented in the README.

## Alternatives considered

- **Hand-rolling an openQA client instead of using `ruoqa`** — rejected: ruoqa already
  does HMAC-SHA1 signing, `client.conf` discovery, YAML fallback, retries, and
  same-origin redirect pinning. Re-implementing that is the bulk of the security surface.
- **`#[serde(flatten)]` for the shared 13 job filters** — rejected: saves ~40 lines but
  produces `allOf`-shaped JSON Schemas that some MCP clients render badly. Duplication is
  cheaper than a client-compat bug.
- **`disable_route` for `--readonly`** — see the flagged deviation in step 7.
- **Single-binary crate (no `lib.rs`)** — rejected: integration tests could then only
  drive the server through a spawned process, which is far slower and hides assertion
  detail.
- **Adding a typed openQA response model layer** — rejected: out of scope, and ruoqa
  explicitly declines to provide one. Tools stay `serde_json::Value` pass-throughs like
  the Python.

## Success criteria

- [ ] `cargo build --release` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `cargo test` green, including the wiremock suite
- [ ] `read_tool_router().list_all()` yields exactly the 25 read-tool names from the README
- [ ] `write_tool_router().list_all()` yields exactly the 15 mutating-tool names
- [ ] `OpenQaServer::new(client, true)` exposes 25 tools; `false` exposes 40
- [ ] stdio smoke: piping an `initialize` + `tools/list` JSON-RPC pair into
      `./target/release/ruoqa-mcp` returns 40 tools and writes nothing else to stdout
- [ ] HTTP smoke: `ruoqa-mcp --http --port 8000` answers a `tools/list` at `/mcp`
- [ ] live read against `OPENQA_SERVER=openqa.opensuse.org`:
      `list_jobs` with `limit=5` returns 5 jobs; `summary=true` returns the documented
      `{total, by_result, by_state, by_arch, jobs}` shape
- [ ] live write without credentials returns an error mentioning `403`, with no secret
      in the message
- [ ] `ruoqa-mcp --help` matches the README flag table
- [ ] Ctrl-C on both transports exits 0 with no panic or backtrace
