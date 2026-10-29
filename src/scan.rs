//! Incremental library scan: walk enabled libraries, re-read only files whose mtime/size changed.

use crate::{db, job, tags};
use rayon::prelude::*;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use walkdir::WalkDir;

fn mtime_size(p: &Path) -> Option<(i64, i64)> {
    let m = std::fs::metadata(p).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((t, m.len() as i64))
}

/// Disc number of a disc subfolder ("CD1", "Disc 2", "disk_03 - Bonus"), None for anything else.
/// "Disc 1 Flow State" is not one: such folders usually sit in an artist folder, not an album folder.
pub fn disc_folder(name: &str) -> Option<usize> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:cd|dis[ck])[\s._-]*(\d{1,2})(?:\s*[-–:(\[].*)?$").unwrap());
    RE.captures(name.trim())?[1].parse().ok()
}

/// Album folder of a file: its parent, or the grandparent when the parent is a disc subfolder.
pub fn album_dir(p: &Path) -> String {
    let parent = p.parent().unwrap_or(Path::new(""));
    let disc = parent.file_name().and_then(|n| n.to_str()).and_then(disc_folder);
    match (disc, parent.parent()) {
        (Some(_), Some(gp)) => gp,
        _ => parent,
    }
    .to_string_lossy()
    .into_owned()
}

fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| tags::AUDIO_EXT.contains(&e.to_ascii_lowercase().as_str()))
}

/// Cover files next to the music; `web::cover` serves them when a track has no embedded art.
pub const COVER_FILES: [&str; 4] = ["cover.jpg", "folder.jpg", "cover.png", "folder.png"];

/// Returns number of files (re)read.
pub fn run(c: &mut Connection) -> usize {
    let libs: Vec<(i64, String)> = c
        .prepare("SELECT id,path FROM libraries WHERE enabled=1")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        // an unmounted library must not wipe its tracks
        .filter(|(_, p): &(i64, String)| Path::new(p).is_dir())
        .collect();
    let known: HashMap<String, (i64, i64)> = c
        .prepare("SELECT path,COALESCE(mtime,-1),size FROM tracks") // NULL mtime = force a re-read
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    job::phase("scan: listing files", 0);
    let mut all: Vec<(i64, PathBuf, (i64, i64))> = Vec::new();
    let mut covers: HashSet<String> = HashSet::new();
    for (lib, root) in &libs {
        let walk = WalkDir::new(root).follow_links(true).into_iter().filter_entry(|e| e.file_name() != crate::apply::TRASH);
        for e in walk.filter_map(Result::ok) {
            crate::job::check(); // listing a big network mount takes minutes
            if e.file_type().is_file() && COVER_FILES.iter().any(|n| e.file_name() == *n) {
                covers.insert(album_dir(e.path()));
            }
            if e.file_type().is_file() && is_audio(e.path())
                && let Some(ms) = mtime_size(e.path()) {
                    all.push((*lib, e.into_path(), ms));
                }
        }
    }
    let changed: Vec<_> = all
        .iter()
        .filter(|(_, p, ms)| known.get(p.to_str().unwrap_or_default()) != Some(ms))
        .collect();

    job::phase("scan: reading tags", changed.len() as u64);
    let read: Vec<_> = db::Settings::load(c).io_pool().install(|| {
        changed
            .par_iter()
            .filter_map(|(lib, p, ms)| {
                let r = tags::read(p);
                job::tick();
                match r {
                    Ok(info) => Some((*lib, p, *ms, info)),
                    Err(e) => {
                        eprintln!("skip {}: {e}", p.display());
                        None
                    }
                }
            })
            .collect()
    });

    let scan_id = db::now();
    let tx = c.transaction().unwrap();
    {
        let mut up = tx
            .prepare(
                "INSERT INTO tracks(library_id,path,dir,mtime,size,format,bitrate,sample_rate,bit_depth,duration,has_cover,has_lyrics,tags,seen,cover_px)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                 ON CONFLICT(path) DO UPDATE SET library_id=excluded.library_id,dir=excluded.dir,mtime=excluded.mtime,size=excluded.size,
                 format=excluded.format,bitrate=excluded.bitrate,sample_rate=excluded.sample_rate,bit_depth=excluded.bit_depth,
                 duration=excluded.duration,has_cover=excluded.has_cover,has_lyrics=excluded.has_lyrics,tags=excluded.tags,seen=excluded.seen,cover_px=excluded.cover_px",
            )
            .unwrap();
        for (lib, p, (mt, sz), i) in &read {
            let dir = album_dir(p);
            let has_lyrics = i.fields.contains_key("LYRICS") || i.fields.contains_key("UNSYNCEDLYRICS") || p.with_extension("lrc").exists();
            up.execute(params![
                lib, p.to_string_lossy(), dir, mt, sz, i.format, i.bitrate, i.sample_rate, i.bit_depth, i.duration,
                i.has_cover, has_lyrics, serde_json::to_string(&i.fields).unwrap(), scan_id, i.cover_px
            ])
            .unwrap();
        }
        let mut seen = tx.prepare("UPDATE tracks SET seen=?1 WHERE path=?2").unwrap();
        for (_, p, _) in &all {
            seen.execute(params![scan_id, p.to_string_lossy()]).unwrap();
        }
        // only prune inside libraries we actually walked
        for (_, root) in &libs {
            let root = root.trim_end_matches('/');
            tx.execute("DELETE FROM cover_files WHERE dir=?1 OR substr(dir,1,length(?1)+1)=?1||'/'", [root]).unwrap();
        }
        let mut cov = tx.prepare("INSERT OR IGNORE INTO cover_files(dir) VALUES(?1)").unwrap();
        for d in &covers {
            cov.execute([d]).unwrap();
        }
        for (lib, _) in &libs {
            tx.execute("DELETE FROM tracks WHERE library_id=?1 AND seen<>?2", params![lib, scan_id]).unwrap();
            tx.execute("UPDATE libraries SET last_scan=?1 WHERE id=?2", params![scan_id, lib]).unwrap();
        }
        // folders that are gone (deleted, or merged into their album as a disc subfolder); applied ones stay as history
        tx.execute("DELETE FROM proposals WHERE status<>'applied' AND dir NOT IN (SELECT dir FROM tracks)", []).unwrap();
    }
    tx.commit().unwrap();
    read.len()
}

