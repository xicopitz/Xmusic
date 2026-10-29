# Safety and undo

## What xmusic writes

- **Tags and embedded covers and lyrics** in your audio files, only for albums you approved, albums that
  scored above *Auto-apply above*, or when you run an explicit job (rules, covers, lyrics).
- **Optional sidecar files:** `cover.jpg` (if enabled) and `.lrc` lyrics (if enabled).
- **Its own database** in the data folder.

It never deletes music files.

## The undo log

Before a file is changed, its original values are written to the undo log, and the file is changed only after
that succeeds. When a cover was added, or replaced by a bigger one, the log notes it, and the old picture is kept
so it can be restored. Files that already match are not written at all.

Undo from an album's proposal (**Undo this album**) or for everything (**Jobs → Undo all changes**).

## Duplicates and the trash

**Keep this copy** moves the other copies to `.xmusic-trash/` in the same library. They keep their relative
path, are logged in the database and can be restored. Scans skip the folder. Emptying it is up to you.

## Review before writing

Low-confidence matches are never written by a job. They wait in **Review**, with every old and new value visible
and editable.

## Access

- Settings, libraries and users are admin-only.
- Passwords are hashed with argon2, session tokens are stored as SHA-256 hashes, and login attempts are throttled
  per IP.
- `/metrics` is public so Prometheus can scrape it, but it only exposes counts and labels such as library names,
  formats and job kinds, never file paths or tag values.
- Tag values are untrusted: everything inserted into the dashboard is escaped.
- Put xmusic behind a reverse proxy with HTTPS if you expose it beyond your LAN.
