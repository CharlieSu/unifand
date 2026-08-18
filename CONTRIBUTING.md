# Contributing to unifand

Thanks for considering a contribution. unifand is a small daemon with a
correspondingly small bar for entry — this doc covers dev setup, the gates
your PR needs to pass, and one policy specific to a hardware-protocol
project: changes to the wire format need more than a green test suite.

## Dev setup

The Rust toolchain is pinned via [mise](https://mise.jdx.dev) — `mise.toml`
is the source of truth for the exact version:

```sh
mise install         # installs the pinned Rust toolchain from mise.toml
mise run build        # cargo build
mise run test         # cargo test
mise run fmt          # cargo fmt --check
mise run clippy       # cargo clippy --all-targets -- -D warnings
mise run release      # cargo build --release
```

No hardware is required to build, test, or run `--config file --oneshot`
(see the README's CLI section) — the daemon only opens a real hidraw device
once it leaves oneshot mode and finds a hub. Unit tests (`mise run test`)
exercise the HID/RGB packet builders, config validation, curve
interpolation, the alarm-ladder state machine, and metrics rendering
entirely in-process; none of them touch `/dev`.

## Gates

CI (`.github/workflows/ci.yaml`) runs, and your PR needs to pass, all of:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --locked`
- `cargo audit` and `cargo deny check` (RustSec advisories + license/bans
  policy in `deny.toml`)

Run the first four locally with `mise run fmt`, `mise run clippy`,
`mise run test` before pushing — much faster than round-tripping through
CI. `cargo audit`/`cargo deny` need `cargo-audit`/`cargo-deny` installed
(`cargo install cargo-audit cargo-deny`, or `taiki-e/install-action` locally
if you have it) — optional locally, CI will catch it either way.

## Protocol-change policy

unifand's HID/RGB byte layouts (`src/hid.rs`, `src/rgb.rs`) were
reverse-engineered against real hardware — see the README's "Protocol
provenance" section. Getting a byte layout wrong doesn't just fail a test:
it can silently misdrive real fans and LEDs (the chain-length bug fixed in
v0.4.0, where a wrong byte left two fans on a six-fan chain permanently
dark, is exactly this class of mistake — it shipped with a passing test
suite because nothing in CI can see an actual LED).

**Any change to an existing byte layout, packet structure, or protocol
constant requires one of:**

1. **Hardware validation** — you ran the change against a real SL V2 hub
   and fans and can describe what you observed (which LEDs lit, what color,
   measured RPM vs. commanded duty, etc.) in the PR description; or
2. **An issue discussion first** — open an issue describing the proposed
   change and the reasoning (e.g. a new hub variant's packet format from
   public reverse-engineering sources) before sending a PR, so it can be
   reviewed by someone who *can* validate on hardware before it merges.

New protocol support (e.g. one of the unsupported UNI HUB generations in
the README's "Supported hardware" table) falls under the same policy — a
PR that "looks right" against documentation alone, with no hardware
validation and no prior issue discussion, will be asked for one of the two
above before merge.

Everything else (config parsing, the control loop, metrics, sensors,
deploy manifests, docs) doesn't need hardware to validate — the existing
unit tests are the bar.

## PR expectations

- Keep PRs scoped to one change; a protocol fix and an unrelated refactor
  in the same PR make the hardware-validation story harder to review.
- Add or update tests for behavior changes. `cargo test` should meaningfully
  cover what changed, not just still pass.
- If you touch `README.md`'s Metrics table, `Config` defaults, or the CLI
  flags, make sure the table/section stays in sync with the code — these
  went stale in the past (see `CHANGELOG.md`) and are exactly the kind of
  drift a reviewer will ask about.
- Small, focused PRs get reviewed faster than large ones. If a change is
  going to be large (new hub-generation support, a new sensor backend),
  open an issue first to align on approach before investing the time.
