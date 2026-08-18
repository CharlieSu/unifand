# Security Policy

## Reporting a vulnerability

Please report security issues privately via
[GitHub's private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability):
open a draft security advisory on this repository (Security tab →
"Report a vulnerability") rather than a public issue. This lets us discuss
and fix the problem before it's public, and GitHub will offer to credit you
in the resulting advisory if you'd like.

If you're unsure whether something rises to the level of a security issue
(vs. an ordinary bug), err on the side of reporting it privately — it's
easy to downgrade a private report to a public issue, much harder to do the
reverse.

## Threat model

unifand runs as a **privileged pod with a `hostPath` mount of the entire
`/dev` tree** (`deploy/base/daemonset.yaml`). This is broader host access
than the daemon strictly uses — it only ever opens `/dev/hidraw*` — but
Kubernetes has no lighter-weight mechanism for granting access to a device
node whose number isn't known ahead of time (the hidraw index shifts across
USB re-enumeration): a non-privileged container can't open a device node
seen through a `hostPath` mount at all, because the kernel's device cgroup
denies it regardless of file permissions or capabilities — this is a
device-allowlist problem, not something `CAP_*` grants solve. Two lighter
alternatives exist (a device plugin/Akri instance exposing `/dev/hidrawN`
as an allocatable resource, or CDI injection) but both would break the
daemon's own re-enumeration recovery, which re-discovers the hub by
VID:PID after a hidraw index change — a statically-mounted device path
can't do that. "Privileged, one pinned node, dedicated namespace" is the
current tradeoff.

**What a compromise of the unifand container yields:** effectively
root-equivalent access to the node it runs on. `privileged: true` disables
the container's seccomp and capability confinement entirely, and the
`hostPath: /dev` mount reaches every device node on the host — including
block devices, other USB peripherals, and (on GPU nodes) the GPU device
nodes themselves. A container escape here is not contained to "control of
some fans"; treat it as node-level compromise. This is the direct
consequence of the hardware-access requirement above, not a bug — but it's
the reason unifand should run in its own namespace, pinned to only the
node(s) that actually have a hub attached (see README "Scheduling"), rather
than as a cluster-wide DaemonSet on nodes that don't need it.

**Mitigations shipped, given that starting point:**

- `readOnlyRootFilesystem: true` — the container writes no files (config is
  a read-only ConfigMap mount; hidraw/NVML I/O go through `/dev` and
  `dlopen`'d libraries, not the container filesystem), so a compromised
  process can't persist anything to the image layer.
- `automountServiceAccountToken: false` — the daemon never calls the
  Kubernetes API; the default ServiceAccount token would be pure unused
  attack surface on an already-privileged pod, so it isn't mounted at all.
- A `NetworkPolicy` component (`deploy/components/network-policy/`, opt-in)
  restricts ingress on the metrics port (`:9877`) to a namespace you
  label — the metrics body itself leaks nothing sensitive, but an open,
  unauthenticated port on a privileged pod is worth closing by default for
  anyone who wants it.
- A dedicated `unifand` namespace with the `privileged` Pod Security
  Standard scoped to it — the requirement doesn't leak into any other
  namespace's admission policy.
- `priorityClassName: unifand-critical` isn't a security control, but is
  worth noting here too: it keeps the daemon from being evicted ahead of
  ordinary workloads, which matters for the thermal-safety invariant this
  whole privileged-pod tradeoff exists to serve in the first place.
- Related honesty note (not a security control, but relevant to the same
  "what happens on pod death" question): the fallback-duty invariant itself
  only holds for SIGTERM-driven termination, not a hard kill — see the
  README's fail-safe section for the boundary and its mitigations.

None of the above narrows the blast radius of an actual container escape —
only a device plugin/CDI redesign would do that, and it's on the roadmap as
a "someday, if the re-enumeration problem gets solved" item, not a v0.4.0
commitment. If your threat model doesn't tolerate a privileged pod at all,
unifand is not the right tool today.
