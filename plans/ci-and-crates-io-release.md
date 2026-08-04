# CI + crates.io release for `ruoqa-mcp`

Status: plan (not yet executed). Build mode required to apply.

Style source: **`mimi1vx/ruoqa`** — its `ci.yml`, `release.yml`, `release-plz.yml`,
`release-plz.toml`, `deny.toml`, `dependabot.yml` and `Cargo.toml` are the
reference. This plan copies that shape and documents every place `ruoqa-mcp` is
forced to diverge.

## Current state

`ruoqa-mcp` is a single-crate binary+lib at `0.1.0`, one commit deep (`feat: port
openqa-mcp from Python to Rust` — already Conventional Commits), public at
`github.com/mimi1vx/ruoqa-mcp`. `cargo fmt --check`, `cargo clippy --all-targets`
and `cargo test` (8 wiremock tests) pass on rustc 1.97.1. No `.github/` exists.
`Cargo.toml` has none of `repository`, `readme`, `keywords`, `categories`,
`rust-version`, `[lints]`. The name `ruoqa-mcp` is unclaimed on crates.io.

## Decisions taken (clarification rounds)

1. Publishing auth: **crates.io Trusted Publishing (OIDC)**.
2. Versioning/tagging: **release-plz**, same as ruoqa (`publish = false`, publish
   happens via OIDC in `release.yml` called through `workflow_call`).
3. CI job set: **match ruoqa** — `fmt / clippy / test / msrv / deny / semver`.
   No `beta` job, no `cargo-audit` job (cargo-deny's advisories check reads the
   same RustSec DB).
4. `Cargo.toml` lints: **adopt both**, including fixing the 30 pedantic warnings.
5. `semver` job: **added after** 0.1.0 is on crates.io, so CI is green from the
   first push.
6. Release artifacts: crates.io **plus** Linux `x86_64` + `aarch64` tarballs on
   the GitHub Release. ruoqa is a library and has no equivalent; this is an
   addition, designed to sit inside ruoqa's `release.yml` structure.

### Scope note — please object if wrong

You answered "release workflow only" for how far the ruoqa style should
propagate, but answers 3, 4 and 5 pin the CI job set, the lint config and the
semver job to ruoqa's anyway. The net effect is that `ci.yml` converges on
ruoqa's design too. I have therefore aligned `ci.yml` and **dropped the `docs`
(rustdoc) job** from my first plan, since ruoqa has no such job. Say so if you
want that job kept.

## Forced divergences from ruoqa (the actual work)

These are not stylistic choices; each is something ruoqa's config cannot be
copied verbatim for.

1. **`unsafe_code = "forbid"` is impossible here.** `src/heartbeat.rs:100,105,111`
   and `src/cli.rs:83,93,99,106,112,114,118` call `unsafe { std::env::set_var(…) }`
   inside `#[cfg(test)]` modules (edition 2024 made those unsafe). `forbid`
   applies crate-wide and cannot be locally overridden. Use
   `unsafe_code = "deny"` plus `#[allow(unsafe_code)]` on the two test modules.
2. **`deny.toml` exceptions must list two crates, not one.** ruoqa's file excepts
   only `ruoqa` because it was the GPL root. Here `ruoqa-mcp` is GPL *and*
   depends on `ruoqa`, which is also `GPL-3.0-or-later` — so a verbatim copy
   fails the licenses check immediately.
3. **The allow-list needs more SPDX ids.** ruoqa's dep tree is small; `ruoqa-mcp`
   adds axum, rmcp, clap, tracing-subscriber, wiremock. Expect additions beyond
   ruoqa's ten entries — notably `Apache-2.0 WITH LLVM-exception` and the
   compound `aws-lc-sys` string. Keep the permissive-only policy and its comment.
4. **`release.yml` gains a `binaries` job**, which needs `contents: write`.
   Called-workflow permissions are **capped by the caller**, so `release-plz.yml`'s
   `publish:` job must widen from `contents: read` to `contents: write`. Easy to
   miss; it fails at upload time, after a successful publish.
5. **Trusted Publishing cannot bootstrap a new crate.** crates.io docs: *"Your
   crate must already be published to crates.io (initial publish requires an API
   token)."* 0.1.0 must be published by hand before release-plz/OIDC can take
   over — and the `v0.1.0` tag must exist before release-plz runs, or it will
   immediately try to re-release it.

## Established facts

