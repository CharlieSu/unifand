# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/) —
with the pre-1.0 caveat that minor version bumps may carry breaking changes
(protocol/metric shape changes), called out explicitly below.

## [0.6.3](https://github.com/CharlieSu/unifand/compare/v0.6.2...v0.6.3) (2026-09-03)


### Bug Fixes

* **deps:** update rust crate nvml-wrapper to 0.13 ([#32](https://github.com/CharlieSu/unifand/issues/32)) ([325d7de](https://github.com/CharlieSu/unifand/commit/325d7dee51e10829bf6dd8a41e9c62042ecae55c))
* **deps:** update rust:1.98.0-slim-trixie docker digest to 17d1ba8 ([#28](https://github.com/CharlieSu/unifand/issues/28)) ([a636df3](https://github.com/CharlieSu/unifand/commit/a636df3910784b98f48a3dc0ad0c153233e07f01))

## [0.6.2](https://github.com/CharlieSu/unifand/compare/v0.6.1...v0.6.2) (2026-08-25)


### Bug Fixes

* **build:** sync Rust pins and release image-affecting dep updates ([1159b23](https://github.com/CharlieSu/unifand/commit/1159b2337c4c48ed7cf0778f239a298fdbe56f18))

## [0.6.1](https://github.com/CharlieSu/unifand/compare/v0.6.0...v0.6.1) (2026-08-19)


### Bug Fixes

* **contrib:** aggregate dashboard queries by node to survive pod churn ([#16](https://github.com/CharlieSu/unifand/issues/16)) ([b7fcb65](https://github.com/CharlieSu/unifand/commit/b7fcb656bffb036015721462e2c2a8092e2881e4))

## [0.6.0](https://github.com/CharlieSu/unifand/compare/v0.5.1...v0.6.0) (2026-08-19)


### Features

* **signals:** multi-signal fusion — GPU power, thermal margin, throttle floor ([#14](https://github.com/CharlieSu/unifand/issues/14)) ([78f34af](https://github.com/CharlieSu/unifand/commit/78f34af955311e5fc2bcdc24b9945f20d2bea35c))

## [0.5.1](https://github.com/CharlieSu/unifand/compare/v0.5.0...v0.5.1) (2026-08-19)


### Bug Fixes

* **build:** cross-compile target arches instead of QEMU emulation ([9c15695](https://github.com/CharlieSu/unifand/commit/9c15695d8fb7b2b8d4dcc0eb43875012a272606d))
* **deps:** update rust crate nvml-wrapper to 0.12 ([#2](https://github.com/CharlieSu/unifand/issues/2)) ([929a1df](https://github.com/CharlieSu/unifand/commit/929a1df230ddb9f85ddcc2c76251f2669f9b44f5))
* **deps:** update rust crate signal-hook to 0.4 ([#3](https://github.com/CharlieSu/unifand/issues/3)) ([800dced](https://github.com/CharlieSu/unifand/commit/800dced9a6e8c1d7266c81ce02f752bb88a2a747))
* **deps:** update rust crate toml to v1 ([#11](https://github.com/CharlieSu/unifand/issues/11)) ([91a7398](https://github.com/CharlieSu/unifand/commit/91a73985557056991796c1a68363dbc612fb074d))

## [0.5.0] (2026-08-18)


### Features

* **contrib:** fan-stall alert rule ([6dc0518](https://github.com/CharlieSu/unifand/commit/6dc051873375ba13e16b1b0b6439c86036b9b494))

<!-- Sections below predate this repository's public history baseline;
     their tags/commits are not published, so version headers are unlinked. -->
## [0.4.0] - 2026-08-18

A hardening release: closes the observability/liveness gap found by an
internal six-lens review (Rust, docs, CI, Flux, monitoring, SRE), fixes a
real fan-chain hardware bug, and adds supply-chain/deploy hardening plus
community docs and observability contrib artifacts ahead of a public
release.

### Added

- `unifand_last_tick_timestamp_seconds` gauge and a `/healthz` endpoint on
  the metrics server: the DaemonSet's livenessProbe now checks control-loop
  progress (`now - last_tick <= 3 * poll_interval_secs`), not just whether
  the (independent) metrics HTTP thread is still answering — a wedged
  control loop used to look "healthy" forever.
- `unifand_fallback_active` gauge — 1 when all sensors are lost and
  `fallback_duty` is being forced; previously only visible as a log line.
- `unifand_hub_present` gauge — 1 when the SL V2 hub is currently open and
  initialized; 0 during the new startup hub-wait (see below) or after the
  hub is lost.
- `unifand_build_info{version="x.y.z"}` gauge, always 1 — lets a scrape
  identify which build is running on which node.
- Optional `[rgb.fans]` TOML table for a per-channel LED chain-length
  override (e.g. `1 = 3`, `2 = 6`), falling back to `[rgb] fans_per_channel`
  for any channel not listed.
- Unknown-config-key warnings via `serde_ignored`: a misspelled or removed
  key (e.g. `fallbackduty` instead of `fallback_duty`) is now logged at
  `warn` on load instead of silently doing nothing — while still not
  failing validation, so config rollback across versions stays safe.
- Sensor re-discovery: a CPU or GPU sensor that's absent, or whose reads
  have failed 3+ ticks in a row, is retried at most every ~60s instead of
  only at process startup — recovers from late driver/toolkit injection or
  a mid-run driver restart without a pod restart.
- Startup hub-wait: a missing hub at startup is no longer fatal. The daemon
  now serves `/metrics`/`/healthz` immediately and retries hub discovery
  every 5s (warning at most every 30s) until the hub appears or shutdown is
  requested — replaces the previous crashloop-with-kubelet-backoff behavior.
- CI: a `cargo-audit` + `cargo-deny` job (`deny.toml`: advisory denial,
  MIT/Apache-2.0/BSD-3-Clause/Unicode/ISC/Zlib license allowlist,
  multiple-versions warn), gating the release (`image`/`deploy-artifact`)
  jobs.
- Docker build: a dependency-cache layer (dummy-`main.rs` pattern) so
  dependency compilation is cached across builds that only change `src/`.
- `deploy/base/priorityclass.yaml` (`unifand-critical`, value `100010000`)
  applied to the DaemonSet, so the thermal-safety daemon isn't evicted
  ahead of ordinary pods under node pressure.
- `deploy/components/network-policy/` — opt-in Kustomize component
  restricting ingress on the metrics port to a namespace you label.
- `contrib/`: a raw importable Grafana dashboard, a generic Prometheus
  alerting-rules file (six rules), and PodMonitor / VMPodScrape / plain
  pod-annotation scrape examples for the three Prometheus-adjacent
  ecosystems. See `contrib/README.md`.
- `CONTRIBUTING.md`, `SECURITY.md`, this `CHANGELOG.md`.
- README: a Troubleshooting section, a CLI section documenting `--oneshot`
  (including as a pre-apply GitOps config-validation gate), a validated
  Flux `.spec.components` + regex-targeted ConfigMap `.spec.patches`
  composition recipe, and a Monitoring section pointing at `contrib/`.

### Changed

- **Breaking:** `unifand_hid_errors_total` gained a `kind="write"|"read"`
  label (previously unlabeled). Existing alert rules or dashboard panels
  querying it without the label need updating to sum across `kind` or pick
  one.
- **Breaking:** `[rgb] fans_per_channel` default changed from `4` to `6`.
  Root cause: this value is the LED chain length declared to the hub in
  the RGB start packet (byte 3), not a cosmetic setting — `4` on a 6-fan
  chain silently left fans 5-6 dark. Shipped example configs updated to
  `6`; anyone with a config pinning `fans_per_channel = 4` on a genuinely
  4-fan chain is unaffected, but anyone relying on the old default on a
  longer chain will now see previously-dark fans light up.
- The DaemonSet's livenessProbe now targets `/healthz` instead of
  `/metrics`.
- Container `securityContext` gained `readOnlyRootFilesystem: true`; pod
  spec gained `automountServiceAccountToken: false`.
- Base images pinned: `rust:1.97.1-slim-trixie` (builder),
  `gcr.io/distroless/cc-debian13` (runtime, pinned by digest with a tag
  comment). `rust-version = "1.97"` added to `Cargo.toml`; CI toolchain
  pinned to `1.97.1` (was floating stable).
- `env_logger` built with `default-features = false`.
- Every third-party GitHub Action SHA-pinned (was tag-pinned).
- `deploy-artifact`'s CI job now depends on `audit` passing (previously
  only `test`), so the release/OCI-artifact path is gated by the
  supply-chain checks too.

### Fixed

- LED chain-length bug: fans 5-6 on a 6-fan chain stayed permanently dark
  because the shipped `fans_per_channel` default (4) declared a shorter
  chain than physically connected. Root cause confirmed by hardware spike;
  fixed by the new default (6) plus the `[rgb.fans]` override above.

## [0.3.0] - 2026-08-18

The alarm ladder: LEDs escalate through hub-native animations as thermal
pressure builds.

### Added

- Alarm-ladder state machine (`Normal` → `SustainedHot` → `NearLimit` →
  `Fault`, exported as `unifand_led_state`), driven by a new
  `[rgb.alerts]` config section (thresholds, escalation interval, cooldown,
  colors).
- Effect-mode LED packet builders (breathing, runway) and a hub speed
  table; commit-only writes for speed-only escalation steps.

### Fixed

- LED dispatch now runs every tick, not only on ticks where the duty
  controller returns a new decision — the alarm ladder's speed escalation
  needs to keep advancing even while duty is steady-state.
- `multi_color_packet` total-length calculation over an empty color input.
- A clippy `useless_format` lint.

## [0.2.2] - 2026-08-18

First GHCR release.

### Added

- OSS packaging: Kustomize deploy manifests (base + `nodename`/`nfd-usb`
  overlays + `nvidia-gpu` component), annotated example configs, CI
  workflow, README, LICENSE.

### Changed

- CI: OCI image/tag references lowercased (GHCR requires it;
  `repository_owner` can be mixed-case).
- `rustfmt` normalization pass.

## [0.2.1] - 2026-08-17

### Changed

- Runtime image switched to `distroless/cc` (glibc-based, required for
  `dlopen`-loaded NVML at runtime).

## [0.2.0] - 2026-08-17

Thermal glow: fan LEDs display a duty-mapped color gradient.

### Added

- `[rgb]` config section (gradient stops, brightness, bucket
  quantization) with validation.
- SL V2 RGB packet builders and the hub RGB write sequence.
- Duty-mapped LED color dispatch in the control loop.

## [0.1.1] - 2026-08-17

### Fixed

- Hub-recovery robustness improvements.
- Reject degenerate controller configs (e.g. a curve that can never
  produce a valid decision) at validation time instead of at runtime.

## [0.1.0] - 2026-08-17

Initial release: the core fan-control daemon.

### Added

- GPU-led fan curve: control temperature is `max(gpu, cpu - offset)`,
  piecewise-linear curve interpolation, hysteresis, and slew-limited ramps.
- SL V2 USB HID protocol: hidraw device discovery, packet builders/parsers,
  RPM readback.
- `k10temp` (CPU) and NVML (GPU) sensors with graceful degradation when a
  sensor is unavailable.
- Prometheus metrics endpoint.
- Fail-safe control loop: SIGTERM/SIGINT handling writes a configurable
  fallback duty before exit.
- TOML config with defaults and validation.
- Multi-stage container build.
