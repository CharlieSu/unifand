# For: plain Prometheus (no operator) — pod annotation scraping

`prometheus.io/scrape`, `prometheus.io/port`, and `prometheus.io/path` are
**not a Prometheus standard** — there's no CRD or spec behind them. They're a
long-standing *convention* that a Prometheus install can opt into by writing
its own `kubernetes_sd_configs` relabeling rules (this is what the
`kube-prometheus`/`prometheus-community` "annotation-based discovery"
examples do, and what the deprecated `prometheus.io/*` annotations in a lot
of older Helm charts assume). If your Prometheus doesn't have those
relabeling rules configured, these annotations do nothing — they're inert
metadata.

Add this to the unifand DaemonSet's pod template (e.g. via a Kustomize
patch on `deploy/base/daemonset.yaml`'s `spec.template.metadata.annotations`):

```yaml
spec:
  template:
    metadata:
      annotations:
        prometheus.io/scrape: "true"
        prometheus.io/port: "9877"
        prometheus.io/path: /metrics
```

If your Prometheus config doesn't already relabel on these, add something
like this to `prometheus.yml`'s `kubernetes_sd_configs` job for pods:

```yaml
relabel_configs:
  - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_scrape]
    action: keep
    regex: "true"
  - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_path]
    action: replace
    target_label: __metrics_path__
    regex: (.+)
  - source_labels: [__address__, __meta_kubernetes_pod_annotation_prometheus_io_port]
    action: replace
    regex: ([^:]+)(?::\d+)?;(\d+)
    replacement: $1:$2
    target_label: __address__
```

If you're running prometheus-operator or the VictoriaMetrics operator
instead, use `podmonitor.yaml` or `vmpodscrape.yaml` in this directory —
they don't need any of the above.
