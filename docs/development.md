# Development

## Build and test

```sh
cargo test            # unit tests; the FLAC round-trip test needs ffmpeg and skips without it
cargo run             # serves on :8337, data in ./data (XMUSIC_PORT, XMUSIC_DATA)
docker compose up --build
```

The UI assets (daisyUI, Tailwind browser build, Chart.js) are pinned in [assets/](../assets) and compiled into the
binary, so the dashboard works offline.

## Code layout (src/)

| File | Role |
|------|------|
| `main.rs` | CLI dispatch; starts the scheduler, watcher and web server |
| `db.rs` | SQLite schema, migrations, settings (one JSON blob), track queries |
| `auth.rs` | Users, sessions, login throttle |
| `tags.rs` | Tag read/write. FLAC uses native Vorbis comments, other formats use lofty's generic tag |
| `scan.rs` | Incremental library walk and album folder logic |
| `remote.rs` | All HTTP calls, with pacing and counters |
| `matcher.rs` | Candidate scoring, track alignment, proposals, rules |
| `apply.rs` | Every file write, undo and trash |
| `job.rs` | The background job, phases, progress, scheduler, notifications |
| `watch.rs` | Watch mode |
| `metrics.rs` | Prometheus exposition |
| `web.rs` | Routes and JSON API |
| `dashboard.html` | The whole UI, vanilla JS |

[CLAUDE.md](../CLAUDE.md) lists the invariants the code relies on (single write path, one write per file, MusicBrainz
rate limit, cancel points, and so on). Read it before changing anything that writes files.

## Adding a setting

1. Add the field and its default to `Settings` in `db.rs`.
2. Add a form field in `dashboard.html` (the `txt`/`chk`/`list` helpers, keyed by the field name).
3. Read it where it is used through `Settings::load`.

Schema changes go in `db::migrate`, keyed on `PRAGMA user_version`; never edit `SCHEMA` for an existing table.

## Build and deploy to a home server

`tools/build.sh` builds the image and pushes it to a registry; `tools/deploy.sh` pulls it on the server over SSH and
recreates the container. Both are plain shell with variables you can override (`NUC`, `DIR`, `SERVICE`); edit the
defaults for your own host and registry.