/// Update a track's row after we wrote it, from what was written instead of re-reading the file
/// (each open on a network mount costs). Non-FLAC files re-read: their generic tag drops unmapped keys.
pub fn store(c: &Connection, path: &str, fields: &tags::Fields, cover: Option<&[u8]>) {
    let p = Path::new(path);
    let Some((mt, sz)) = mtime_size(p).filter(|_| tags::is_flac(p)) else { return refresh(c, &[path.to_string()]) };
    let has_lyrics = fields.contains_key("LYRICS") || fields.contains_key("UNSYNCEDLYRICS") || p.with_extension("lrc").exists();
    let tags = serde_json::to_string(fields).unwrap();
    match cover {
        Some(b) => c.execute(
            "UPDATE tracks SET mtime=?1,size=?2,has_lyrics=?3,tags=?4,has_cover=1,cover_px=?5 WHERE path=?6",
            params![mt, sz, has_lyrics, tags, tags::image_px(b).unwrap_or(0), path],
        ),
        None => c.execute("UPDATE tracks SET mtime=?1,size=?2,has_lyrics=?3,tags=?4 WHERE path=?5", params![mt, sz, has_lyrics, tags, path]),
    }
    .unwrap();
}

/// Re-read specific files (after undo, or a non-FLAC write).
pub fn refresh(c: &Connection, paths: &[String]) {
    for p in paths {
        let path = Path::new(p);
        let (Ok(i), Some((mt, sz))) = (tags::read(path), mtime_size(path)) else { continue };
        let has_lyrics = i.fields.contains_key("LYRICS") || path.with_extension("lrc").exists();
        c.execute(
            "UPDATE tracks SET mtime=?1,size=?2,has_cover=?3,has_lyrics=?4,tags=?5,cover_px=?6 WHERE path=?7",
            params![mt, sz, i.has_cover, has_lyrics, serde_json::to_string(&i.fields).unwrap(), i.cover_px, p],
        )
        .unwrap();
    }
}

#[test]
fn disc_folders() {
    assert_eq!(disc_folder("CD1"), Some(1));
    assert_eq!(disc_folder("Disc 2"), Some(2));
    assert_eq!(disc_folder("disk_03 - Bonus"), Some(3));
    assert_eq!(disc_folder("CDs"), None);
    assert_eq!(disc_folder("Discography"), None);
    assert_eq!(disc_folder("CD 2 (Remixes)"), Some(2));
    assert_eq!(disc_folder("Disc 1 Flow State"), None);
    assert_eq!(album_dir(Path::new("/m/Artist - Album/CD2/01.flac")), "/m/Artist - Album");
    assert_eq!(album_dir(Path::new("/m/Artist - Album/01.flac")), "/m/Artist - Album");
}
