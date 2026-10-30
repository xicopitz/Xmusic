# xmusic

Self-hosted music metadata fixer: a Rust binary that serves a web dashboard (daisyUI) and runs background jobs. These scan libraries, match albums against MusicBrainz/AcoustID, and write tags, cover art (Cover Art Archive) and lyrics (LRCLIB).

## Commands
- `cargo test`: unit tests. The FLAC round-trip test in `apply.rs` needs `ffmpeg`; it skips without it.
- `cargo run`: serves on :8337 with data in `./data` (`XMUSIC_PORT`, `XMUSIC_DATA`).
- `docker compose up --build`: production-style run; music is mounted at `/music`.
- `xmusic reset-password <user>`: reads the new password from stdin.

## Modules (src/)
- `main.rs`: CLI dispatch; starts the scheduler thread and the web server.
- `db.rs`: SQLite schema, `Settings` (a single JSON blob with serde defaults), track queries.
- `auth.rs`: argon2 users, sha256-hashed session tokens, per-IP login throttle.
- `tags.rs`: tag read/write. FLAC uses native Vorbis comments so custom keys work; other formats use lofty's generic tag.
- `scan.rs`: incremental walk (mtime+size), parallel tag reads with rayon. `album_dir` folds disc subfolders (`CD1/`, `Disc 2/`) into their parent album.
- `remote.rs`: all HTTP calls (MusicBrainz, Discogs, AcoustID, CAA, LRCLIB, Gotify, Navidrome `startScan`).
- `matcher.rs`: candidate scoring, track alignment, building proposals, custom tags and rewrite rules.
- `apply.rs`: every file write (`write_with_undo`). The album pass (`albums`) fetches covers and lyrics per album, then writes each track once. `Pass` picks which parts run (full run: all; the Jobs page can run covers or lyrics alone). Also undo and trash.
- `job.rs`: the single background job, phases, progress/ETA, cron scheduler, notifications (linking to `public_url`), and the Navidrome rescan after any job that changed files.
- `watch.rs`: watch mode. inotify on enabled libraries, 30 s debounce, then a scan or full run. `watch::reload()` after any library/settings change.
- `metrics.rs`: Prometheus text exposition for `GET /metrics`. Gauges are read from SQLite at scrape time, plus in-memory counters (`remote()`, `FILES_WRITTEN`, `FILES_RESTORED`).
- `health.rs`: the library health report (`GET /api/health`), computed from the tracks table only.
- `web.rs`: tiny_http routes and the JSON API, including the library browser, album detail, cover serving and duplicate finder.
- `dashboard.html`: the whole UI (vanilla JS). `assets/` holds pinned daisyUI, Tailwind browser and Chart.js, compiled into the binary.

## Invariants
- Never write to a music file except through `apply::write_with_undo`, which logs the original values first, and only for approved proposals, auto-approved ones, or explicit rule/cover/lyrics jobs.
- A run writes each file at most once: `apply::albums` gathers approved tags, the cover and lyrics, then makes one `write_with_undo` per track. Don't add a pass that writes files on its own. On rclone and SMB mounts every write re-uploads the whole file, and lofty rewrites the whole FLAC anyway.
- `write_with_undo` skips files whose values already match, and updates the row via `scan::store` without re-reading the file. Every extra open costs on a network mount.
- File I/O (scan reads, writes) runs in `Settings::io_pool()`, sized by `io_threads`. Don't use rayon's global pool for file work.
- MusicBrainz is limited to 1 req/s by the global slot in `remote.rs`. Don't bypass it or parallelize MB calls. Discogs has its own slot (25/min without a token, 60 with).
- Discogs is a second source: `matcher::match_album` asks it only when nothing scores above `match_threshold`. Its release ids are `discogs:<n>` (`matcher::DISCOGS`); a Discogs match writes `DISCOGS_RELEASE_ID` and never `MUSICBRAINZ_*`. "Has a release id" means either one: use `db::HAS_RELEASE` in SQL.
- Jobs never wait for a human. Low-confidence albums become `review`/`manual` proposals and are reported when the run ends.
- Settings, libraries and users are admin-only (`web.rs` has an `admin_only` guard arm; user routes go above it).
- Backup and restore (`web::files`) stream whole files and skip the JSON-only body check, so they check the admin session themselves. Restore goes through `db::restore` (SQLite backup API into the live database, after keeping `xmusic-before-restore.db`); never swap the database file under a running server.
- An album is the set of tracks in one directory (`tracks.dir`), where a disc subfolder counts as its parent's directory. Compute it only with `scan::album_dir`.
- Full runs only match albums where no track has a release id (`job::NEEDS_MATCH`); an unaligned bonus track must not re-queue an applied album. A re-match (`job::Kind::Rematch`) also scores the album's own release (`matcher::match_album` picks up `MUSICBRAINZ_ALBUMID` or `DISCOGS_RELEASE_ID`), always lands in Review, never auto-applies, and is marked applied when nothing differs.
- A cover.jpg/folder.jpg in the album folder (`scan::COVER_FILES`, recorded per album in `cover_files` by each scan) counts as a cover: such albums aren't embedded or reported as missing one. Use `db::HAS_COVER` in SQL.
- Cover upgrades only replace a cover with a bigger one, and shrinking (`cover_shrink_above`) only with a smaller one. Both log the old picture bytes (`undo_log.old_cover`) so undo restores it.
- Secret settings (`Settings::SECRETS`) never reach the browser: `GET /api/settings` sends `Settings::masked()`, and `PUT` restores masked values with `unmask`. A new secret goes in that list.
- Schema changes go in `db::migrate`, keyed on `PRAGMA user_version`. Never edit `SCHEMA` for an existing table.
- xmusic never deletes music files. "Keep this copy" in the duplicate finder moves the other copies with `apply::trash` to `<library>/.xmusic-trash/` (same relative path, logged in the `trash` table). `apply::restore` moves them back. Scans skip that folder, and emptying it is left to the user.
- Duplicate groups include the disc set in their key, so `Disc 1 X` and `Disc 2 X` folders are halves of one album, not copies.
- Cancelling a job (`job::cancel`) unwinds it at its next `job::tick()` or `job::check()` (a cancel point that doesn't count an item; the album pass checks between files). Never call either in the middle of a file write or a DB transaction you need to keep.
- `/metrics` is public (no session) on purpose, so Prometheus can scrape it. Labels may hold library names, formats, statuses and job kinds, never file paths or tag values.
- Every outbound HTTP call in `remote.rs` goes through `counted(service, ...)` so it shows up in `xmusic_remote_requests_total`.
- Everything inserted into the dashboard with `innerHTML` goes through `esc()`, because tag values are untrusted.

## Adding a setting
1. Add the field and its default to `Settings` in `db.rs`.
2. Add a form field in `dashboard.html` (`txt`/`chk`/`list` helpers, keyed by the field name).
3. Read it where it's used via `Settings::load`.

## Conventions
- `// ponytail:` comments mark deliberate simplifications and say when to upgrade them.
- Responses are `serde_json::Value`; use `web::rows()` to turn SQL results into JSON.

## Monitoring
- `grafana/xmusic-dashboard.json`: import it in Grafana (Dashboards → New → Import). Pick the Prometheus datasource from the dashboard's variable.
- Prometheus scrape config:
  ```yaml
  - job_name: xmusic
    scrape_interval: 60s
    static_configs:
      - targets: ["<xmusic-host>:8337"]
  ```
- New metrics need a panel in the dashboard JSON, and a check in the `metrics::renders` test.
