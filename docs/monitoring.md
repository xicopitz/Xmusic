# Monitoring

## Prometheus

`GET /metrics` serves Prometheus text format and needs no login. Add a scrape job:

```yaml
- job_name: xmusic
  scrape_interval: 60s
  static_configs:
    - targets: ["<xmusic-host>:8337"]
```

Gauges are read from the database at scrape time. Counters live in memory and reset on restart.

| Metric | Meaning |
|--------|---------|
| `xmusic_tracks`, `xmusic_albums`, `xmusic_size_bytes` | Library size |
| `xmusic_tracks_by_quality` | Tracks per quality class |
| `xmusic_tracks_missing` | Tracks missing a field (`field="cover"`, `"title"`, …) |
| `xmusic_jobs_total` | Finished jobs by `kind` and `result` |
| `xmusic_files_written_total`, `xmusic_files_restored_total` | Files changed, and restored by undo, since start |
| `xmusic_last_job_duration_seconds` | Duration of the last job |
| `xmusic_remote_requests_total` | Calls to MusicBrainz, AcoustID, Cover Art Archive, LRCLIB, Gotify, Navidrome |

Labels only hold library names, formats, statuses and job kinds.

## Grafana

Import [grafana/xmusic-dashboard.json](../grafana/xmusic-dashboard.json) (Dashboards → New → Import) and pick your
Prometheus data source from the dashboard's variable.
