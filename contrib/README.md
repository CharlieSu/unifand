# contrib/

Optional, bring-your-own-stack observability artifacts. Nothing here is
wired into `deploy/` or required to run unifand — it's a starting point for
whatever Prometheus-adjacent stack you already run.

```
contrib/
├── grafana/
│   └── unifand-dashboard.json   # raw importable Grafana dashboard
├── prometheus/
│   └── alerts.yaml              # alerting rules (generic `groups:` shape)
└── scrape/
    ├── podmonitor.yaml          # prometheus-operator
    ├── vmpodscrape.yaml         # VictoriaMetrics operator
    └── annotations.md           # plain Prometheus, no operator
```

## Dashboard

`grafana/unifand-dashboard.json` is a raw, ecosystem-neutral Grafana
dashboard — fan RPM, duty %, temperatures, the alarm-ladder state, error
rates, and a build-info panel, plus a multi-signal fusion row: per-signal
candidate duty overlaid (shows which signal is winning), the
`unifand_control_signal` one-hot (who is driving the fans right now), GPU
power against its enforced limit, throttle-reason bits with the duty
floor, and per-signal read-error rates. The fusion panels are simply
empty on a daemon running with `[signals]` disabled — those series aren't
emitted at all. It carries an `__inputs` block for a
`${DS_PROMETHEUS}` datasource variable, so Grafana's import flow will prompt
you to pick a datasource rather than assuming a "default" one exists.

Every panel aggregates `by (node, ...)` rather than querying the raw series.
unifand is a DaemonSet, so without that a pod restart splits every series in
two: the graphs grow duplicate legend entries and disjoint segments with gaps,
and each single-value panel sprouts an extra box per pod in the time window.
Aggregating collapses that churn into one continuous series per node.

For the `node` grouping to mean anything, your scrape has to attach a node
label — with the Prometheus or VictoriaMetrics operators that is one relabel
rule on the pod scrape:

```yaml
relabelConfigs:
  - sourceLabels: [__meta_kubernetes_pod_node_name]
    targetLabel: node
```

Without it the dashboard still works and still de-duplicates pod churn; the
`by (node)` clause just collapses to a single empty-node group, which merges
nodes together if you run unifand on more than one.
Works against Prometheus, VictoriaMetrics (its Grafana datasource plugin
identifies as type `prometheus`), Thanos, or Mimir.

**UI import** (simplest): Grafana → Dashboards → New → Import → upload
`unifand-dashboard.json` or paste its contents → select your Prometheus
datasource when prompted.

**Provisioning directory**: drop the file into Grafana's
[dashboard provisioning](https://grafana.com/docs/grafana/latest/administration/provisioning/#dashboards)
path (e.g. `/etc/grafana/provisioning/dashboards/`, or a ConfigMap mounted
there if you run Grafana on Kubernetes). The `${DS_PROMETHEUS}` input is
resolved at import time in the UI; when provisioning as a file instead, set
`__inputs`' value directly or point the provisioning config's
`options.path` at a copy with the datasource UID substituted in.

**grafana-operator users**: this repo does not ship a
`GrafanaDashboard` CR — that CRD (`grafana.integreatly.org/v1beta1`) is a
specific operator's provisioning mechanism, not a Grafana convention, and
most installs (kube-prometheus-stack's sidecar, Helm chart ConfigMap
provisioning, or a plain UI import) never touch it. If you do run
grafana-operator, wrap the JSON yourself:

```yaml
apiVersion: grafana.integreatly.org/v1beta1
kind: GrafanaDashboard
metadata:
  name: unifand
  namespace: monitoring
spec:
  instanceSelector:
    matchLabels:
      dashboards: "grafana"
  folder: Hardware
  json: |
    <paste unifand-dashboard.json here, minus __inputs/__requires>
```

The JSON in `grafana/unifand-dashboard.json` stays the canonical source —
if you keep a CR wrapper like the above, keep it manually in sync (or
generate it with a small script); it isn't checked in here because a
generic `${DS_PROMETHEUS}`-input JSON is the strictly more portable
artifact, and CR wrapping is a maintainer-specific choice (folder name,
instance selector labels) that a stranger importing this dashboard is
unlikely to already match.

`uid`, `tags`, and the dashboard `description` are sensible shipped
defaults, not load-bearing — edit freely.

## Alerts

`prometheus/alerts.yaml` is a plain `groups: [...]` rules body — the same
shape prometheus-operator's `PrometheusRule`, the VictoriaMetrics
operator's `VMRule`, and standalone Prometheus `rule_files:` all accept.
The file's header comment shows how to wrap it in either CRD envelope.

Nine rules: `UnifandAbsent` (scrape target gone), `UnifandStuck` (control
loop wedged — the same signal `/healthz` uses), `UnifandHubMissing` (no SL
V2 hub found), `UnifandDegraded` (running CPU-only), `UnifandFallbackActive`
(all sensors lost — the single most important one to page on),
`UnifandHidWriteErrors` (duty writes failing), `UnifandFanStalled`
(commanded duty but near-zero RPM), and two that only fire on a daemon
running with `[signals]` enabled: `UnifandThermalThrottle` (a thermal
throttle bit held for minutes — never observed in normal operation, so
treat it as cooling genuinely failing; `sw_power_cap` is deliberately not
matched, it asserts throughout any sustained load) and
`UnifandSignalErrors` (`unifand_signal_errors_total` increasing — a
sensor that should work is failing repeatedly; unsupported sensors are
silently absent and never counted). Every `expr` was checked against the
metric names actually emitted by `src/metrics.rs`.

**Loading it:**
- prometheus-operator: wrap in a `PrometheusRule` (see the file's header
  comment) and apply — make sure its labels match your `Prometheus`
  object's `ruleSelector`.
- VictoriaMetrics operator: wrap in a `VMRule` and apply.
- Standalone Prometheus: reference the file from a `rule_files:` glob in
  `prometheus.yml` — no wrapping needed, it's already the right shape.

No `promtool` was available in the environment this was authored in to
lint the file directly; it was validated with a strict YAML parse and a
manual structural check (every `alert`/`expr`/`for`/`annotations` field
present, every metric name cross-checked against `src/metrics.rs`).
Run `promtool check rules contrib/prometheus/alerts.yaml` yourself before
relying on it in CI, if you have it available.

## Pick your scrape mechanism

| You run... | Use |
|---|---|
| prometheus-operator (kube-prometheus-stack, etc.) | `scrape/podmonitor.yaml` |
| VictoriaMetrics operator | `scrape/vmpodscrape.yaml` |
| Plain Prometheus, no operator | `scrape/annotations.md` (pod annotations + matching `relabel_configs`) |

A `PodMonitor`/`VMPodScrape` is used rather than a `ServiceMonitor` because
unifand has no `Service` in front of it — it's a privileged hostPath
DaemonSet, one pod per node, each scraped independently; there's nothing to
load-balance. Both examples match pod label `app.kubernetes.io/name:
unifand`, namespace `unifand`, and the DaemonSet's named container port
`metrics` (9877).
