# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
