//! One background job at a time: scan → match → apply → cover art → lyrics → notify.
//! Albums needing a human are parked as proposals; the job never waits for anyone.

use crate::db::{self, Settings};
use crate::{apply, matcher, remote, scan};
use chrono::Local;
use rusqlite::params;
use serde::Serialize;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::Instant;

struct State {
    running: bool,
    kind: String,
    phase: String,
    done: u64,
    total: u64,
    phase_started: Instant,
    started: i64,
    /// phases still to come: (name, items, seconds per item seed)
    upcoming: Vec<(&'static str, u64, f64)>,
    last: Option<String>,
    /// seconds spent per finished phase ("scan: listing files" and "scan: reading tags" count as "scan")
    took: Vec<(String, f64)>,
}

static ST: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        running: false, kind: String::new(), phase: String::new(), done: 0, total: 0,
        phase_started: Instant::now(), started: 0, upcoming: vec![], last: None, took: vec![],
    })
});
static WAKE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());
static CANCEL: AtomicBool = AtomicBool::new(false);
/// Album folders the import hook announced while another job ran; imported when it ends.
static PENDING_IMPORT: Mutex<std::collections::BTreeSet<String>> = Mutex::new(std::collections::BTreeSet::new());

/// Import hook: scan, match and finish these album folders now, or right after the running job.
/// Returns whether it started now (false = queued).
pub fn import(dirs: Vec<String>) -> bool {
    PENDING_IMPORT.lock().unwrap().extend(dirs);
    start_pending()
}

fn start_pending() -> bool {
    let dirs: Vec<String> = std::mem::take(&mut *PENDING_IMPORT.lock().unwrap()).into_iter().collect();
    if dirs.is_empty() {
        return false;
    }
    if start(Kind::Import(dirs.clone())) {
        return true;
    }
    PENDING_IMPORT.lock().unwrap().extend(dirs); // busy: keep them for the end of the running job
    false
}

/// Panic payload that unwinds a cancelled job out of whatever phase it is in.
struct Cancelled;

/// Seconds per item before the phase has measured anything. Match ≈ 1 search + up to 3 lookups at 1.1s each.
fn seed(phase: &str) -> f64 {
    match phase.split(':').next().unwrap_or_default() {
        "scan" => 0.01,
        "match" => 4.4,
        "albums" => 8.0,
        "artist images" => 3.0,
        _ => 0.05,
    }
}

pub fn eta(done: u64, total: u64, elapsed: f64, seed: f64) -> f64 {
    let left = total.saturating_sub(done) as f64;
    if done == 0 { seed * left } else { elapsed / done as f64 * left }
}

pub fn phase(name: &str, total: u64) {
    let mut s = ST.lock().unwrap();
    close_phase(&mut s);
    s.phase = name.into();
    s.done = 0;
    s.total = total;
    s.phase_started = Instant::now();
    s.upcoming.retain(|(n, ..)| !name.starts_with(n));
}

/// More items for the current phase (albums approved while it runs).
pub fn grow(n: u64) {
    ST.lock().unwrap().total += n;
}

/// Unwinds the job if it was cancelled. Call between files, never mid-write or mid-transaction.
pub fn check() {
    if CANCEL.load(Relaxed) {
        std::panic::resume_unwind(Box::new(Cancelled));
    }
}

/// One item done (and a cancel point).
pub fn tick() {
    check();
    ST.lock().unwrap().done += 1;
}

/// Ask the running job to stop after its current item; false if nothing runs.
pub fn cancel() -> bool {
    let running = running();
    if running {
        CANCEL.store(true, Relaxed);
    }
    running
}

fn close_phase(s: &mut State) {
    let name = s.phase.split(':').next().unwrap_or_default().to_string();
    if name.is_empty() || name == "starting" {
        return;
    }
    let secs = s.phase_started.elapsed().as_secs_f64();
    match s.took.iter_mut().find(|(n, _)| *n == name) {
        Some(t) => t.1 += secs,
        None => s.took.push((name, secs)),
    }
}

