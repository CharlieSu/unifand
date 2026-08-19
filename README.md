# unifand

**Temperature-led fan control for the Lian Li UNI HUB SL V2, built for Kubernetes.**

unifand is a small Rust daemon that drives Lian Li UNI FAN SL V2 case fans from a
GPU-led temperature curve — and turns the fans' LEDs into a physical load gauge,
sweeping from blue at idle to red at full tilt. It speaks the hub's USB HID wire
protocol directly (no vendor software, no kernel driver) and ships as a ~22 MB
distroless container you deploy with a DaemonSet.

It was born on an immutable-OS Kubernetes node (Talos Linux) with an RTX 5090
doing LLM inference: no package manager on the host, no Windows for L-Connect,
and case fans that sat at a fixed speed while the GPU dumped 500 W into the case.

## Why this exists

Mostly for fun — and because my inference server deserved better thermal
management than it had. The CPU and GPU manage their own fans dynamically,
but the case fans, the things actually responsible for moving 500 W of GPU
exhaust out of the chassis, sat at whatever fixed speed the hub booted with.
They weren't participating in thermal management at all. unifand makes the
case itself a first-class part of the cooling loop: case airflow follows GPU
load, and the LEDs tell you what's happening at a glance.

It's also an experiment in AI-assisted engineering: this project was built in
close collaboration with an AI (Anthropic's Claude) — the protocol
reimplementation, the control loop, the test suite, the deployment tooling,
and much of these docs came out of that loop, with hardware validation on the
real hub gating every protocol claim along the way. The design decisions,
the hardware, and the responsibility for what ships are mine.

## Features

- **GPU-led fan curve** — control temperature is `max(gpu, cpu - offset)`:
  GPU temp via NVML, CPU temp via the `k10temp` hwmon sensor. Piecewise-linear
  curve, hysteresis, and slew-limited ramps so fans never flutter or lurch.
- **Multi-signal fusion** (opt-in) — fans can also follow GPU power draw
  (which leads temperature), thermal headroom to the card's own limit, and
  the GPU's throttle status, fused raise-only: any signal can raise duty
  above what die temp asks for, none can lower it. Off by default; see
  "Multi-signal fusion" below.
- **Thermal glow** — fan LEDs display a duty-mapped color gradient
  (fully configurable stops, brightness, and quantization). Glance at the
  case, know the load.
- **The alarm ladder** — LEDs escalate through hub-native animations as
  thermal pressure builds: slow breathing when heat is sustained, a red
  pulse that beats faster the longer you sit near the thermal limit, and a
  distinct runway pattern when the daemon loses its sensors. All animations
  run on the hub itself — a whole thermal incident costs a handful of USB
  packets.
- **Prometheus metrics** — temps, per-channel duty and rpm, degradation and
  error counters on `:9877`. Dashboard-ready.
- **Fail-safe by design** — the invariant is *daemon-down must never mean
  fans-at-idle under load*: SIGTERM and total sensor loss pin fans at a
  configurable fallback duty; USB resets are detected (even while duty writes
  are hysteresis-held) and the hub is re-initialized with duty re-asserted.
- **Zero host footprint** — one privileged pod with `/dev`; nothing installed
  on the node. Works on immutable OSes (Talos, Bottlerocket, CoreOS).
- **Honest degradation** — no NVIDIA runtime? unifand runs CPU-only and says
  so (`unifand_degraded 1`). GPU workloads are never impacted: NVML is used
  for temperature only and the `nvidia.com/gpu` resource is never claimed.

**The fail-safe guarantee has a boundary: it holds for SIGTERM, not
SIGKILL.** The fallback-duty write happens in the shutdown path after
SIGTERM/SIGINT — normal termination, pod evictions, and rolling
updates all go through that path and land the fallback duty on the hub
before exit. A hard kill (SIGKILL, an OOM-kill, a kernel panic) skips
that path entirely: the hub is left at its last commanded duty in manual
mode until the pod restarts and re-asserts. Nothing in-process can close
this gap — the hub has no watchdog of its own — so the mitigation is
honesty plus alerting: the container's memory limit carries roughly 4x
headroom over measured usage (making an OOM-kill unlikely in normal
operation), `priorityClassName: unifand-critical` resists node eviction
pressure, and the `UnifandAbsent`/`UnifandStuck` alerts in
[`contrib/`](contrib/) catch the resulting gap even when the hard-kill
itself goes unnoticed.

## Supported hardware

| Device | USB ID | Status |
|---|---|---|
| Lian Li UNI HUB SL V2 (SL / SL-INF V2 fans) | `0cf2:a105` | ✅ validated on hardware |

Channels 1-4, up to 6 fans per channel for LED addressing. Other UNI HUB
generations (SL `a100`, AL `a101`, SL-Infinity `a102`, AL V2 `a104`) use
related but different protocols and are not supported yet — PRs welcome.

## Quickstart

### kubectl + kustomize

```sh
# 1. Pick a scheduling overlay and set your node (see "Scheduling" below):
#    deploy/overlays/nodename  — pin to a named node (simplest)
#    deploy/overlays/nfd-usb   — follow the hub via Node Feature Discovery
#
# 2. Edit deploy/base/config.toml (fan curve, channels, RGB) — see examples/.
#    Sanity-check it first: cargo run -- --config deploy/base/config.toml --oneshot
#
# 3. Apply:
kubectl apply -k deploy/overlays/nodename
```

GPU temperature needs the NVIDIA container toolkit's `nvidia` RuntimeClass —
uncomment the `nvidia-gpu` component in your overlay's `kustomization.yaml`.

The base image tag is `ghcr.io/charliesu/unifand:latest` with
`imagePullPolicy: IfNotPresent` — after the first pull on a node, that
behaves like a pin (no silent upgrades on pod restart), but it's *not* a
reproducible one across nodes or over time. For a real pin, either
`kustomize edit set image ghcr.io/charliesu/unifand:v0.6.1` in your overlay, <!-- x-release-please-version -->
or consume the release's digest-pinned OCI artifact via the Flux path below
(pre-pinned by CI on every tag).

### Flux (OCI artifact)

Every release publishes the `deploy/` tree as an OCI artifact with the image
version pre-pinned:

```yaml
apiVersion: source.toolkit.fluxcd.io/v1
kind: OCIRepository
metadata:
  name: unifand
  namespace: flux-system
spec:
  interval: 1h
  url: oci://ghcr.io/charliesu/unifand-deploy
  ref:
    # Track releases from the current one forward (any floor works — a
    # semver range always resolves to the newest matching release).
    semver: ">=0.6.1" # x-release-please-version
---
apiVersion: kustomize.toolkit.fluxcd.io/v1
kind: Kustomization
metadata:
  name: unifand
  namespace: flux-system
spec:
  interval: 10m
  sourceRef:
    kind: OCIRepository
    name: unifand
  path: ./overlays/nodename   # or ./overlays/nfd-usb — patch in your node
  prune: true
```

**Composing GPU support and a custom config through Flux** (no fork needed):
`.spec.components` and `.spec.patches` work against the OCI artifact exactly
like they do against a local checkout. This combines pinning to `./base`,
enabling the `nvidia-gpu` component, setting a nodeSelector, and overriding
`config.toml` — the generated ConfigMap's name is hash-suffixed, so the
patch targets it with a regex:

```yaml
apiVersion: kustomize.toolkit.fluxcd.io/v1
kind: Kustomization
metadata:
  name: unifand
  namespace: flux-system
spec:
  interval: 10m
  sourceRef:
    kind: OCIRepository
    name: unifand
  path: ./base
  components:
    - ../components/nvidia-gpu
  patches:
    - target:
        kind: DaemonSet
        name: unifand
      patch: |
        - op: add
          path: /spec/template/spec/nodeSelector
          value:
            kubernetes.io/hostname: my-node
    - target:
        kind: ConfigMap
        name: unifand-config.*   # matches the hash-suffixed generated name
      patch: |
        - op: replace
          path: /data/config.toml
          value: |
            channels = [1, 2]
            fallback_duty = 60
            # ...your full config.toml contents...
  images:
    - name: ghcr.io/charliesu/unifand
      newTag: v0.6.1 # x-release-please-version
  prune: true
```

Note: Kustomize Components (`.spec.components`) are marked
alpha/experimental by upstream Flux docs — stable in practice for this use
case, but worth knowing if you're deciding how much to depend on it.

Note: the deploy OCI artifact ships with the image already digest-pinned
per release (see above); the `images:` override shown here replaces that
digest pin with your own tag. That's an intentional, sensible trade if
you're redirecting to a mirrored registry — just know it trades the
release's reproducibility guarantee for the tag's mutability.

### Verify

```sh
kubectl -n unifand logs ds/unifand
# sensors: cpu(k10temp)=true gpu(nvml)=true
# hub at /dev/hidraw10
# duty -> 33% (control Some(41.1)C)
```

Fans respond, LEDs take the idle color, and `:9877/metrics` starts reporting.

## Scheduling

The base DaemonSet has **no scheduling constraints on purpose** — you decide
where it runs. Two worked overlays:

- **`overlays/nodename`** — pin to the node the hub is plugged into. Simple,
  explicit, survives everything except moving the hub. Edit
  [`deploy/overlays/nodename/patch.yaml`](deploy/overlays/nodename/patch.yaml)
  and replace `my-node` with your target node's name (`kubectl get nodes -o
  name`). **Applying the overlay unedited is not an error** — kustomize and
  Kubernetes both accept it — it just matches zero nodes and the DaemonSet
  silently sits at `0/0` forever; see Troubleshooting below.
- **`overlays/nfd-usb`** — let [Node Feature Discovery](https://kubernetes-sigs.github.io/node-feature-discovery/)
  schedule unifand wherever the hub is detected. NFD publishes a per-device
  label; find yours with:

  ```sh
  kubectl get nodes -o json | grep -o 'usb-ff_0cf2_a105[^"]*'
  ```

  **Caveat:** the label embeds your hub's serial number, so a replacement hub
  changes the label. It's the "follows the hardware" option — we run this in
  production — but check the label after any hub swap.

If your target node carries taints (GPU nodes often do), add matching
tolerations to the overlay's `patch.yaml` (a commented example is included
in both overlays) or the daemon silently never schedules.

## CLI

```
unifand [--config PATH] [--oneshot]
```

| Flag | Default | Meaning |
|---|---|---|
| `--config PATH` | `/etc/unifand/config.toml` | Config file to load |
| `--oneshot` | off | Read sensors, print the control decision, exit. **Never touches the hub.** |

`--oneshot` loads and validates the config (identical to normal startup —
`Config::load` runs the same parse/validate path either way), discovers
sensors, computes one control decision, and prints it — no hidraw device is
opened, no HID write happens. Two uses:

- **Pre-flight sanity check** before wiring real hardware: run it locally
  (`cargo run -- --config my-config.toml --oneshot`) to confirm sensor
  discovery and see what duty your curve would produce, without a hub
  attached at all.
- **A pre-apply config-validation gate in CI/GitOps**: since it exits
  non-zero on any config error (bad TOML, an out-of-range value, a
  malformed `[rgb.fans]` entry) and never touches hardware, it's safe to run
  in a pipeline step before `kubectl apply -k` / before a Flux push —
  catches the exact class of error that otherwise only surfaces as a
  crashlooping pod after rollout.

## Configuration

One TOML file, mounted from a ConfigMap (the kustomize base generates it from
`deploy/base/config.toml`; edits roll the pod automatically). Full annotated
reference: [`examples/config.toml`](examples/config.toml). A silence-tuned
variant: [`examples/config-quiet.toml`](examples/config-quiet.toml). A
multi-signal starting point (measured curve knees, needs per-card tuning):
[`examples/config-multisignal.toml`](examples/config-multisignal.toml).

| Key | Default | Meaning |
|---|---|---|
| `poll_interval_secs` | 5 | Control-loop tick |
| `channels` | `[1, 2]` | Hub channels to drive (1-4) |
| `fallback_duty` | 60 | Duty on shutdown / total sensor loss |
| `cpu_offset` | 10.0 | Subtracted from CPU temp vs GPU temp |
| `hysteresis_c` / `min_duty_delta` | 2.0 / 5 | Change suppression |
| `max_step_per_tick` | 10 | Ramp smoothing (duty points/tick) |
| `[[curve]]` | 35→30% … 80→100% | Piecewise-linear fan curve |
| `[rgb]` | disabled in code, enabled in the shipped config | LED gradient: stops, brightness, buckets |
| `[rgb] fans_per_channel` | 6 | LED chain length declared to the hub (start-packet byte 3) — **not cosmetic**; too low leaves the tail fans on a chain dark |
| `[rgb.fans]` | empty | Optional per-channel override of `fans_per_channel`, e.g. `1 = 3` for a shorter chain on channel 1 |
| `[rgb.alerts]` | disabled in code, enabled in the shipped config | Alarm ladder thresholds, escalation interval, colors |
| `[signals]` | disabled | Multi-signal fusion (see below). Absent or `enabled = false` ⇒ behavior is exactly the legacy single-curve loop |
| `[signals.gpu_temp]` / `[signals.cpu_temp]` | enabled (inert until `[signals]` is) | Die-temp signals: optional own `curve` (falls back to the top-level `[[curve]]`), EWMA `alpha` (1.0 = off), CPU `offset_c` (10.0) |
| `[signals.gpu_power]` | disabled | Power-draw curve; `unit` = `"watts"` (default) or `"percent_tdp"` (portable); asymmetric `rise_alpha`/`fall_alpha` (0.5/0.1) |
| `[signals.thermal_margin]` | disabled | Inverted curve over headroom °C to the card's thermal limit; `alpha` 0.4 |
| `[signals.mem_temp]` | disabled | Memory-junction-temp curve — NVML doesn't expose it on most consumer cards, so it ships off |
| `[signals.throttle]` | disabled | Duty floor while the GPU reports a throttle `reasons` bit: `floor_duty` 85, `hold_secs` 30 |
| `metrics.listen` | `0.0.0.0:9877` | Prometheus endpoint |

### Multi-signal fusion

With `[signals] enabled = true`, the single temperature-to-duty curve
becomes one voice among several. Each enabled signal is read per tick,
filtered, and mapped through its own piecewise-linear curve to a candidate
duty; the applied target is the **maximum** of the candidates. Fusion is
**raise-only**: a signal can raise duty above what die temperature asks
for, but nothing can lower it below the hottest signal's demand. The
feature is off by default — with `[signals]` absent or `enabled = false`,
behavior is exactly as before: the legacy control path runs unchanged, and
the only trace in the scrape output is a constant
`unifand_throttle_floor_active 0`.

The signals, and what was actually measured about them (RTX 5090 FE,
575 W enforced limit):

- **GPU power draw** (`[signals.gpu_power]`) is genuinely independent of
  die temperature, and it *leads* it: on load release, power shed ~96% of
  its range in ~20 s while die temp had shed about half and kept decaying
  for 40+ s. That measurement justifies two design choices at once —
  raise-only fusion, and the asymmetric filter (`rise_alpha` 0.5 /
  `fall_alpha` 0.1: fast attack, slow release). A symmetric filter would
  have dropped the fans the moment the job ended, with ~68 °C still
  sitting in the heatsink. `unit = "percent_tdp"` denominates the curve in
  percent of the card's enforced power limit and is portable across cards;
  `"watts"` is absolute and must be retuned per card.

  There is a second, blunter argument for weighting power at all, measured
  across a real batched-inference release: **die temperature is a volatile
  observable.**
  It swung 82 → 69 → 78 → 68 °C inside 30 s as batches started and stopped,
  which moved its candidate duty by up to **32 points in a single 5 s tick**
  (mean 8.4). Power's candidate moved at most 6 points a tick (mean 3.9) over
  the same window. Fed straight to the fans, the die-temp signal is what you
  would *hear*; the fused output instead descended monotonically — 100, 93,
  88, 83, 77, 71, 66, 60, 52, 44, 39 — never exceeding the slew limit. Across
  that release the power candidate sat above the die-temp candidate on 17 of
  21 ticks, by a mean of 14 duty points and a peak of 29: fans kept purging
  the heatsink while the die had already cooled.
- **Thermal margin** (`[signals.thermal_margin]`) is headroom in °C to the
  card's own thermal limit, with an inverted curve (less headroom → more
  fan). Be clear about what this is: on a card with a static thermal limit
  it is **not** an independent observable — the measured card's limit is a
  hard constant 90 °C, so margin is exactly `90 − die_temp`, an affine
  reparameterization of die temperature, not new information. Its real
  value is **portability and self-calibration**: one margin curve is
  correct on any card without knowing that card's limit, which an
  absolute-temperature curve can never be. That's exactly what a shipped
  default needs, and no more than that.
- **Memory junction temperature** (`[signals.mem_temp]`) is unavailable on
  most consumer cards: on the measured card NVML returns `NotSupported`,
  `nvidia-smi` reports N/A, and DCGM reported 0 across 6875 samples over 7
  days (that zero was DCGM's encoding of "unsupported", not a reading). It
  ships disabled and only works where NVML actually exposes it. The only
  memory-overtemp signal reachable on consumer silicon is
  `SW_THERMAL_SLOWDOWN` — which fires on GPU *or* memory over-temp — and
  that is why the throttle floor below exists.
- **The throttle floor** (`[signals.throttle]`) raises applied duty to at
  least `floor_duty` while the GPU reports any configured throttle reason,
  and holds it for `hold_secs` after the last assertion (anti-flap). It is
  a safety net, not a routine control path: no thermal throttle bit was
  ever observed asserting, even at 84 °C with 5 °C of headroom.
  `sw_power_cap` is deliberately excluded from the default `reasons` —
  it was measured asserting continuously (0x4) throughout a sustained
  load and 0x0 at idle, so including it would pin the fans at the floor
  during every job (the daemon warns if you configure it anyway).

Filtering: every reading passes a plausibility guard (a spurious 0 or an
absurd value is treated as absent, not smoothed in), then a per-signal
EWMA (`alpha`, 1.0 = no smoothing; power uses the asymmetric pair). An
absent signal simply doesn't produce a candidate — it never drags duty
down and never triggers alarms.

One legacy knob changes meaning: `hysteresis_c` is honored in legacy mode
but **inert in fusion mode**. Fusion decides in duty space, not
temperature space, so change suppression degenerates to the
`min_duty_delta` check (slew limiting via `max_step_per_tick` still
applies).

### The alarm ladder

With `[rgb.alerts]` enabled, the LEDs stop being just a gauge and become an
alarm. Highest active state wins; de-escalation waits out a cooldown so the
display never flaps:

| State | Trigger | LEDs |
|---|---|---|
| Normal | — | static gradient color (duty-mapped) |
| Sustained hot | control temp ≥ `sustained_hot_c` continuously for `sustained_after_secs` | slow breathing in the current gradient color |
| Near limit | within `near_limit_margin_c` of the curve's top temp, **or** duty pinned at 100%, **or** (with `[signals]` enabled) thermal margin ≤ `near_limit_margin_c` or active thermal throttling | red breathing, stepping through four escalating pulse rates — one step per `escalate_every_secs`, capping at the fastest |
| Fault | all sensors lost (fallback duty active) | orange/red runway — deliberately unlike the thermal states |

The current rung is exported as `unifand_led_state`, so your monitoring can
alert on the same thing your case is showing.

The two signal-driven Near-limit triggers are live whenever `[signals]
enabled = true` and the hardware exposes them — independent of whether the
throttle *floor* is enabled. Disabling `[signals.throttle]` turns off the
duty floor, not the alarm: sustained thermal throttling still drives the
LEDs to Near-limit.

## Metrics

| Metric | Labels | Meaning |
|---|---|---|
| `unifand_temp_celsius` | `source=gpu\|cpu\|control` | Sensor and control temperatures |
| `unifand_duty_percent` | `channel` | Last duty written |
| `unifand_fan_rpm` | `channel` | Fan speed read back from the hub |
| `unifand_degraded` | — | 1 when the GPU sensor is unavailable |
| `unifand_fallback_active` | — | 1 when all sensors are lost and `fallback_duty` is being forced |
| `unifand_hub_present` | — | 1 when the SL V2 hub is currently open and initialized (0 during startup hub-wait or a lost hub) |
| `unifand_hid_errors_total` | `kind=write\|read` | Fan-protocol write/read failures |
| `unifand_rgb_errors_total` | — | LED write failures (never affect fan control) |
| `unifand_led_state` | — | Alarm ladder rung: 0 normal, 1 sustained-hot, 2 near-limit, 3 fault |
| `unifand_signal_value` | `signal`, `unit=celsius\|watts` | Conditioned (smoothed) value of each available input signal |
| `unifand_signal_candidate_duty_percent` | `signal` | Duty each available signal's curve commands this tick |
| `unifand_control_signal` | `signal` | One-hot: 1 for the signal driving the applied duty, 0 for the others (all 0 on a floor-only tick) |
| `unifand_gpu_power_limit_watts` | — | Enforced GPU power limit — join against `unifand_signal_value{signal="gpu_power"}` for percent-of-limit dashboards regardless of curve unit |
| `unifand_throttle_active` | `reason=sw_thermal\|hw_thermal\|hw_power_brake\|sw_power_cap` | NVML throttle reason currently asserted |
| `unifand_throttle_floor_active` | — | 1 while the throttle floor is raising applied duty (including its hold window) |
| `unifand_signal_errors_total` | `signal` (incl. `throttle`) | Real read failures per signal; `NotSupported` is not counted — it means the signal is absent |
| `unifand_last_tick_timestamp_seconds` | — | Unix time the control loop last completed a tick; backs `/healthz` |
| `unifand_build_info` | `version` | Always 1; join on `version` to identify the running build |

The fusion families only appear with `[signals] enabled = true` (except
`unifand_throttle_floor_active`, a plain scalar that always renders and
reads 0 in legacy mode), and an absent signal emits **no** series (no
`NaN`, no stale zero) — the
`unifand_signal_*` and `unifand_control_signal` series are replaced
wholesale each tick and describe the latest tick only, so a signal that
goes away disappears from the scrape rather than lingering at its last
value.

`/healthz` (same port as `/metrics`) returns 200 while `now - unifand_last_tick_timestamp_seconds <= 3 * poll_interval_secs`, else 500 — this is what the DaemonSet's livenessProbe checks, so a wedged control loop (not just an unreachable metrics port) gets restarted.

### Monitoring

[`contrib/`](contrib/) has a ready-to-import Grafana dashboard, a
Prometheus/VictoriaMetrics alerting-rules file (nine rules covering the
failure modes above — hub missing, loop stuck, all sensors lost, fans
stalled, sustained thermal throttling, etc.), and
scrape examples for prometheus-operator, the VictoriaMetrics operator, and
plain annotation-based discovery. See [`contrib/README.md`](contrib/README.md).

## Troubleshooting

**`ERROR: init 250 result=11` glued to the first log line (Talos Linux).**
Not from unifand — that line prints before the daemon's first instruction
executes. It's the NVIDIA container toolkit's driver-init probe failing an
`ld.so.cache` lookup during container setup: on Talos the glibc ldcache is
read-only and doesn't include the staged NVIDIA libraries, so the probe
errors (and its stderr shares the container's log pipe), then everything
resolves via direct loader paths anyway. Harmless as long as the next lines
show `gpu(nvml)=true` — which they will. Tracked upstream in
[siderolabs/extensions#940](https://github.com/siderolabs/extensions/issues/940),
closed by a migration to CDI — the line persists for any cluster still using
the legacy `runtimeClassName: nvidia` + env-var flow (as this project's
`nvidia-gpu` component does) and goes away only if your cluster adopts the
CDI device flow. (The
severe form of the same bug segfaults GPU containers on older
`nvidia-container-toolkit-lts` versions — if unifand *crashes* at startup on
Talos rather than just logging this line, read that issue.)

**DaemonSet shows `0/0` pods, no errors, no events.** The `nodeSelector`
doesn't match any node — almost always the `overlays/nodename` overlay
applied with its placeholder `my-node` hostname unedited (see Scheduling
above), or an `overlays/nfd-usb` label whose serial doesn't match your hub
(see NFD drift below). Check with:

```sh
kubectl get daemonset -n unifand unifand -o wide
kubectl get nodes -o name   # confirm the name you targeted actually exists
```

**Hub not found.** As of v0.4.0 this is *not* fatal — the daemon starts,
serves `/metrics` and `/healthz`, and retries `find_hidraw()` every 5s
(logging a warning every 30s) until the hub appears or it's shut down.
Watch `unifand_hub_present` (0 while waiting) and:

```sh
kubectl -n unifand logs ds/unifand | grep -i hub
```

If it never flips to 1: confirm the hub is physically attached to *this*
node (USB ID `0cf2:a105` — `lsusb` on the host, or `ls /sys/class/hidraw`
inside a debug pod with the same `/dev` hostPath), and that no other
process on the node has the device open.

**Config validation errors.** `Config::load` fails fast with a descriptive
`anyhow` message (e.g. `curve temps must be strictly increasing`,
`rgb.fans[3] = 7 out of range 1..=6`) — check `kubectl logs` /
`kubectl describe pod` for the crashlooping pod's last log line, which
contains the reason verbatim. Better: catch it before it ever reaches the
cluster with `--oneshot` (see the CLI section above) as a pre-apply gate.
Unknown/misspelled keys (e.g. `fallbackduty` instead of `fallback_duty`)
don't fail validation — by design, so config rolls back cleanly across
versions — but are logged at `warn`: `ignoring unknown config key: <path>`.
Grep your pod logs for `ignoring unknown config key` after any config edit.

**NFD label drift after a hub swap.** `overlays/nfd-usb`'s node label
embeds the hub's serial number (`usb-ff_0cf2_a105_<serial>.present`). A
replacement hub gets a new serial, so the DaemonSet silently stops matching
any node — same symptom as the first item above. Re-run
`kubectl get nodes -o json | grep -o 'usb-ff_0cf2_a105[^"]*'` after any hub
swap and update `deploy/overlays/nfd-usb/patch.yaml`.

**Some fans in a chain stay dark.** `[rgb] fans_per_channel` (or a
per-channel `[rgb.fans]` entry) declares the LED chain length to the hub —
too low and the tail fans on that channel never get a color write. Set it
to (or above) your actual chain length per channel; 6 is the hub's max and
safe to leave as the default even on shorter chains.

**arm64: fans permanently pinned at `fallback_duty`, LED runway (Fault
state) from the first tick.** This is expected, not a bug: there's no NVML
on ARM, and the CPU sensor is hardcoded to `k10temp` (AMD, x86-only) — see
Limitations. Both sensors read `None` forever, so `unifand_degraded=1`,
`unifand_fallback_active=1`, and `unifand_led_state=3` from startup. The
behavior is safe (a constant duty is a sane dumb-fan-controller fallback)
but closed-loop control simply isn't available on arm64 today.

## How it works

The SL V2 hub is a USB HID device. unifand finds it by scanning
`/sys/class/hidraw` for USB ID `0cf2:a105` and talks to `/dev/hidrawN`
directly:

- **Fan control**: 4-byte reports set per-channel mode and duty; RPM is read
  back via the `HIDIOCGINPUT` ioctl. On startup the hub's motherboard-PWM
  sync is disabled so duty commands stick.
- **LEDs**: 353-byte reports (start → color → commit) set a static color per
  channel. The wire format is R,**B**,G — pinned by tests so nobody "fixes"
  it. Colors are written only when the duty's quantized bucket changes, and
  strictly after a successful duty write: the LED path can fail forever
  without affecting fan control.
- **Recovery**: repeated HID failures (writes *or* reads) trigger
  re-enumeration — reopen the device, re-initialize modes, re-assert the
  current duty. Validated behaviors: 100% duty ≈ 1995 rpm against the fans'
  2000 rpm spec; SIGTERM lands the fallback duty before exit.

### Protocol provenance

The wire protocol was reimplemented in Rust from publicly documented
byte layouts established by the
[liquidctl](https://github.com/liquidctl/liquidctl),
[uni-sync](https://github.com/EightB1ts/uni-sync), and
[OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) projects — enormous
credit to those communities for the reverse engineering. No code was copied
from any of them; unifand's implementation is original and MIT-licensed.

## Building

Toolchain is pinned via [mise](https://mise.jdx.dev):

```sh
mise install
mise run test      # runs the full unit-test suite
mise run release   # optimized binary
docker build -t unifand .   # rust:slim builder → distroless/cc runtime
```

The runtime image must stay glibc-based (`distroless/cc`) — NVML is loaded
via `dlopen` at runtime from the NVIDIA container toolkit's injected
libraries, which rules out musl/scratch. CPU-only builds would work static,
but we ship one image.

## Limitations & roadmap

- SL V2 hub only (`0cf2:a105`); one hub per node.
- One duty and one color for all configured channels (per-channel curves
  when someone has a real use case).
- CPU sensor is `k10temp` (AMD). Intel `coretemp` support is a small,
  welcome PR.
- arm64: no NVML and no `k10temp` on ARM SoCs, so today the arm64 image
  always runs sensor-less — permanent `fallback_duty` and a Fault LED
  signature (see Troubleshooting). Safe, but not closed-loop.
- Thermal margin is computed as `temperature_threshold(GpuMax) − die_temp`.
  NVML's dedicated T.Limit fields (193/194/196) are unreadable through
  `nvml-wrapper` 0.12.1 — the driver reports a value type the crate's
  `SampleValue` enum can't decode. A crate limitation, not a hardware one
  (`nvidia-smi` reads them fine); the computed path is equivalent on
  static-limit cards.
- LED animations are used only where they carry meaning (the alarm ladder);
  decorative effects (rainbow, meteor-for-fun) are deliberately out of scope —
  it's a gauge, not a light show.

## License

[MIT](LICENSE). Not affiliated with or endorsed by Lian Li.
