# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.1](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.5.0...v0.5.1) - 2026-09-05

### Other

- update Cargo.lock dependencies

## [0.5.0](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.4.0...v0.5.0) - 2026-08-30

### Added

- add a fail-closed gate for the audit stream
- export a metrics signal over OTLP/HTTP
- export a traces signal over OTLP/HTTP and correlate it with logs
- emit tracing diagnostics and bridge the audit stream over OTLP
- resolve OTEL_* env and export logs over OTLP/HTTP
- add a pure-std OTLP protobuf wire writer (otel::proto)
- add --transport, deprecate --http/--stdio
- add a JSONL audit stream for every tool call

### Fixed

- collapse nullable-union tool schemas for Gemini/Vertex compat
- clear clippy warnings surfaced by CI's floating toolchain

### Other

- realign the README with the audit-stream and tool registry
- document the audit stream and OTLP export, add examples and container wiring
- *(deps)* [**breaking**] bump ruoqa to 0.3.0
- *(deps)* refresh the lockfile

## [0.4.0](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.3.2...v0.4.0) - 2026-08-19

### Added

- [**breaking**] require a server argument on every tool, add list_servers (gh#3)
- parse OPENQA_SERVER as a list and add ServerRegistry (gh#3)
- add get_job_log_errors, a tiered failure digest over a job's logs

### Fixed

- exclude the --timeout <n> CLI-flag form from the noise marker
- stop timeout marker false-positives, catch worker-level verdicts
- report the artefact total, not the tail window, in unsupported_media

### Other

- note list_job_logs covers logs and ulogs only, not assets

## [0.3.2](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.3.1...v0.3.2) - 2026-08-18

### Added

- add job-log-artifact tools (list_job_logs, list_job_log_members, get_job_log)

### Other

- fix cargo-deny warnings and update dependencies
- bump h2 to 0.4.16, fixing RUSTSEC-2026-0258

## [0.3.1](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.3.0...v0.3.1) - 2026-08-16

### Added

- add container image for HTTP-transport deployment

## [0.3.0](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.2.0...v0.3.0) - 2026-08-15

### Added

- [**breaking**] delegate restart_jobs to openQA's bulk endpoint
- [**breaking**] bound amplifying arguments and the whole-call duration
- [**breaking**] authenticate HTTP callers with scoped bearer tokens

### Fixed

- [**breaking**] classify upstream failures as tool-level errors instead of -32603
- [**breaking**] require the documented jobs shape under summary=true
- [**breaking**] reject trigger_isos extra keys that collide with reserved fields
- fix!(config): reject non-finite or out-of-range duration env vars
- percent-encode cancel_scheduled_product's name into one path segment
- [**breaking**] reject cancel_jobs calls with no filters

### Other

- correct tool count from 40 to 39
- add a routing matrix over all 39 tools
- warn that OPENQA_VERIFY=false disables TLS certificate checks
- release v0.1.3

## [0.2.0](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.1.2...v0.2.0) - 2026-08-11

### Added

- [**breaking**] adopt ruoqa 0.2.0

### Other

- *(deps)* refresh the lockfile
- add project logo and GitHub social preview header

## [0.1.2](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.1.1...v0.1.2) - 2026-08-04

### Added

- *(cli)* add --version/-V flag

### Other

- stop tracking plans/ directory

## [0.1.1](https://github.com/mimi1vx/ruoqa-mcp/compare/v0.1.0...v0.1.1) - 2026-08-04

### Other

- automate crates.io releases with release-plz + Trusted Publishing
