//! Watch mode: start a scan (or full run) once files under an enabled library stop changing.
// ponytail: changes made while any job runs are ignored (they are mostly our own tag writes);
// the scheduled scan picks up anything copied in during a long run.

use crate::db::{self, Settings};
use crate::{job, tags};
use notify::event::{EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Wait this long after the last change, so copying an album triggers one scan, not hundreds.
const QUIET: Duration = Duration::from_secs(30);

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Libraries or settings changed: rebuild the watch list.
pub fn reload() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Audio files and folders matter; covers, .lrc files and other sidecars don't.
fn relevant(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_none_or(|e| tags::AUDIO_EXT.contains(&e.to_ascii_lowercase().as_str()))
}

/// Players and scanners reading files, or atime updates, are not changes.
fn is_change(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_))
        && !matches!(kind, EventKind::Modify(ModifyKind::Metadata(_)))
}

pub fn run() {
    loop {
        let generation = GENERATION.load(Ordering::Relaxed);
        let c = db::open();
        let s = Settings::load(&c);
        let libs: Vec<String> = if s.watch_enabled {
            c.prepare("SELECT path FROM libraries WHERE enabled=1").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        } else {
            vec![]
        };
        drop(c);
        let (tx, rx) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("watch: {e}");
                std::thread::sleep(Duration::from_secs(60));
                continue;
            }
        };
        for lib in &libs {
            // a big library can exceed fs.inotify.max_user_watches on the host; say so instead of failing silently
            if let Err(e) = watcher.watch(Path::new(lib), RecursiveMode::Recursive) {
                eprintln!("watch {lib}: {e} (raise fs.inotify.max_user_watches on the host for large libraries)");
            }
        }
        if !libs.is_empty() {
            println!("watch: watching {} librar{}", libs.len(), if libs.len() == 1 { "y" } else { "ies" });
        }
        let mut dirty: Option<Instant> = None;
        while GENERATION.load(Ordering::Relaxed) == generation {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(ev)) if is_change(&ev.kind) && !job::running() && ev.paths.iter().any(|p| relevant(p)) => dirty = Some(Instant::now()),
                Ok(Err(e)) => eprintln!("watch: {e}"),
                Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if dirty.is_some_and(|t| t.elapsed() >= QUIET) {
                let kind = if s.watch_full_run { job::Kind::Full } else { job::Kind::Scan };
                // busy: stay dirty and retry on the next tick
                if job::start(kind).is_ok() {
                    dirty = None;
                }
            }
        }
    }
}

#[test]
fn filters() {
    use notify::event::{AccessKind, CreateKind, MetadataKind};
    assert!(relevant(Path::new("/m/a/01.FLAC")));
    assert!(relevant(Path::new("/m/a/New Album")));
    assert!(!relevant(Path::new("/m/a/cover.jpg")));
    assert!(!relevant(Path::new("/m/a/01.lrc")));
    assert!(is_change(&EventKind::Create(CreateKind::File)));
    assert!(!is_change(&EventKind::Access(AccessKind::Read)));
    assert!(!is_change(&EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))));
}
