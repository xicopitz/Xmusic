//! Every file write goes through `write_with_undo`: original values are logged before touching the file.

use crate::db::{self, Settings, Track};
use crate::{job, matcher, remote, scan, tags};
use rayon::prelude::*;
use rusqlite::{Connection, params};
use std::collections::BTreeMap;
use std::path::Path;

fn written() {
    crate::metrics::FILES_WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The one write path: `set` fields and, if given, the front cover (added, or replacing the current
/// one) in a single save. Logs the original values first; if nothing differs the file isn't touched.
/// Updates the track's row from what was written. Returns whether the file was written.
pub fn write_with_undo(c: &Connection, t: &Track, set: &BTreeMap<String, String>, cover: Option<&[u8]>) -> Result<bool, String> {
    if set.is_empty() && cover.is_none() {
        return Ok(false);
    }
    let changes: Vec<_> = set.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
    let mut after = tags::Fields::new();
    let mut logged = None;
    let res = tags::write(Path::new(&t.path), &changes, cover, |fields, old_cover| {
        if cover.is_none() && set.iter().all(|(k, v)| fields.get(k) == Some(v)) {
            return Ok(false); // already what we'd write: no write, no upload
        }
        let old: BTreeMap<&String, Option<&String>> = set.keys().map(|k| (k, fields.get(k))).collect();
        c.execute(
            "INSERT INTO undo_log(path,dir,ts,old,added_cover,old_cover) VALUES(?1,?2,?3,?4,?5,?6)",
            params![t.path, t.dir, db::now(), serde_json::to_string(&old).unwrap(), cover.is_some() && old_cover.is_none(), cover.and(old_cover)],
        )
        .map_err(|e| e.to_string())?;
        logged = Some(c.last_insert_rowid());
        after = fields.clone();
        after.extend(set.clone());
        after.retain(|_, v| !v.trim().is_empty()); // as tags::read stores them
        Ok(true)
    });
    match res {
        Ok(true) => {
            written();
            scan::store(c, &t.path, &after, cover);
            Ok(true)
        }
        Ok(false) => Ok(false),
        Err(e) => {
            if let Some(id) = logged {
                c.execute("DELETE FROM undo_log WHERE id=?1", [id]).ok();
            }
            Err(e)
        }
    }
}

fn fetch_cover(s: &Settings, release: &str, dir: &str) -> Option<Vec<u8>> {
    let bytes = remote::cover(&s.contact_email, release, &s.cover_size).map_err(|e| eprintln!("{e}")).ok()??;
    if s.cover_save_file {
        let p = Path::new(dir).join("cover.jpg");
        if !p.exists() {
            std::fs::write(&p, &bytes).map_err(|e| eprintln!("{}: {e}", p.display())).ok();
        }
    }
    Some(bytes)
}

/// Totals of the album pass, for the job summary.
#[derive(Default)]
pub struct Done {
    pub applied: usize,
    pub failed: usize,
    pub files: usize,
    pub covers: usize,
    pub lyrics: usize,
}

impl std::ops::AddAssign for Done {
    fn add_assign(&mut self, o: Done) {
        self.applied += o.applied;
        self.failed += o.failed;
        self.files += o.files;
        self.covers += o.covers;
        self.lyrics += o.lyrics;
    }
}

pub fn wants_lyrics(s: &Settings) -> bool {
    s.lyrics_enabled && (s.lyrics_embed || s.lyrics_save_lrc)
}

/// What the album pass does. A full run does all of it; the Jobs page can run each part alone.
#[derive(Clone, Copy)]
pub struct Pass {
    /// approved proposals (their albums also get a missing cover, in the same write)
    pub tags: bool,
    pub covers: bool,
    pub lyrics: bool,
}

impl Pass {
    pub const ALL: Pass = Pass { tags: true, covers: true, lyrics: true };
    pub const APPROVED: Pass = Pass { tags: true, covers: false, lyrics: false };
    pub const COVERS: Pass = Pass { tags: false, covers: true, lyrics: false };
    pub const LYRICS: Pass = Pass { tags: false, covers: false, lyrics: true };
}

/// Albums the write pass has work for: an approved proposal, a missing, small or oversized cover on a
/// tagged album, or tracks without lyrics, each only if `p` asks for it.
pub fn album_dirs(c: &Connection, s: &Settings, p: Pass) -> Vec<String> {
    c.prepare(&format!(
        "SELECT DISTINCT dir FROM tracks WHERE (?5 AND dir IN (SELECT dir FROM proposals WHERE status='approved'))
         OR (?1 AND (NOT {} OR (?2>0 AND cover_px>0 AND cover_px<?2) OR (?6>0 AND cover_px>?6)) AND json_extract(tags,'$.MUSICBRAINZ_ALBUMID') IS NOT NULL)
         OR (?3 AND has_lyrics=0 AND (lyrics_miss IS NULL OR lyrics_miss<?4)
             AND json_extract(tags,'$.TITLE') IS NOT NULL AND json_extract(tags,'$.ARTIST') IS NOT NULL)",
        db::HAS_COVER
    ))
    .unwrap()
    .query_map(params![s.cover_embed && p.covers, s.cover_upgrade_below, wants_lyrics(s) && p.lyrics, lyrics_retry(), p.tags, s.cover_shrink_above], |r| r.get(0))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// Misses older than this are asked again.
fn lyrics_retry() -> i64 {
    db::now() - 30 * 86400
}

/// The write pass. Each album fetches what it lacks (cover once, lyrics per track) and then writes
/// every track at most once: approved tags + cover + lyrics together. Albums run `io_threads` at a time.
/// `only`: limit the first round to these albums (an import); albums approved meanwhile still join.
pub fn albums(s: &Settings, p: Pass, only: Option<&[String]>) -> Done {
    let c = db::open();
    let mut dirs = album_dirs(&c, s, p);
    if let Some(only) = only {
        dirs.retain(|d| only.contains(d));
    }
    job::phase("albums", dirs.len() as u64);
    let mut seen = std::collections::HashSet::new();
    let mut done = Done::default();
    while !dirs.is_empty() {
        seen.extend(dirs.iter().cloned());
        done += s.io_pool().install(|| {
            dirs.par_iter()
                .map_init(db::open, |c, dir| {
                    let d = album(c, dir, s, p);
                    job::tick();
                    d
                })
                .reduce(Done::default, |mut a, b| {
                    a += b;
                    a
                })
        });
        // albums approved while this pass ran join it instead of waiting for the next run
        dirs = if p.tags { album_dirs(&c, s, Pass::APPROVED).into_iter().filter(|d| !seen.contains(d)).collect() } else { vec![] };
        job::grow(dirs.len() as u64);
    }
    done
}

fn album(c: &Connection, dir: &str, s: &Settings, p: Pass) -> Done {
    let mut d = Done::default();
    let prop: Option<(i64, String)> = c
        .query_row("SELECT id,changes FROM proposals WHERE dir=?1 AND status='approved'", [dir], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok()
        .filter(|_| p.tags);
    let changes: matcher::Changes = prop.as_ref().and_then(|p| serde_json::from_str(&p.1).ok()).unwrap_or_default();
    let tracks = db::tracks_where(c, "dir=?1", [dir]);
    let mut sets: Vec<BTreeMap<String, String>> = tracks.iter().map(|t| changes.get(&t.id).cloned().unwrap_or_default()).collect();
    // tags as they will be after this write
    let eff = |sets: &[BTreeMap<String, String>], i: usize, k: &str| sets[i].get(k).or(tracks[i].tags.get(k)).cloned().unwrap_or_default();

    let small = s.cover_upgrade_below;
    // a cover.jpg in the folder is a cover: players like Navidrome read it, and embedding would rewrite every file
    let cover_file: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM cover_files WHERE dir=?1)", [dir], |r| r.get(0)).unwrap();
    let too_big = |t: &Track| s.cover_shrink_above > 0 && t.cover_px > s.cover_shrink_above;
    let wants_cover = |t: &Track| (!t.has_cover && !cover_file) || (small > 0 && t.cover_px > 0 && t.cover_px < small) || too_big(t);
    let release = (0..tracks.len()).map(|i| eff(&sets, i, "MUSICBRAINZ_ALBUMID")).find(|r| !r.is_empty());
    // cancel points between files: on a network mount one album can take minutes. A half-written album
    // keeps its proposal approved; the next run finishes it (tracks already written are skipped).
    job::check();
    let cover = match release {
        Some(r) if s.cover_embed && (p.covers || prop.is_some()) && tracks.iter().any(wants_cover) => fetch_cover(s, &r, dir),
        _ => None,
    };
    let new_px = cover.as_deref().and_then(tags::image_px).unwrap_or(0);

    if wants_lyrics(s) && p.lyrics {
        let need: std::collections::HashSet<i64> = c
            .prepare("SELECT id FROM tracks WHERE dir=?1 AND has_lyrics=0 AND (lyrics_miss IS NULL OR lyrics_miss<?2)")
            .unwrap()
            .query_map(params![dir, lyrics_retry()], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for i in (0..tracks.len()).filter(|&i| need.contains(&tracks[i].id)) {
            job::check();
            if remote::lyrics_paused() {
                break;
            }
            let t = &tracks[i];
            let (artist, title) = (eff(&sets, i, "ARTIST"), eff(&sets, i, "TITLE"));
            if artist.is_empty() || title.is_empty() {
                continue;
            }
            let (synced, plain) = match remote::lyrics(&s.contact_email, &artist, &title, &eff(&sets, i, "ALBUM"), t.duration) {
                Ok(Some(x)) => x,
                Ok(None) => {
                    // remember the miss so we don't ask again for 30 days
                    c.execute("UPDATE tracks SET lyrics_miss=?1 WHERE id=?2", params![db::now(), t.id]).unwrap();
                    continue;
                }
                Err(e) => {
                    eprintln!("{e}");
                    continue;
                }
            };
            let text = if s.lyrics_prefer_synced { synced.clone().or(plain) } else { plain.or(synced.clone()) }.unwrap();
            d.lyrics += 1;
            if s.lyrics_save_lrc {
                let lrc = Path::new(&t.path).with_extension("lrc");
                if !lrc.exists() && std::fs::write(&lrc, synced.as_deref().unwrap_or(&text)).map_err(|e| eprintln!("{}: {e}", lrc.display())).is_ok() {
                    c.execute("UPDATE tracks SET has_lyrics=1 WHERE id=?1", [t.id]).unwrap();
                }
            }
            if s.lyrics_embed {
                sets[i].insert("LYRICS".into(), text);
            }
        }
    }

    let mut errs = vec![];
    for (t, set) in tracks.iter().zip(&sets) {
        job::check();
        // only a cover that adds one, is bigger than a too-small one, or smaller than an oversized one
        let fits = !t.has_cover || if too_big(t) { new_px > 0 && new_px < t.cover_px } else { new_px > t.cover_px };
        let cv = cover.as_deref().filter(|_| wants_cover(t) && fits);
        match write_with_undo(c, t, set, cv) {
            Ok(true) => {
                d.files += 1;
                d.covers += cv.is_some() as usize;
            }
            Ok(false) => {}
            Err(e) => errs.push(format!("{}: {e}", t.path)),
        }
    }
    match prop {
        Some((id, _)) => {
            let status = if errs.is_empty() { "applied" } else { "review" };
            c.execute("UPDATE proposals SET status=?1, note=?2, updated=?3 WHERE id=?4", params![status, errs.join("; "), db::now(), id]).unwrap();
            if errs.is_empty() { d.applied += 1 } else { d.failed += 1 }
        }
        None => errs.iter().for_each(|e| eprintln!("{e}")),
    }
    d
}

/// Write custom tags + rewrite rules to every file (no MusicBrainz involved).
pub fn rules_everywhere(s: &Settings) -> usize {
    let c = db::open();
    let rules = matcher::compile_rules(s);
    let tracks = db::tracks_where(&c, "1", []);
    job::phase("custom tags & rewrites", tracks.len() as u64);
    s.io_pool().install(|| {
        tracks
            .par_iter()
            .map_init(db::open, |c, t| {
                let mut new = BTreeMap::new();
                matcher::apply_rules(t, &mut new, s, &rules);
                let wrote = write_with_undo(c, t, &new, None).map_err(|e| eprintln!("{}: {e}", t.path)).unwrap_or(false);
                job::tick();
                wrote as usize
            })
            .sum()
    })
}

/// Health → disc numbers: write `health::disc_changes` for these albums (None = every album that has some).
pub fn fix_discs(s: &Settings, dirs: Option<&[String]>) -> usize {
    let c = db::open();
    let tracks = match dirs {
        Some(d) => db::tracks_where(&c, "dir IN (SELECT value FROM json_each(?1))", [serde_json::to_string(d).unwrap()]),
        None => db::tracks_where(&c, "1", []),
    };
    let mut albums: BTreeMap<&str, Vec<&Track>> = BTreeMap::new();
    for t in &tracks {
        albums.entry(&t.dir).or_default().push(t);
    }
    let mut work: Vec<(&Track, BTreeMap<String, String>)> = vec![];
    for (dir, ts) in &albums {
        let mut fixes = crate::health::disc_changes(dir, ts.iter().map(|t| (t.id, t.path.as_str(), &t.tags)));
        for t in ts {
            if let Some(mut set) = fixes.remove(&t.id) {
                set.retain(|k, _| s.writable(k)); // "Never overwrite" still holds
                work.push((t, set));
            }
        }
    }
    job::phase("disc numbers", work.len() as u64);
    s.io_pool().install(|| {
        work.par_iter()
            .map_init(db::open, |c, (t, set)| {
                let wrote = write_with_undo(c, t, set, None).map_err(|e| eprintln!("{}: {e}", t.path)).unwrap_or(false);
                job::tick();
                wrote as usize
            })
            .sum()
    })
}

pub const ARTIST_FIELDS: [&str; 2] = ["ARTIST", "ALBUMARTIST"];

/// Set ARTIST / ALBUMARTIST to `to` on tracks where it is exactly one of `from`. Only those files are touched.
pub fn rename_artist(s: &Settings, from: &[String], to: &str) -> usize {
    let c = db::open();
    let tracks = db::tracks_where(
        &c,
        "json_extract(tags,'$.ARTIST') IN (SELECT value FROM json_each(?1)) OR json_extract(tags,'$.ALBUMARTIST') IN (SELECT value FROM json_each(?1))",
        [serde_json::to_string(from).unwrap()],
    );
    job::phase("rename artist", tracks.len() as u64);
    s.io_pool().install(|| {
        tracks
            .par_iter()
            .map_init(db::open, |c, t| {
                let set: BTreeMap<String, String> =
                    ARTIST_FIELDS.iter().filter(|k| t.tags.get(**k).is_some_and(|v| from.contains(v))).map(|k| (k.to_string(), to.to_string())).collect();
                let wrote = write_with_undo(c, t, &set, None).map_err(|e| eprintln!("{}: {e}", t.path)).unwrap_or(false);
                job::tick();
                wrote as usize
            })
            .sum()
    })
}

/// Folder (inside each library root) that duplicate copies are moved to. Never emptied by xmusic.
pub const TRASH: &str = ".xmusic-trash";

/// Move an album folder to `<library>/.xmusic-trash/<same relative path>`. Nothing is deleted;
/// `restore` moves it back. Only folders that hold this album alone (plus its disc subfolders).
pub fn trash(c: &Connection, dir: &str) -> Result<(), String> {
    let (root, bytes): (String, i64) = c
        .query_row("SELECT l.path, SUM(t.size) FROM tracks t JOIN libraries l ON l.id=t.library_id WHERE t.dir=?1", [dir], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| format!("{dir} is not an album in the library"))?;
    let rel = Path::new(dir).strip_prefix(&root).ok().filter(|r| !r.as_os_str().is_empty()).ok_or(format!("{dir} is a library root, move its files yourself"))?;
    let foreign: i64 = c.query_row("SELECT COUNT(*) FROM tracks WHERE substr(path,1,length(?1)+1)=?1||'/' AND dir<>?1", [dir], |r| r.get(0)).unwrap();
    if foreign > 0 {
        return Err(format!("{dir} also holds other albums, move it yourself"));
    }
    let mut to = Path::new(&root).join(TRASH).join(rel);
    if to.exists() {
        to = to.with_file_name(format!("{} ({})", to.file_name().unwrap().to_string_lossy(), db::now()));
    }
    std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::rename(dir, &to).map_err(|e| format!("move {dir}: {e}"))?;
    c.execute("INSERT INTO trash(dir,moved_to,bytes,ts) VALUES(?1,?2,?3,?4)", params![dir, to.to_string_lossy(), bytes, db::now()]).unwrap();
    c.execute("DELETE FROM tracks WHERE dir=?1", [dir]).unwrap();
    c.execute("DELETE FROM proposals WHERE dir=?1 AND status<>'applied'", [dir]).unwrap();
    Ok(())
}

/// Move a trashed folder back; the next scan reads it again.
pub fn restore(c: &Connection, id: i64) -> Result<(), String> {
    let (dir, from): (String, String) = c.query_row("SELECT dir,moved_to FROM trash WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|_| "not in trash".to_string())?;
    if Path::new(&dir).exists() {
        return Err(format!("{dir} exists again, move {from} back yourself"));
    }
    if Path::new(&from).exists() {
        std::fs::rename(&from, &dir).map_err(|e| format!("restore {dir}: {e}"))?;
    }
    c.execute("DELETE FROM trash WHERE id=?1", [id]).unwrap();
    Ok(())
}

/// Restore original values, newest first. `dir` None = everything.
pub fn undo(dir: Option<&str>) -> usize {
    let c = db::open();
    let rows: Vec<(i64, String, String, bool, Option<Vec<u8>>)> = c
        .prepare("SELECT id,path,old,added_cover,old_cover FROM undo_log WHERE ?1 IS NULL OR dir=?1 ORDER BY id DESC")
        .unwrap()
        .query_map([dir], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    job::phase("undo", rows.len() as u64);
    let mut n = 0;
    for (id, path, old, added_cover, old_cover) in rows {
        let old: BTreeMap<String, Option<String>> = serde_json::from_str(&old).unwrap_or_default();
        let changes: Vec<_> = old.into_iter().collect();
        // a replaced cover goes back; an added one is removed (empty bytes)
        let cover = old_cover.as_deref().or(added_cover.then_some(&[][..]));
        match tags::write(Path::new(&path), &changes, cover, |_, _| Ok(true)) {
            Ok(_) => {
                c.execute("DELETE FROM undo_log WHERE id=?1", [id]).unwrap();
                scan::refresh(&c, std::slice::from_ref(&path));
                crate::metrics::FILES_RESTORED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                n += 1;
            }
            Err(e) => eprintln!("undo {path}: {e}"),
        }
        job::tick();
    }
    // undone albums go back to the queue
    c.execute("UPDATE proposals SET status='review' WHERE status='applied' AND (?1 IS NULL OR dir=?1)", [dir]).unwrap();
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs ffmpeg to make a real FLAC; skipped otherwise.
    #[test]
    fn write_read_undo_roundtrip() {
        let dir = std::env::temp_dir().join(format!("xmusic-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.flac");
        let ok = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "quiet", "-y", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo", "-t", "1", "-metadata", "TITLE=Old", "-metadata", "MOOD=calm"])
            .arg(&f)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("ffmpeg missing, skipping");
            return;
        }
        unsafe { std::env::set_var("XMUSIC_DATA", &dir) };
        let c = db::open();
        let p = f.to_string_lossy().into_owned();
        let t = Track { id: 1, path: p.clone(), dir: dir.to_string_lossy().into(), duration: 1.0, has_cover: false, cover_px: 0, tags: Default::default() };
        let set = BTreeMap::from([("TITLE".to_string(), "New".to_string()), ("SOURCE".to_string(), "vinyl".to_string())]);
        // 1x1 PNG
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\rIDATx\x9cc\xf8\x0f\0\0\x01\x01\0\x05\x18\xd8N\0\0\0\0IEND\xaeB`\x82";
        let mut set = set;
        set.insert("LYRICS".into(), "la la".into());
        assert!(write_with_undo(&c, &t, &set, Some(png)).unwrap());
        let i = tags::read(&f).unwrap();
        assert_eq!(i.fields["TITLE"], "New");
        assert_eq!(i.fields["SOURCE"], "vinyl");
        assert_eq!(i.fields["LYRICS"], "la la");
        assert_eq!(i.fields["MOOD"], "calm", "untouched custom keys survive");
        assert!(i.has_cover);
        let rows = |c: &Connection| c.query_row("SELECT COUNT(*) FROM undo_log", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(rows(&c), 1, "tags + cover + lyrics are one write, one undo row");

        // same values again: no write, no undo row, file untouched
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        assert!(!write_with_undo(&c, &t, &set, None).unwrap());
        assert_eq!(rows(&c), 1);
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), mtime);

        assert_eq!(undo(None), 1);
        let i = tags::read(&f).unwrap();
        assert_eq!(i.fields["TITLE"], "Old");
        assert!(!i.fields.contains_key("SOURCE"));
        assert!(!i.fields.contains_key("LYRICS"));
        assert!(!i.has_cover);

        // cover upgrade: a 100px cover replaced by a 600px one, then undone back to 100px
        let jpg = |px: u32| {
            let out = dir.join(format!("{px}.jpg"));
            std::process::Command::new("ffmpeg")
                .args(["-loglevel", "quiet", "-y", "-f", "lavfi", "-i", &format!("color=red:s={px}x{px}"), "-frames:v", "1"])
                .arg(&out)
                .status()
                .unwrap();
            std::fs::read(out).unwrap()
        };
        tags::write(&f, &[], Some(&jpg(100)), |_, _| Ok(true)).unwrap();
        assert_eq!(tags::read(&f).unwrap().cover_px, 100);
        write_with_undo(&c, &t, &BTreeMap::new(), Some(&jpg(600))).unwrap();
        let i = tags::read(&f).unwrap();
        assert_eq!(i.cover_px, 600);
        assert_eq!(i.fields["TITLE"], "Old", "cover swap leaves tags alone");
        assert_eq!(undo(None), 1);
        assert_eq!(tags::read(&f).unwrap().cover_px, 100);
        std::fs::remove_dir_all(&dir).ok();
    }
}
