# Observability

`ruoqa-mcp` can emit two independent streams: a JSONL audit log of every tool
call, and OpenTelemetry logs/traces/metrics over OTLP/HTTP. Both are off
unless configured.

## Off by default

With no `OTEL_*` endpoint variable set and no `--audit-config` /
`OPENQA_MCP_AUDIT_CONFIG`, the server builds neither subsystem: no background
task, no HTTP client for export, no file handle, no socket. Turning either on
costs one flag or one environment variable; turning neither on costs nothing
at runtime.

## The audit stream

Enabled by `--audit-config` / `OPENQA_MCP_AUDIT_CONFIG` naming a TOML file
(see [Configuration](#configuration) below). Once enabled, the server appends
one JSON object per line to the configured file for every session open, tool
call, and shutdown.

### Record schema

Fields appear in this order — the on-disk schema — and every optional field
is omitted (not `null`) when it does not apply:

| Field | Type | Present on | Meaning |
| --- | --- | --- | --- |
| `v` | integer | always | Schema version, `1`. |
| `ts` | string | always | RFC 3339 timestamp, millisecond precision, UTC. |
| `seq` | integer | always | Per-process, monotonically increasing sequence number, starting at `1`. |
| `session` | string | always | `Mcp-Session-Id` for an HTTP call; a per-process id (`p<pid>-<start_ms>`) for stdio and for `initialize`, which has no session id yet. |
| `transport` | string | always | `stdio` or `http`. |
| `scope` | string | always | `read`, `write`, or `none` for a session-level event. |
| `event` | string | always | `session_open`, `tool_call`, `audit_gap`, or `shutdown`. |
| `tool` | string | `tool_call` | The tool name. |
| `server` | string | `tool_call` | The resolved server id the call targeted. |
| `args` | object | `tool_call` | Captured arguments; see below. Results are never recorded. |
| `outcome` | string or object | `tool_call` | `"ok"`, `{"tool_error":{"kind":...,"status":...}}`, or `{"protocol_error":{"code":...}}`. |
| `duration_ms` | integer | `tool_call` | Wall-clock duration of the call. |
| `trace` | string | `tool_call` (sampled) | Lowercase hex trace id, shared with the exported `mcp.tool/<name>` span. |
| `span` | string | `tool_call` (sampled) | Lowercase hex span id, same span. |
| `count` | integer | `audit_gap` | How many appends failed during the outage. |
| `since` | string | `audit_gap` | Timestamp of the first failed append. |
| `refused` | integer | `audit_gap` | How many tool calls the fail-closed gate refused during the outage. |

`trace`/`span` and the `audit_gap` fields are additive to schema version `1`;
a consumer that ignores unknown fields already handles them correctly.

### Argument capture

Only an allow-listed set of argument names is recorded verbatim (`server`,
`job_id`, `job_ids`, `ids`, `group_id`, `groupid`, `parent_group_id`,
`comment_id`, `scheduled_product_id`, `asset_id`, `bugid`, `build`, `distri`,
`version`, `flavor`, `arch`, `machine`, `test`, `result`, `state`, `group`,
`text`, `title`, `prio`, `force`, `dup_type_auto`, `filename`, `member`,
`tool`, `tier`) — deny-by-default, so a tool gaining a new argument is
invisible in the audit stream until it is added to this list. An allow-listed
array longer than 20 elements, and every other argument regardless of name,
is summarized as `{"_len": n}` (string length, array length, or object key
count). Tool *results* are never recorded, only the call.

### File handling

The audit directory is created (if needed) mode `0700`; the audit file is
opened or created mode `0600`. A path that is a symlink is refused rather
than followed. The file rotates when the next append would cross
`rotate_max_bytes`: the current file becomes `<path>.1`, existing numbered
files shift up, and the file beyond `rotate_keep` is deleted. `rotate_max_bytes
= 0` disables rotation entirely. A single record larger than
`rotate_max_bytes` is still written whole into an empty file rather than
rotated forever.

## Configuration

Set `--audit-config <path>` or `OPENQA_MCP_AUDIT_CONFIG=<path>` to a TOML
file. The file is parsed strictly: an unrecognized key is a startup error.

| Key | Default | Meaning |
| --- | --- | --- |
| `path` | *(required)* | Where to write the JSONL file. `"none"` disables the file sink — useful when the audit stream is exported over OTLP only (see [Fail modes](#fail-modes)). |
| `fsync` | `false` | Call `fsync` after every append. |
| `fail_mode` | *(unset)* | `"open"`, `"closed_writes"`, or `"closed_all"`. Unset resolves per transport — see below. |
| `rotate_max_bytes` | `67108864` (64 MiB) | Rotate when the next append would cross this size. `0` disables rotation. |
| `rotate_keep` | `8` | How many rotated files to retain, clamped to `1..=10000`. |

See [`docs/examples/audit.toml`](examples/audit.toml) for a fully-commented
example, including the `path = "none"` collector-only variant.

## Fail modes

`fail_mode` governs whether a tool call is refused while the audit stream
cannot persist:

| Mode | Behaviour while persistence is failing |
| --- | --- |
| `open` | Every call proceeds; the outage is only visible after the fact, in the `audit_gap` record written on recovery. |
| `closed_writes` | Read-scope calls proceed; write-scope calls are refused. |
| `closed_all` | Every call is refused. |

An operator who does not set `fail_mode` gets a transport-dependent default:
**`open` on stdio**, because a dead collector or a full disk must never take
a reviewer's local session offline; **`closed_all` on HTTP**, because a
shared deployment defaults to the safer failure mode.

A configured file sink *is* the persistence: as long as it is writable,
delivery health of any OTLP bridge is irrelevant to the gate. Delivery
health only matters when `path = "none"` — there, the audit stream's own
OTLP export task's health *is* the persistence the gate watches.

A refused call fails exactly like any other tool error: `kind:
"audit_unavailable"` in the ordinary `{"error": {...}}` shape, with the
message "the audit stream is unavailable; this call was not attempted". The
refusal never names a path or an endpoint — a caller learns that the call was
not attempted, not how the gate is wired.

When persistence recovers, an `audit_gap` record is written with `count` (how
many appends failed), `since` (when the first one failed), and `refused` (how
many calls the gate turned away during the outage).

## OTLP export

Three independent OTLP/HTTP signals, each off unless its endpoint resolves:

- **Logs** carry two kinds of record, distinguished by a `ruoqa.stream`
  attribute: `diagnostics` (the `tracing` output governed by `RUST_LOG`) and
  `audit` (a copy of every audit record, only when `--audit-config` is set —
  a second, independent export task onto the same endpoint, so a slow or
  down collector never blocks the diagnostics stream or vice versa).
- **Traces** carry one `mcp.tool/<name>` SERVER span per tool call, with an
  `openqa.request` CLIENT child span for each upstream HTTP request the call
  made. Attributes include `tool`, `scope`, `outcome`, `error.kind`, and
  `server`.
- **Metrics** carry two cumulative instruments: `ruoqa.tool.calls` (a
  counter, attributed by `tool`/`server`/`outcome`/`error_kind`) and
  `ruoqa.tool.duration` (a millisecond histogram, attributed by
  `tool`/`server`).

### `OTEL_*` variables

There is no default endpoint — unset means off. Setting any endpoint below
lights up that signal (or, for the base endpoint, every signal that has no
more specific override).

| Variable | Default | Meaning |
| --- | --- | --- |
| `OTEL_SDK_DISABLED` | `false` | `true` (case-insensitive) disables every signal regardless of anything else below. |
| `OTEL_SERVICE_NAME` | `ruoqa-mcp` | `service.name` resource attribute. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | *(unset)* | Base endpoint. Each signal appends its own path (`v1/logs`, `v1/traces`, `v1/metrics`) with exactly one `/`, e.g. `http://host:4318` and `http://host:4318/otlp` both work as expected. |
| `OTEL_EXPORTER_OTLP_{LOGS,TRACES,METRICS}_ENDPOINT` | *(unset)* | Per-signal endpoint override, used **verbatim** — no path is appended. |
| `OTEL_EXPORTER_OTLP_HEADERS` / `_{LOGS,TRACES,METRICS}_HEADERS` | *(unset)* | `k=v,k2=v2`, percent-decoded. A per-signal value **shadows** the base value; it does not merge with it. |
| `OTEL_EXPORTER_OTLP_TIMEOUT` / `_{LOGS,TRACES,METRICS}_TIMEOUT` | `10000` (ms) | Per-request export timeout. |
| `OTEL_EXPORTER_OTLP_PROTOCOL` / `_{LOGS,TRACES,METRICS}_PROTOCOL` | `http/protobuf` | Only value accepted; anything else is a startup error. |
| `OTEL_EXPORTER_OTLP_COMPRESSION` / `_{LOGS,TRACES,METRICS}_COMPRESSION` | `none` | Only value accepted; anything else is a startup error. |
| `OTEL_{LOGS,TRACES,METRICS}_EXPORTER` | `otlp` | `otlp` or `none`. `none` is the per-signal off switch, checked before any endpoint parsing. |
| `OTEL_TRACES_SAMPLER` | `always_on` | `always_on` or `parentbased_always_on`. Validated at startup even if the traces signal ends up unconfigured. |
| `OTEL_METRIC_EXPORT_INTERVAL` | `60000` (ms) | Metrics export period. `0` is rejected (a busy loop, not "export immediately"). |
| `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | `cumulative` | Only value accepted; `delta`/`lowmemory` are startup errors. |
| `OTEL_BLRP_MAX_QUEUE_SIZE` | `2048` | Bounded export queue size, shared by every signal's export task. |
| `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE` | `512` | Records per export batch. |
| `OTEL_BLRP_SCHEDULE_DELAY` | `5000` (ms) | Delay between scheduled batch flushes. |

The following are **rejected at startup**, base and per-signal alike, because
honoring them would change the wire without the operator noticing:
`OTEL_EXPORTER_OTLP{,_LOGS,_TRACES,_METRICS}_CERTIFICATE`, `_CLIENT_KEY`, and
`_CLIENT_CERTIFICATE`. This crate has no custom-TLS-material support; set
these and the process refuses to start rather than silently ignore them.

### Startup probe and shutdown flush

Before binding a socket or serving stdio, every configured signal is probed
once against its collector. **A failed probe is fatal**: the process exits
with an error before any session starts, on both transports. This is also
why `--help`/`--version` are handled before telemetry initializes — they
must work even with a dead collector configured.

On shutdown, buffered records are flushed with a 5-second budget. This flush
is unconditional — it runs even when the server is exiting on an error path,
since telemetry about a failing run is the most valuable kind.

## Correlation

A tool call's exported `mcp.tool/<name>` span's trace and span ids are copied
onto that call's audit record as `trace`/`span` (lowercase hex) — the same
ids, not re-derived ones. Every exported log record carries `ruoqa.stream`
(`audit` or `diagnostics`). Together, a collector query can join the audit
line, the diagnostics logs, and the trace for one call by trace/span id and
`ruoqa.stream`.

> The exported audit stream is exactly as sensitive as the audit file — it
> carries comment text and other tool arguments verbatim, which is the point
> of an audit trail. Treat the collector and its storage with the same care
> as the audit file itself.

> `OTEL_EXPORTER_OTLP_HEADERS` (and its per-signal variants) is a
> **credential**: environment-only, with no CLI flag and no audit-config key,
> and never logged. A failed export deliberately discards the underlying
> `reqwest::Error`, because that error carries the request URL.

## Operational notes

- Dropped export records are counted per reason (`queue_full`, `network`,
  `http_status`, `shutdown`) and a warning fires when a reason's running
  total reaches a power of two (1, 2, 4, 8, …), carrying only the reason and
  the count — never a URL, an error, or a header.
- The export pipeline's own tracing targets (`ruoqa_mcp::otel`, and the HTTP
  stack beneath it: `reqwest`, `hyper`, `hyper_util`, `rustls`, `h2`,
  `tower`) are excluded from the diagnostics stream, so a failing export can
  never log its way into queuing another export.
- `RUST_LOG` governs the diagnostics stream: the OTLP diagnostics layer
  defaults to `info` when telemetry is configured, while the stderr `fmt`
  layer stays ERROR-only by default, unchanged.
