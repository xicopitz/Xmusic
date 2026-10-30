//! SQLite state. Every thread opens its own connection (WAL + busy timeout), no pool.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn data_dir() -> PathBuf {
    std::env::var("XMUSIC_DATA").unwrap_or_else(|_| "./data".into()).into()
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Startup check: the data dir must be writable, or every request would panic later.
pub fn check() -> Result<(), String> {
    let dir = data_dir();
    let ok = std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| {
        let c = Connection::open(dir.join("xmusic.db")).map_err(|e| e.to_string())?;
        c.execute_batch("CREATE TABLE IF NOT EXISTS _probe(x); DROP TABLE _probe;").map_err(|e| e.to_string())
    });
    ok.map_err(|e| {
        // the uid the process runs as is what needs write access (docker-compose `user:`)
        let ids = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| Some(format!("{}:{}", s.lines().find(|l| l.starts_with("Uid:"))?.split_whitespace().nth(1)?, s.lines().find(|l| l.starts_with("Gid:"))?.split_whitespace().nth(1)?)))
            .unwrap_or_else(|| "?".into());
        format!(
            "cannot write the database in {} ({e}).\nxmusic runs as uid:gid {ids}; that user must own the data folder.\nOn the host: sudo chown -R {ids} <the folder mounted at {}>, or change `user:` in docker-compose.yml.",
            dir.display(),
            dir.display()
        )
    })
}

pub fn open() -> Connection {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).expect("create data dir");
    let c = Connection::open(dir.join("xmusic.db")).expect("open db");
    c.busy_timeout(std::time::Duration::from_secs(30)).unwrap();
    // schema once per process: re-running it on every open (journal_mode=WAL) while other threads hold
    // connections could fail with "database is locked" without waiting for the busy timeout
    static SCHEMA_ONCE: std::sync::Once = std::sync::Once::new();
    SCHEMA_ONCE.call_once(|| init(&c));
    c.execute_batch("PRAGMA foreign_keys=ON;").expect("foreign keys"); // per connection, for the ON DELETE CASCADEs
    c
}

pub fn init(c: &Connection) {
    c.execute_batch(SCHEMA).expect("schema");
    migrate(c);
}

/// Schema changes after the first release, keyed on PRAGMA user_version.
const LATEST: i64 = 6;