- **MSRV is 1.96**, forced by `ruoqa 0.1.2`. Same pin as ruoqa's `msrv` job.
- **TLS is rustls + `aws-lc-rs`** (`aws-lc-sys` in `Cargo.lock`) — a C/cmake
  build. The repo is public, so the free **`ubuntu-24.04-arm` native runner**
  avoids cross-compiling it. No `cross`, no docker.
- **30 clippy-pedantic warnings** across 7 files (enumerated in step 2).
  `pedantic = "warn"` + CI's `-D warnings` means pedantic is effectively denied,
  so these must all go before CI can be green.

## Plan

### Phase 1 — make the crate publishable and pedantic-clean

1. **[trivial] `Cargo.toml`: publish metadata + lints, mirroring ruoqa's layout.**
   ```toml
   rust-version = "1.96"
   repository = "https://github.com/mimi1vx/ruoqa-mcp"
   readme = "README.md"
   keywords = ["mcp", "openqa", "qa", "testing", "api"]
   categories = ["command-line-utilities", "api-bindings", "development-tools::testing"]
   exclude = ["/plans", "/.github"]

   [lints.rust]
   unsafe_code = "deny"   # ruoqa uses `forbid`; see divergence #1

   [lints.clippy]
   pedantic = "warn"
   ```
   Field order to match ruoqa's `[package]`: name, version, edition,
   rust-version, license, description, repository, readme, keywords, categories.
   — verify: `cargo package --list` shows `src/`, `tests/`, `README.md`,
   `COPYING`, `Cargo.toml`, `Cargo.lock` and no `plans/` or `.github/`.

2. **[small] Clear the 30 pedantic warnings.** `cargo clippy --fix --all-targets
   -- -W clippy::pedantic` handles ~20 of them; the rest are by hand:

   | file | count | nature |
   | --- | --- | --- |
   | `src/query.rs` | 6 | `must_use` on builders (7, 18, 23, 31×2, 41) |
   | `src/form.rs` | 6 | `must_use` on builders (10, 14, 19, 26×2, 33) |
   | `src/cli.rs` | 5 | doc backticks (15, 27), `must_use` (35, 44, 50), `map(..).unwrap_or(false)` (36) |
   | `src/config.rs` | 6 | `Duration::from_millis`→`from_secs` (14, 146), missing `# Errors` (23, 86), identical match arms (45), `must_use` (67) |
   | `src/server.rs` | 4 | `must_use` (28), redundant closures (173, 180), `needless_pass_by_value` (77) |
   | `src/main.rs` | 1 | `Arc::default()` (91) |

   `src/server.rs:77` is `pub(crate) fn err(e: ruoqa::Error)`. It is used as
   `result.map_err(err)`, which requires by-value — so the fix is
   `#[allow(clippy::needless_pass_by_value)]` with a one-line reason, **not** a
   signature change to `&ruoqa::Error`.
   — verify: `cargo clippy --all-targets --locked -- -D warnings` exits 0 with
   the `[lints]` table in place; `cargo test` still 8/8.

3. **[trivial] Add `#[allow(unsafe_code)]` to the two test modules.**
   On the `mod tests` in `src/heartbeat.rs` (line 71) and `src/cli.rs` (line 55).
   — verify: `cargo test --locked` compiles and passes; `cargo build --locked`
   produces no `unsafe_code` diagnostics.

4. **[trivial] Confirm MSRV 1.96 actually holds.**
   — verify: `rustup toolchain install 1.96 && cargo +1.96 check --locked
   --all-targets` succeeds. If not, raise `rust-version` and the `msrv` job pin
   together.

### Phase 2 — CI, in ruoqa's shape

5. **[small] Create `.github/workflows/ci.yml`.** Structural copy of ruoqa's:
   `on: push: branches: [main]` + `pull_request`; `env: CARGO_TERM_COLOR: always`;
   `permissions: contents: read`; `concurrency: ${{ github.workflow }}-${{ github.ref }}`
   with `cancel-in-progress: true`. `actions/checkout@v7`,
   `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`.

   | job | steps |
   | --- | --- |
   | `fmt` | toolchain `components: rustfmt` → `cargo fmt --check` |
   | `clippy` | `components: clippy`, cache → `cargo clippy --all-targets --locked -- -D warnings` |
   | `test` | cache → `cargo test --locked` |
   | `msrv` | `toolchain: "1.96"`, cache → `cargo check --locked --all-targets` |
   | `deny` | `EmbarkStudios/cargo-deny-action@v2` with `command: check` |

   No `--all-features` (the crate declares no features), matching ruoqa. `semver`
   is deliberately absent here — step 12 adds it.
   — verify: push a branch, five jobs green. Then add a stray pedantic-triggering
   line and confirm `clippy` goes red — proves `[lints] pedantic` + `-D warnings`
   actually bites.