fn hms(secs: f64) -> String {
    let s = secs as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

fn plan(upcoming: Vec<(&'static str, u64, f64)>) {
    ST.lock().unwrap().upcoming = upcoming;
}

#[derive(Serialize)]
pub struct Status {
    running: bool,
    kind: String,
    phase: String,
    done: u64,
    total: u64,
    started: i64,
    phase_eta: Option<f64>,
    eta: Option<f64>,
    upcoming: Vec<(&'static str, u64)>,
    finished: Vec<(String, f64)>,
    last: Option<String>,
    cancelling: bool,
}

pub fn status() -> Status {
    let s = ST.lock().unwrap();
    let phase_eta = s.running.then(|| eta(s.done, s.total, s.phase_started.elapsed().as_secs_f64(), seed(&s.phase)));
    let rest: f64 = s.upcoming.iter().map(|(_, n, sec)| *n as f64 * sec).sum();
    Status {
        running: s.running,
        kind: s.kind.clone(),
        phase: s.phase.clone(),
        done: s.done,
        total: s.total,
        started: s.started,
        phase_eta,
        eta: phase_eta.map(|p| p + rest),
        upcoming: s.upcoming.iter().map(|(n, c, _)| (*n, *c)).collect(),
        finished: s.took.iter().filter(|(n, _)| !s.phase.starts_with(n.as_str())).cloned().collect(),
        last: s.last.clone(),
        cancelling: s.running && CANCEL.load(Relaxed),
    }
}

pub enum Kind {
    Full,
    Apply,
    Scan,
    Rematch(Vec<String>),
    Rules,
    /// Health → artist spellings: these names become the last one
    Rename(Vec<String>, String),
    /// Health → disc numbers; None = every album
    Discs(Option<Vec<String>>),
    /// artist.jpg next to each artist's albums
    ArtistImages,
    /// the import hook: a full run limited to these album folders
    Import(Vec<String>),
    /// None = every album that needs one
    Covers(Option<Vec<String>>),
    Lyrics(Option<Vec<String>>),
    Undo(Option<String>),
}

pub fn running() -> bool {
    ST.lock().unwrap().running
}

/// Starts the job in the background; false if one is already running.
pub fn start(kind: Kind) -> bool {
    {
        let mut s = ST.lock().unwrap();
        if s.running {
            return false;
        }
        s.running = true;
        s.kind = match &kind {
            Kind::Full => "full run",
            Kind::Apply => "apply approved",
            Kind::Scan => "scan",
            Kind::Rematch(_) => "re-match",
            Kind::Rules => "custom tags & rewrites",
            Kind::Rename(..) => "rename artist",
            Kind::Discs(_) => "fix disc numbers",
            Kind::ArtistImages => "artist images",
            Kind::Import(_) => "import",
            Kind::Covers(_) => "fetch covers",
            Kind::Lyrics(_) => "fetch lyrics",
            Kind::Undo(_) => "undo",
        }
        .into();
        s.started = db::now();
        s.phase = "starting".into();
        s.upcoming.clear();
        s.took.clear();
        CANCEL.store(false, Relaxed);
    }
    std::thread::spawn(move || {
        let started = db::now();
        let label = ST.lock().unwrap().kind.clone();
        let settings = Settings::load(&db::open());
        let changed = || crate::metrics::FILES_WRITTEN.load(Relaxed) + crate::metrics::FILES_RESTORED.load(Relaxed);
        let before = changed();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(kind, &settings)));
        if changed() != before {
            navidrome(&settings);
        }
        let mut summary = match &res {
            Ok(s) => s.clone(),
            Err(e) if e.is::<Cancelled>() => "cancelled".into(),
            Err(e) => format!("failed: {}", e.downcast_ref::<String>().cloned().or(e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()),
        };
        {
            let mut s = ST.lock().unwrap();
            close_phase(&mut s);
            if s.took.len() > 1 || s.took.first().is_some_and(|t| t.1 >= 60.0) {
                summary += &format!(" · took {}", s.took.iter().map(|(n, t)| format!("{n} {}", hms(*t))).collect::<Vec<_>>().join(", "));
            }
        }
        let c = db::open();
        c.execute("INSERT INTO jobs(kind,started,finished,summary) VALUES(?1,?2,?3,?4)", params![label, started, db::now(), summary]).unwrap();
        let review: i64 = c.query_row("SELECT COUNT(*) FROM proposals WHERE status IN ('review','manual')", [], |r| r.get(0)).unwrap();
        let cancelled = res.as_ref().is_err_and(|e| e.is::<Cancelled>());
        if res.is_err() && !cancelled && settings.notify_error {
            notify(&settings, "xmusic: job failed", &summary, "#jobs");
        } else if res.is_ok() && (settings.notify_finished || (settings.notify_review && review > 0)) {
            let mut msg = summary.clone();
            if review > 0 {
                msg += &format!("\n{review} album(s) need your review in the dashboard.");
            }
            notify(&settings, &format!("xmusic: {label} finished"), &msg, if review > 0 { "#review" } else { "#jobs" });
        }
        {
            let mut s = ST.lock().unwrap();
            s.running = false;
            s.phase.clear();
            s.upcoming.clear();
            s.last = Some(format!("{label}: {summary}"));
        }
        // albums approved while another job ran (or after the album pass's last look) get applied now;
        // only after a clean finish, so a failing apply can't restart itself forever
        // imports announced meanwhile come first; their own end chains the apply
        let waiting = "SELECT EXISTS(SELECT 1 FROM proposals WHERE status='approved' AND dir IN (SELECT dir FROM tracks))";
        if !start_pending() && res.is_ok() && c.query_row(waiting, [], |r| r.get::<_, bool>(0)).unwrap() {
            start(Kind::Apply);
        }
    });
    true
}

/// Tell Navidrome to rescan, if configured. Errors are only logged.
pub fn navidrome(s: &Settings) {
    if !s.navidrome_url.is_empty()
        && let Err(e) = remote::navidrome_scan(&s.contact_email, &s.navidrome_url, &s.navidrome_user, &s.navidrome_password) {
            eprintln!("{e}");
        }
}

/// `page`: dashboard route the notification opens, e.g. "#review" (needs `public_url`).
pub fn notify(s: &Settings, title: &str, msg: &str, page: &str) {
    let link = (!s.public_url.trim().is_empty()).then(|| format!("{}/{page}", s.public_url.trim().trim_end_matches('/')));
    if !s.gotify_url.is_empty() && !s.gotify_token.is_empty()
        && let Err(e) = remote::gotify(&s.contact_email, &s.gotify_url, &s.gotify_token, s.gotify_priority, title, msg, link.as_deref()) {
            eprintln!("{e}");
        }
}

fn count(c: &rusqlite::Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn store_proposal(c: &rusqlite::Connection, dir: &str, p: &matcher::Proposal) {
    c.execute(
        "INSERT INTO proposals(dir,status,confidence,release_id,release_title,candidates,changes,note,updated)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(dir) DO UPDATE SET status=excluded.status,confidence=excluded.confidence,release_id=excluded.release_id,
         release_title=excluded.release_title,candidates=excluded.candidates,changes=excluded.changes,note=excluded.note,updated=excluded.updated",
        params![
            dir, p.status, p.confidence, p.release.as_ref().map(|r| &r.id),
            p.release.as_ref().map(|r| format!("{} – {}", r.artist, r.title)),
            serde_json::to_string(&p.candidates).unwrap(), serde_json::to_string(&p.changes).unwrap(), p.note, db::now()
        ],
    )
    .unwrap();
}

/// Match one album again, even if it already has a release id. Always lands in Review so the
/// user sees the diff; nothing to change = marked applied.
fn rematch(dirs: &[String], s: &Settings) -> String {
    let c = db::open();
    phase("match", dirs.len() as u64);
    let out: Vec<String> = dirs.iter().map(|d| rematch_one(&c, d, s)).collect();
    if out.len() == 1 { out[0].clone() } else { format!("{} album(s) re-matched, see Review", out.len()) }
}

fn rematch_one(c: &rusqlite::Connection, dir: &str, s: &Settings) -> String {
    let tracks = db::tracks_where(c, "dir=?1", [dir]);
    if tracks.is_empty() {
        tick();
        return "album folder not found in the library".into();
    }
    let mut p = matcher::match_album(&tracks, s);
    let fields: usize = p.changes.values().map(|m| m.len()).sum();
    if p.release.is_some() && fields == 0 {
        p.status = "applied";
        p.note = format!("tags already match MusicBrainz{}{}", if p.note.is_empty() { "" } else { "; " }, p.note);
    } else if p.status == "approved" {
        p.status = "review";
    }
    store_proposal(c, dir, &p);
    tick();
    let name = dir.rsplit('/').next().unwrap_or(dir);
    match &p.release {
        None => format!("{name}: no MusicBrainz match, see Review → Needs manual"),
        Some(_) if fields == 0 => format!("{name}: tags already match MusicBrainz"),
        Some(r) => format!("{name}: {fields} tag change(s) proposed against {} – {}", r.artist, r.title),
    }
}

// an album with an id on any track is matched: a bonus track the release doesn't list never gets one,
// and re-matching the album for it on every run would put an applied album back in Review
// (a const can't use format!, so this spells out db::HAS_RELEASE)
const NEEDS_MATCH: &str = "FROM tracks WHERE dir NOT IN (SELECT dir FROM tracks WHERE json_extract(tags,'$.MUSICBRAINZ_ALBUMID') IS NOT NULL
      OR json_extract(tags,'$.DISCOGS_RELEASE_ID') IS NOT NULL)
    AND dir NOT IN (SELECT dir FROM proposals WHERE status IN ('review','manual','rejected','approved'))";

/// Matches these albums and stores their proposals. Returns (auto-approved, parked for review).
fn match_dirs(c: &rusqlite::Connection, dirs: &[String], s: &Settings) -> (usize, usize) {
    phase("match", dirs.len() as u64);
    let (mut auto, mut review) = (0, 0);
    for dir in dirs {
        let tracks = db::tracks_where(c, "dir=?1", [dir]);
        let p = matcher::match_album(&tracks, s);
        match p.status {
            "approved" => auto += 1,
            _ => review += 1,
        }
        store_proposal(c, dir, &p);
        tick();
    }
    (auto, review)
}

fn run(kind: Kind, s: &Settings) -> String {
    match kind {
        Kind::Apply => {
            let d = apply::albums(s, apply::Pass::APPROVED, None);
            format!("{} album(s) applied, {} failed; {} file(s) written", d.applied, d.failed, d.files)
        }
        Kind::Rematch(dirs) => rematch(&dirs, s),
        Kind::Scan => format!("{} new or changed file(s) read", scan::run(&mut db::open())),
        Kind::Rules => format!("{} file(s) updated", apply::rules_everywhere(s)),
        Kind::Discs(dirs) => format!("{} file(s) got disc numbers fixed", apply::fix_discs(s, dirs.as_deref())),
        Kind::ArtistImages => crate::artist::images(s).to_string(),
        Kind::Import(dirs) => {
            let mut c = db::open();
            let read = scan::run_in(&mut c, Some(&dirs));
            // the scan may fold a disc subfolder into its album, so ask the db which albums these are now
            let albums: Vec<String> = c
                .prepare(&format!("SELECT DISTINCT dir {NEEDS_MATCH} AND dir IN (SELECT value FROM json_each(?1))"))
                .unwrap()
                .query_map([serde_json::to_string(&dirs).unwrap()], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let (auto, review) = match_dirs(&c, &albums, s);
            let d = apply::albums(s, apply::Pass::ALL, Some(&dirs));
            format!(
                "{} folder(s): {read} file(s) read, {} matched ({auto} auto-approved, {review} for review); {} file(s) written, {} cover(s), lyrics for {} track(s)",
                dirs.len(), albums.len(), d.files, d.covers, d.lyrics
            )
        }
        Kind::Rename(from, to) => format!("{} file(s) renamed to {to}", apply::rename_artist(s, &from, &to)),
        Kind::Covers(_) if !s.cover_embed => "cover art is turned off in Settings".into(),
        Kind::Covers(dirs) => format!("{} file(s) got a cover", apply::albums(s, apply::Pass::COVERS, dirs.as_deref()).covers),
        Kind::Lyrics(_) if !apply::wants_lyrics(s) => "lyrics are turned off in Settings".into(),
        Kind::Lyrics(dirs) => {
            if let Some(dirs) = &dirs {
                // asked for by hand: earlier misses are tried again now, not after the retry delay
                let c = db::open();
                for d in dirs {
                    c.execute("UPDATE tracks SET lyrics_miss=NULL WHERE dir=?1", [d]).unwrap();
                }
            }
            let d = apply::albums(s, apply::Pass::LYRICS, dirs.as_deref());
            format!("lyrics for {} track(s); {} file(s) written", d.lyrics, d.files)
        }
        Kind::Undo(dir) => format!("{} file(s) restored", apply::undo(dir.as_deref())),
        Kind::Full => {
            let mut c = db::open();
            plan(vec![("match", 0, seed("match"))]);
            let scanned = scan::run(&mut c);
            let dirs: Vec<String> = c
                .prepare(&format!("SELECT DISTINCT dir {NEEDS_MATCH}"))
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            plan(vec![("match", dirs.len() as u64, seed("match")), ("albums", 0, seed("albums"))]);

            let (auto, review) = match_dirs(&c, &dirs, s);
            plan(vec![("albums", apply::album_dirs(&c, s, apply::Pass::ALL).len() as u64, seed("albums"))]);
            let d = apply::albums(s, apply::Pass::ALL, None);
            let applied = d.applied;

            let review_total = count(&c, "SELECT COUNT(*) FROM proposals WHERE status IN ('review','manual')");
            c.execute(
                "INSERT INTO stats_snapshots(ts,tracks,bytes,fixed,review,open) VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    db::now(),
                    count(&c, "SELECT COUNT(*) FROM tracks"),
                    count(&c, "SELECT COALESCE(SUM(size),0) FROM tracks"),
                    applied as i64,
                    review_total,
                    count(&c, &format!("SELECT COUNT(DISTINCT dir) FROM tracks WHERE dir NOT IN (SELECT dir FROM tracks WHERE {})", db::HAS_RELEASE)),
                ],
            )
            .unwrap();
            let artists = if s.artist_images { format!("; {}", crate::artist::images(s)) } else { String::new() };
            format!(
                "{scanned} file(s) read, {} album(s) matched: {auto} auto-approved, {review} parked for review; {applied} applied ({} failed); {} file(s) written, {} cover(s), lyrics for {} track(s){artists}",
                dirs.len(), d.failed, d.files, d.covers, d.lyrics
            )
        }
    }
}

/// Accepts standard 5-field cron (prepends seconds) or the crate's 6/7-field form.
pub fn parse_cron(expr: &str) -> Result<cron::Schedule, String> {
    let e = expr.trim();
    let e = if e.split_whitespace().count() == 5 { format!("0 {e}") } else { e.to_string() };
    cron::Schedule::from_str(&e).map_err(|err| err.to_string())
}

pub fn reschedule() {
    WAKE.1.notify_all();
}

pub fn scheduler() {
    loop {
        let s = Settings::load(&db::open());
        let next = (!s.schedule.trim().is_empty()).then(|| parse_cron(&s.schedule).ok()).flatten().and_then(|c| c.upcoming(Local).next());
        let guard = WAKE.0.lock().unwrap();
        let Some(next) = next else {
            drop(WAKE.1.wait(guard).unwrap());
            continue;
        };
        let wait = (next - Local::now()).to_std().unwrap_or_default();
        let (_g, to) = WAKE.1.wait_timeout(guard, wait).unwrap();
        if to.timed_out() {
            if !start(Kind::Full) {
                eprintln!("scheduled run skipped: a job is already running");
            }
            std::thread::sleep(std::time::Duration::from_secs(1)); // don't re-fire the same slot on an early wakeup
        }
    }
}

#[test]
fn eta_math() {
    assert_eq!(eta(0, 10, 0.0, 2.0), 20.0); // seeded before first item
    assert_eq!(eta(5, 10, 10.0, 2.0), 10.0); // 2s/item measured, 5 left
    assert_eq!(eta(10, 10, 30.0, 2.0), 0.0);
    assert!(parse_cron("0 3 * * *").is_ok());
    assert!(parse_cron("nope").is_err());
}