fn version(c: &Connection) -> i64 {
    c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

fn migrate(c: &Connection) {
    if version(c) >= LATEST {
        return;
    }
    // several threads open the db at startup: take the write lock, then re-check
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    if version(c) < 1 {
        // cover size for upgrades + old cover bytes for undo; clearing mtime makes the next scan
        // re-read every file so cover_px gets filled in
        c.execute_batch(
            "ALTER TABLE tracks ADD COLUMN cover_px INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE undo_log ADD COLUMN old_cover BLOB;
             UPDATE tracks SET mtime=NULL;
             PRAGMA user_version=1;",
        )
        .expect("migration 1");
    }
    if version(c) < 2 {
        // disc subfolders now belong to the parent album: re-read so `dir` is recomputed
        c.execute_batch("UPDATE tracks SET mtime=NULL; PRAGMA user_version=2;").expect("migration 2");
    }
    if version(c) < 3 {
        // empty tag values ("MUSICBRAINZ_ALBUMID=") are now dropped on read; clean stored rows without re-reading
        c.execute_batch(
            "UPDATE tracks SET tags=(SELECT json_group_object(key,value) FROM json_each(tracks.tags) WHERE trim(value)<>'')
               WHERE EXISTS(SELECT 1 FROM json_each(tracks.tags) WHERE trim(value)='');
             PRAGMA user_version=3;",
        )
        .expect("migration 3");
    }
    // new default write fields join a saved write list too
    let add_fields = |keys: &[&str]| {
        let mut s = Settings::load(c);
        for k in keys {
            if !s.write_fields.iter().any(|f| f.eq_ignore_ascii_case(k)) {
                s.write_fields.push(k.to_string());
            }
        }
        s.save(c);
    };
    if version(c) < 4 {
        add_fields(&["LABEL", "CATALOGNUMBER", "ORIGINALDATE"]);
        c.execute_batch("PRAGMA user_version=4;").expect("migration 4");
    }
    if version(c) < 5 {
        add_fields(&["DISCOGS_*"]); // albums matched on Discogs get DISCOGS_RELEASE_ID
        c.execute_batch("PRAGMA user_version=5;").expect("migration 5");
    }
    if version(c) < 6 {
        add_fields(&["BARCODE"]); // matches write the release's barcode, so a re-match is exact
        c.execute_batch("PRAGMA user_version=6;").expect("migration 6");
    }
    c.execute_batch("COMMIT").unwrap();
}

/// A consistent copy of the live database at `to` (which must not exist), taken while others keep using it.
pub fn backup(c: &Connection, to: &Path) -> Result<(), String> {
    c.execute("VACUUM INTO ?1", [to.to_string_lossy()]).map(|_| ()).map_err(|e| e.to_string())
}

/// Replace the live database with the one at `from`, in place: other connections see the new data. Checks
/// that it is an xmusic database with an admin, keeps a copy of the current one in
/// `xmusic-before-restore.db`, then migrates it if it's from an older version.
pub fn restore(from: &Path) -> Result<(), String> {
    let src = Connection::open_with_flags(from, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
    let check: String = src.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(|_| "not an SQLite database".to_string())?;
    if check != "ok" {
        return Err(format!("the file is damaged: {check}"));
    }
    let admins: i64 = src
        .query_row("SELECT COUNT(*) FROM users WHERE role='admin'", [], |r| r.get(0))
        .map_err(|_| "not an xmusic database".to_string())?;
    if admins == 0 || src.prepare("SELECT 1 FROM tracks, settings, libraries").is_err() {
        return Err("not an xmusic database with an admin user".into());
    }
    if version(&src) > LATEST {
        return Err("this backup is from a newer xmusic; update first".into());
    }
    let mut c = open();
    let keep = data_dir().join("xmusic-before-restore.db");
    std::fs::remove_file(&keep).ok();
    backup(&c, &keep)?;
    rusqlite::backup::Backup::new(&src, &mut c)
        .and_then(|b| b.run_to_completion(i32::MAX, std::time::Duration::ZERO, None))
        .map_err(|e| format!("restore: {e}"))?;
    init(&c);
    Ok(())
}

/// SQL condition on `tracks`: the track is matched, to a MusicBrainz or a Discogs release.
pub const HAS_RELEASE: &str = "(json_extract(tags,'$.MUSICBRAINZ_ALBUMID') IS NOT NULL OR json_extract(tags,'$.DISCOGS_RELEASE_ID') IS NOT NULL)";

/// SQL condition on `tracks`: embedded art, or a cover.jpg / folder.jpg in the album folder (`cover_files`, refreshed by each scan).
pub const HAS_COVER: &str = "(has_cover=1 OR dir IN (SELECT dir FROM cover_files))";

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, hash TEXT NOT NULL, role TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions(token_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS libraries(id INTEGER PRIMARY KEY, name TEXT NOT NULL, path TEXT UNIQUE NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, last_scan INTEGER);
CREATE TABLE IF NOT EXISTS tracks(
  id INTEGER PRIMARY KEY, library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
  path TEXT UNIQUE NOT NULL, dir TEXT NOT NULL, mtime INTEGER, size INTEGER,
  format TEXT, bitrate INTEGER, sample_rate INTEGER, bit_depth INTEGER, duration REAL,
  has_cover INTEGER, has_lyrics INTEGER, tags TEXT NOT NULL, lyrics_miss INTEGER, seen INTEGER);
CREATE INDEX IF NOT EXISTS tracks_dir ON tracks(dir);
CREATE TABLE IF NOT EXISTS proposals(
  id INTEGER PRIMARY KEY, dir TEXT UNIQUE NOT NULL, status TEXT NOT NULL, confidence REAL,
  release_id TEXT, release_title TEXT, candidates TEXT, changes TEXT, note TEXT, updated INTEGER);
CREATE TABLE IF NOT EXISTS undo_log(id INTEGER PRIMARY KEY, path TEXT NOT NULL, dir TEXT NOT NULL, ts INTEGER, old TEXT NOT NULL, added_cover INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS stats_snapshots(ts INTEGER, tracks INTEGER, bytes INTEGER, fixed INTEGER, review INTEGER, open INTEGER);
CREATE TABLE IF NOT EXISTS jobs(id INTEGER PRIMARY KEY, kind TEXT, started INTEGER, finished INTEGER, summary TEXT);
CREATE TABLE IF NOT EXISTS trash(id INTEGER PRIMARY KEY, dir TEXT NOT NULL, moved_to TEXT NOT NULL, bytes INTEGER NOT NULL, ts INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS cover_files(dir TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS artist_images(artist_id TEXT PRIMARY KEY, checked INTEGER NOT NULL, found INTEGER NOT NULL);
";

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Rewrite {
    pub field: String,
    pub pattern: String,
    pub replace: String,
}

/// scope: "all", "path:/music/x" (prefix) or "FIELD=value" (exact, case-insensitive)
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CustomTag {
    pub key: String,
    pub value: String,
    pub scope: String,
}

/// All user settings, one JSON blob. To add one: add the field + default here, a form field in
/// dashboard.html, and read it where it's used.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub schedule: String,
    pub match_threshold: f64,
    pub auto_apply_above: f64,
    pub acoustid_key: String,
    /// second source when MusicBrainz has no good match
    pub discogs_enabled: bool,
    /// optional personal access token: raises Discogs' limit from 25 to 60 requests a minute
    pub discogs_token: String,
    pub contact_email: String,
    pub write_fields: Vec<String>,
    pub preserve_fields: Vec<String>,
    pub prefer_country: Vec<String>,
    pub prefer_media: Vec<String>,
    pub rewrites: Vec<Rewrite>,
    pub custom_tags: Vec<CustomTag>,
    pub cover_embed: bool,
    pub cover_save_file: bool,
    /// Cover Art Archive thumbnail size: "250", "500", "1200" or "original"
    pub cover_size: String,
    /// replace embedded covers whose shorter side is below this many px (0 = never)
    pub cover_upgrade_below: u32,
    /// replace embedded covers whose shorter side is above this many px with the smaller CAA one (0 = never)
    pub cover_shrink_above: u32,
    /// save artist.jpg (Wikimedia Commons, via MusicBrainz and Wikidata) during full runs
    pub artist_images: bool,
    pub lyrics_enabled: bool,
    pub lyrics_prefer_synced: bool,
    pub lyrics_embed: bool,
    pub lyrics_save_lrc: bool,
    /// rescan when files under a library change
    pub watch_enabled: bool,
    /// after a change: false = scan only, true = full run (scan + match + apply)
    pub watch_full_run: bool,
    pub gotify_url: String,
    pub gotify_token: String,
    pub gotify_priority: i64,
    pub notify_finished: bool,
    pub notify_review: bool,
    pub notify_error: bool,
    /// dashboard address as seen from your phone, e.g. https://xmusic.lan; notifications link to it
    pub public_url: String,
    /// Navidrome (or any Subsonic server) to rescan after xmusic changes files; empty = off
    pub navidrome_url: String,
    pub navidrome_user: String,
    pub navidrome_password: String,
    /// token for POST /api/hook/import (Lidarr's webhook); empty = hook off
    pub hook_token: String,
    /// files read or written at once; the work is network/disk bound, so more threads mostly add load
    pub io_threads: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            schedule: String::new(),
            match_threshold: 0.80,
            auto_apply_above: 0.95,
            acoustid_key: String::new(),
            discogs_enabled: true,
            discogs_token: String::new(),
            contact_email: String::new(),
            write_fields: ["TITLE", "ARTIST", "ALBUM", "ALBUMARTIST", "DATE", "ORIGINALDATE", "TRACKNUMBER", "TRACKTOTAL", "DISCNUMBER", "DISCTOTAL", "GENRE",
                "LABEL", "CATALOGNUMBER", "BARCODE", "MUSICBRAINZ_*", "DISCOGS_*"]
                .map(String::from)
                .to_vec(),
            preserve_fields: vec!["COMMENT".into(), "REPLAYGAIN_*".into()],
            prefer_country: vec!["XW".into(), "US".into()],
            prefer_media: vec!["Digital Media".into(), "CD".into()],
            rewrites: vec![],
            custom_tags: vec![],
            cover_embed: true,
            cover_save_file: false,
            cover_size: "1200".into(),
            cover_upgrade_below: 500,
            cover_shrink_above: 0,
            artist_images: false,
            lyrics_enabled: true,
            lyrics_prefer_synced: true,
            lyrics_embed: true,
            lyrics_save_lrc: false,
            watch_enabled: false,
            watch_full_run: false,
            gotify_url: String::new(),
            gotify_token: String::new(),
            gotify_priority: 5,
            notify_finished: true,
            notify_review: true,
            notify_error: true,
            public_url: String::new(),
            navidrome_url: String::new(),
            navidrome_user: String::new(),
            navidrome_password: String::new(),
            hook_token: String::new(),
            io_threads: 3,
        }
    }
}

impl Settings {
    /// Thread pool for file I/O (scan reads, writes), sized by `io_threads`.
    pub fn io_pool(&self) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new().num_threads(self.io_threads.clamp(1, 16)).build().unwrap()
    }

    pub fn load(c: &Connection) -> Settings {
        c.query_row("SELECT value FROM settings WHERE key='main'", [], |r| r.get::<_, String>(0))
            .optional()
            .unwrap()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, c: &Connection) {
        c.execute(
            "INSERT INTO settings(key,value) VALUES('main',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![serde_json::to_string(self).unwrap()],
        )
        .unwrap();
    }

    /// Secret fields are never sent to the browser: it gets `MASK` when one is set, and sends `MASK`
    /// back to keep the stored value.
    pub const SECRETS: [&str; 5] = ["acoustid_key", "discogs_token", "gotify_token", "navidrome_password", "hook_token"];
    pub const MASK: &str = "••••••••";

    pub fn masked(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap();
        for k in Self::SECRETS {
            if v[k] != "" {
                v[k] = Self::MASK.into();
            }
        }
        v
    }

    /// `new` as sent by the browser, with masked secrets taken from `self`.
    pub fn unmask(&self, mut new: serde_json::Value) -> serde_json::Value {
        let old = serde_json::to_value(self).unwrap();
        for k in Self::SECRETS {
            if new[k] == Self::MASK {
                new[k] = old[k].clone();
            }
        }
        new
    }

    /// Field may be written: matches write_fields and not preserve_fields (trailing `*` = prefix).
    pub fn writable(&self, key: &str) -> bool {
        let m = |pats: &[String]| {
            pats.iter().any(|p| match p.strip_suffix('*') {
                Some(pre) => key.starts_with(&pre.to_ascii_uppercase()),
                None => key.eq_ignore_ascii_case(p),
            })
        };
        m(&self.write_fields) && !m(&self.preserve_fields)
    }
}

pub struct Track {
    pub id: i64,
    pub path: String,
    pub dir: String,
    pub duration: f64,
    pub has_cover: bool,
    pub cover_px: u32,
    pub tags: crate::tags::Fields,
}

pub fn tracks_where(c: &Connection, cond: &str, p: impl rusqlite::Params) -> Vec<Track> {
    let sql = format!("SELECT id,path,dir,duration,has_cover,tags,cover_px FROM tracks WHERE {cond} ORDER BY path");
    let mut st = c.prepare(&sql).unwrap();
    st.query_map(p, |r| {
        Ok(Track {
            id: r.get(0)?,
            path: r.get(1)?,
            dir: r.get(2)?,
            duration: r.get(3)?,
            has_cover: r.get(4)?,
            tags: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            cover_px: r.get(6)?,
        })
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[test]
fn writable_globs() {
    let s = Settings::default();
    assert!(s.writable("TITLE"));
    assert!(s.writable("MUSICBRAINZ_ALBUMID"));
    assert!(!s.writable("COMMENT"));
    assert!(!s.writable("REPLAYGAIN_TRACK_GAIN"));
    assert!(s.writable("LABEL"));
    assert!(!s.writable("MOOD"));
}