6. **[trivial] Create `deny.toml`.** ruoqa's file is licenses-only and relies on
   cargo-deny defaults for advisories/bans/sources; keep that. Start from its
   allow-list and comment verbatim, with the exceptions widened:
   ```toml
   exceptions = [
     { allow = ["GPL-3.0-or-later"], crate = "ruoqa-mcp" },
     { allow = ["GPL-3.0-or-later"], crate = "ruoqa" },
   ]
   ```
   — verify: `cargo deny check` reports ok for advisories, bans, licenses,
   sources. Expect one or two iterations adding SPDX ids the larger dep tree
   drags in (divergence #3); add only permissive ones, and if anything
   GPL-incompatible appears, treat it as a real finding rather than allow-listing
   it.

7. **[trivial] Create `.github/dependabot.yml`.** Copy ruoqa's verbatim — cargo
   weekly with `open-pull-requests-limit: 5` and a `cargo-minor-patch` group,
   github-actions weekly with an `actions: ["*"]` group, commit prefixes
   `chore(deps)` / `chore(deps-dev)` / `ci(deps)`.
   — verify: Insights → Dependency graph → Dependabot lists both ecosystems with
   a "Last checked" time and no config error.

### Phase 3 — bootstrap 0.1.0 by hand (unavoidable, see divergence #5)

Do this only once Phase 2 is green on `main`, and **before** release-plz lands.

8. **[trivial] Publish and tag 0.1.0.**
   ```sh
   cargo publish --dry-run --locked
   cargo login                       # scoped token: publish-new + publish-update
   cargo publish --locked
   git tag -a v0.1.0 -m "v0.1.0" && git push origin v0.1.0
   ```
   The tag matters as much as the publish: without it release-plz will see no
   released version and immediately open a release PR for 0.1.0.
   — verify: `crates.io/crates/ruoqa-mcp` serves 0.1.0 with README, repo link,
   keywords and categories; `cargo install ruoqa-mcp` from a clean `CARGO_HOME`
   yields a working `ruoqa-mcp --help`.

9. **[trivial] Configure Trusted Publishing, create the environment, revoke the
   token.**
   - crates.io → crate Settings → Trusted Publishing → Add → GitHub:
     owner `mimi1vx`, repo `ruoqa-mcp`, workflow filename `release.yml`,
     environment **`release`** (ruoqa's name — keep it identical).
   - GitHub → Settings → Environments → create `release`, restrict deployment
     branches/tags to `v*`. The environment is what stops a workflow on an
     arbitrary branch from minting a publish token.
   - Delete the step-8 API token from crates.io account settings.
   — verify: TP entry listed on the crate settings page; bootstrap token gone
   from the token list.

### Phase 4 — automated releases, ruoqa's model

10. **[trivial] Create `release-plz.toml`.**
    ```toml
    [workspace]
    publish = false # actual `cargo publish` happens via OIDC in release.yml (tag-triggered)
    ```
    — verify: `release-plz release --dry-run` reports it would tag but not
    publish.

11. **[medium] Create `.github/workflows/release.yml` and
    `.github/workflows/release-plz.yml`.**

    `release.yml` — structural copy of ruoqa's, including the header comment
    explaining *why* `workflow_call` exists (GitHub suppresses workflow triggers
    for events raised by `GITHUB_TOKEN`, so release-plz's tag fires no `push`).
    Triggers: `workflow_call` (input `ref`), `workflow_dispatch` (input `ref`),
    `push: tags: ['v*']`.
    - Job `publish`: `environment: release`, `permissions: id-token: write` +
      `contents: read`, `actions/checkout@v7` with `ref: ${{ inputs.ref }}` and
      `persist-credentials: false`, `rust-lang/crates-io-auth-action@v1`, then
      the same idempotent publish shell block ruoqa uses — swallow
      `already (uploaded|exists)` as a `::notice::` no-op.
    - Job `binaries` (**new vs ruoqa**, `needs: publish` not required — run it in
      parallel so a build failure can't block the registry publish):
      `permissions: contents: write`, matrix
      `ubuntu-latest`/`x86_64-unknown-linux-gnu` and
      `ubuntu-24.04-arm`/`aarch64-unknown-linux-gnu`, both native
      (`cargo build --release --locked`). Package
      `ruoqa-mcp-<tag>-<target>.tar.gz` with the stripped binary + `README.md` +
      `COPYING`, plus a `.sha256`. Resolve the tag as
      `${{ inputs.ref || github.ref_name }}`, then
      `gh release create "$TAG" --generate-notes || true` followed by
      `gh release upload "$TAG" … --clobber` — create-if-missing covers the
      hand-pushed-tag path where release-plz never made a Release, and
      `--clobber` keeps `workflow_dispatch` retries idempotent, matching the
      spirit of ruoqa's publish guard.

    `release-plz.yml` — copy ruoqa's three-job structure verbatim
    (`release-plz-release` → conditional `publish` via `uses:
    ./.github/workflows/release.yml` → `release-plz-pr`), `actions/checkout@v7`
    with `fetch-depth: 0` and `persist-credentials: false`, pinned
    `release-plz/action@v0.5.131`.
    **One change:** the `publish:` job's permissions become
    `id-token: write` + `contents: write` (divergence #4) so the called
    `binaries` job can upload assets.
    — verify: after landing, a `feat:`/`fix:` commit on `main` opens a release PR
    bumping to 0.1.1 with a generated `CHANGELOG.md`. Merging it tags `v0.1.1`,
    publishes to crates.io via OIDC, and attaches two `.tar.gz` + two `.sha256`.
    Re-running the `publish` job via `workflow_dispatch` on `v0.1.1` exits 0 with
    the "already published" notice rather than failing.

12. **[trivial] Add the `semver` job to `ci.yml`.** Now that 0.1.0 is a baseline:
    `actions/checkout@v7` + `obi1kenobi/cargo-semver-checks-action@v2`, exactly
    as in ruoqa.
    — verify: job green on a no-op PR; introducing a breaking change to a `pub`
    item in `src/lib.rs` turns it red.

13. **[trivial] README: CI badge + install-from-crates.io.**
    Add the `ci.yml` badge and lead the Install section with
    `cargo install ruoqa-mcp`, keeping the from-source form secondary.
    — verify: badge renders green; crates.io page shows the same README.

## Files

- Modify: `Cargo.toml`, `README.md`, `src/query.rs`, `src/form.rs`, `src/cli.rs`,
  `src/config.rs`, `src/server.rs`, `src/main.rs`, `src/heartbeat.rs`
- Create: `.github/workflows/ci.yml`, `.github/workflows/release.yml`,
  `.github/workflows/release-plz.yml`, `.github/dependabot.yml`,
  `release-plz.toml`, `deny.toml`
- Delete: none
- Created later by the bot: `CHANGELOG.md`

## Risks

- **Ordering in Phase 3/4 is load-bearing.** Landing `release-plz.yml` before the
  manual `v0.1.0` tag exists makes release-plz open a release PR for a version
  that is already published; landing it before TP is configured makes the publish
  job fail on OIDC. Bootstrap first, automate second.
- **Caller-capped permissions.** `contents: write` must be granted in
  `release-plz.yml`'s `publish:` job, not just in `release.yml`'s `binaries:`
  job. The failure surfaces only after a successful crates.io publish, so the
  retry is awkward — the `--clobber` + idempotent-publish design is what makes
  re-running safe.
- **`aws-lc-sys` on the aarch64 runner.** Needs cmake + a C compiler;
  `aarch64-unknown-linux-gnu` has pregenerated bindings so it should build, but
  it is the most likely thing to break. Keeping `binaries` off `publish`'s
  critical path means a failure costs assets, not the release.
- **`pedantic = "warn"` is a standing tax.** Every dependency bump and new
  function can add warnings that CI treats as errors. That is the same deal
  ruoqa took; noting it so it is a choice rather than a surprise.
- **MSRV 1.96 is one release behind stable (1.97.1).** Expect frequent
  `rust-version` bumps as deps move.
- **release-plz requires Conventional Commits** to compute bumps. The existing
  history complies; future commits must too, or versions will not bump.

## Alternatives considered

- **Keeping `unsafe_code = "forbid"` by removing the test `env::set_var` calls**
  (via a `temp-env`-style helper or by injecting config instead of reading env)
  — rejected as a refactor of working test code that the request did not ask
  for. `deny` + two `#[allow]`s is the surgical equivalent.
- **Changing `err(e: ruoqa::Error)` to take `&ruoqa::Error`** to satisfy
  `needless_pass_by_value` — rejected: it is used as `map_err(err)`, which needs
  by-value, so the "fix" would force a closure at every call site.
- **Tag-triggered manual releases** (my first plan) — superseded by release-plz
  for parity with ruoqa.
- **A separate `cargo-audit` job** — dropped: cargo-deny's advisories check reads
  the same RustSec advisory DB, and ruoqa has no such job.
- **`cross` for aarch64** — rejected: docker + cross-toolchain purely to work
  around `aws-lc-sys` when a free native arm runner exists for public repos.

## Success criteria

- [ ] `cargo clippy --all-targets --locked -- -D warnings` exits 0 with
      `[lints.clippy] pedantic = "warn"` in `Cargo.toml`.
- [ ] `cargo test --locked` passes 8/8 with `unsafe_code = "deny"` in effect.
- [ ] `cargo +1.96 check --locked --all-targets` succeeds.
- [ ] `cargo publish --dry-run --locked` exits 0, no metadata warnings;
      `cargo package --list` has no `plans/` or `.github/`.
- [ ] `cargo deny check` reports ok for all four classes, with GPL exceptions for
      both `ruoqa-mcp` and `ruoqa`.
- [ ] CI green on `main`: `fmt`, `clippy`, `test`, `msrv`, `deny`.
- [ ] Dependabot shows both ecosystems, grouped, no config error.
- [ ] `crates.io/crates/ruoqa-mcp` serves 0.1.0; `cargo install ruoqa-mcp` in a
      clean `CARGO_HOME` gives a working `ruoqa-mcp --help`.
- [ ] Bootstrap API token deleted; TP entry present for
      `mimi1vx/ruoqa-mcp` / `release.yml` / environment `release`; the `release`
      environment restricts deployments to `v*`.
- [ ] A `feat:` commit on `main` opens a release-plz PR with a `CHANGELOG.md`
      entry; merging it publishes 0.1.1 via OIDC with no token in any secret.
- [ ] The 0.1.1 GitHub Release carries two `.tar.gz` and two `.sha256`;
      `sha256sum -c` passes and each binary runs `--help` on its own arch.
- [ ] Re-dispatching `release.yml` on an already-published tag exits 0 with the
      "already published" notice.
- [ ] `semver` job green after being added; a breaking change to a `pub` item in
      `src/lib.rs` turns it red.

## Post-execution corrections

All phases executed and every success criterion above verified live
(0.1.0 hand-published, 0.1.1 released end-to-end through the automated
path). Two details in this plan turned out to be wrong when exercised
against real GitHub/crates.io behavior — noted here so a future re-read
doesn't reintroduce the bugs:

1. **The `release` environment must *not* restrict deployment
   branches/tags.** Step 9 said to restrict to `v*`; doing so makes
   GitHub reject the `release-plz.yml` → `uses: ./release.yml` call with
   *"Branch main is not allowed to deploy to release"*, because
   environment protection is evaluated against the **top-level**
   triggering ref (`refs/heads/main`), not the tag passed via the
   `ref:` input. `mimi1vx/ruoqa`'s actual `release` environment (checked
   via the API) has `deployment_branch_policy: null` — no restriction at
   all. Matched that here.
2. **Trusted Publishing needs two entries, not one.** crates.io's OIDC
   JWT carries the **top-level** workflow filename, same caveat as
   above: a run triggered by `push:main` → `release-plz.yml` calling
   `release.yml` presents `release-plz.yml` in the JWT, not
   `release.yml`, and crates.io rejects a token request that doesn't
   match a registered publisher. Registered two GitHub configs via
   `POST /api/v1/trusted_publishing/github_configs` (crates.io API,
   requires the bootstrap token — this predates deleting it), both
   `environment: release`: one `workflow_filename: release.yml` (covers
   `workflow_dispatch`/hand-pushed-tag runs) and one
   `workflow_filename: release-plz.yml` (covers the automated path).
   `mimi1vx/ruoqa` had not yet exercised its automated path at the time
   of writing and would hit the same error.
3. **Not in the plan at all:** the repo-level "Allow GitHub Actions to
   create and approve pull requests" setting
   (`can_approve_pull_request_reviews` via
   `PUT /repos/{owner}/{repo}/actions/permissions/workflow`) defaults to
   off on new repos and must be enabled, or `release-plz-pr`'s `gh pr
   create` equivalent fails with a 403 regardless of the job's own
   `pull-requests: write` permission.
