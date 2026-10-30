//! Prometheus exposition at GET /metrics (public, read-only). Gauges are read from SQLite at scrape
//! time; the only in-memory state is a few counters for things the database doesn't record.
//! Labels carry library names, formats and statuses, never file paths or tag values.

use crate::db::Settings;
use crate::{job, web};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static REMOTE: Mutex<BTreeMap<(&'static str, &'static str), u64>> = Mutex::new(BTreeMap::new());
pub static FILES_WRITTEN: AtomicU64 = AtomicU64::new(0);
pub static FILES_RESTORED: AtomicU64 = AtomicU64::new(0);

/// Count one outbound HTTP call by service and outcome.
pub fn remote<T>(service: &'static str, r: &Result<T, ureq::Error>) {
    let result = match r {
        Ok(_) => "ok",
        Err(ureq::Error::StatusCode(404)) => "not_found",
        Err(ureq::Error::StatusCode(429 | 503)) => "rate_limited",
        Err(_) => "error",
    };
    *REMOTE.lock().unwrap().entry((service, result)).or_default() += 1;
}

fn esc_label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

struct Out(String);

impl Out {
    /// One metric family: HELP/TYPE header then one line per (labels, value).
    fn family(&mut self, name: &str, kind: &str, help: &str, samples: impl IntoIterator<Item = (Vec<(&'static str, String)>, f64)>) {
        let _ = writeln!(self.0, "# HELP xmusic_{name} {help}\n# TYPE xmusic_{name} {kind}");
        for (labels, v) in samples {
            let l: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", esc_label(v))).collect();
            let l = if l.is_empty() { String::new() } else { format!("{{{}}}", l.join(",")) };
            let _ = writeln!(self.0, "xmusic_{name}{l} {v}");
        }
    }

    fn one(&mut self, name: &str, kind: &str, help: &str, v: f64) {
        self.family(name, kind, help, [(vec![], v)]);
    }
}

/// Rows of (label values..., number) from a query whose last column is the value.
fn query(c: &Connection, sql: &str, p: impl rusqlite::Params) -> Vec<(Vec<String>, f64)> {
    let mut st = c.prepare(sql).unwrap();
    let n = st.column_count();
    st.query_map(p, |r| {
        let labels = (0..n - 1).map(|i| r.get::<_, Option<String>>(i).map(Option::unwrap_or_default)).collect::<Result<_, _>>()?;
        Ok((labels, r.get::<_, Option<f64>>(n - 1)?.unwrap_or(0.0)))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn labeled(rows: Vec<(Vec<String>, f64)>, keys: &[&'static str]) -> Vec<(Vec<(&'static str, String)>, f64)> {
    rows.into_iter().map(|(vals, v)| (keys.iter().copied().zip(vals).collect(), v)).collect()
}

pub fn render(c: &Connection) -> String {
    let mut o = Out(String::new());
    o.one("up", "gauge", "Always 1 when xmusic answers a scrape.", 1.0);
    o.family("build_info", "gauge", "xmusic version.", [(vec![("version", env!("CARGO_PKG_VERSION").to_string())], 1.0)]);

    // library
    let per_format = "FROM libraries l JOIN tracks t ON t.library_id=l.id GROUP BY l.name, t.format";
    o.family("tracks", "gauge", "Tracks per library and format.", labeled(query(c, &format!("SELECT l.name, t.format, COUNT(*) {per_format}"), []), &["library", "format"]));
    o.family("size_bytes", "gauge", "File size per library and format.", labeled(query(c, &format!("SELECT l.name, t.format, SUM(t.size) {per_format}"), []), &["library", "format"]));
    let per_lib = "FROM libraries l LEFT JOIN tracks t ON t.library_id=l.id GROUP BY l.name";
    o.family("duration_seconds", "gauge", "Total play time per library.", labeled(query(c, &format!("SELECT l.name, COALESCE(SUM(t.duration),0) {per_lib}"), []), &["library"]));
    o.family("albums", "gauge", "Album folders per library.", labeled(query(c, &format!("SELECT l.name, COUNT(DISTINCT t.dir) {per_lib}"), []), &["library"]));
    o.family("library_enabled", "gauge", "1 when the library is scanned.", labeled(query(c, "SELECT name, enabled FROM libraries", []), &["library"]));
    o.family(
        "library_last_scan_timestamp_seconds",
        "gauge",
        "Unix time of the library's last scan.",
        labeled(query(c, "SELECT name, last_scan FROM libraries WHERE last_scan IS NOT NULL", []), &["library"]),
    );
    o.family(
        "tracks_by_quality",
        "gauge",
        "Tracks per format and quality bucket.",
        labeled(
            query(
                c,
                "SELECT format, CASE WHEN bit_depth>0 THEN printf('%d-bit / %g kHz', bit_depth, sample_rate/1000.0)
                   ELSE printf('~%d kbps', ((bitrate+16)/32)*32) END, COUNT(*) FROM tracks GROUP BY 1,2",
                [],
            ),
            &["format", "quality"],
        ),
    );

    // health
    let small = Settings::load(c).cover_upgrade_below;
    let no_cover = format!("NOT {}", crate::db::HAS_COVER);
    let missing: Vec<(Vec<(&'static str, String)>, f64)> = [
        ("musicbrainz_id", "json_extract(tags,'$.MUSICBRAINZ_ALBUMID') IS NULL"),
        ("cover", &no_cover),
        ("lyrics", "has_lyrics=0"),
        ("title", "json_extract(tags,'$.TITLE') IS NULL"),
        ("artist", "json_extract(tags,'$.ARTIST') IS NULL"),
        ("album", "json_extract(tags,'$.ALBUM') IS NULL"),
    ]
    .into_iter()
    .map(|(field, cond)| {
        let n: f64 = c.query_row(&format!("SELECT COUNT(*) FROM tracks WHERE {cond}"), [], |r| r.get(0)).unwrap();
        (vec![("field", field.to_string())], n)
    })
    .collect();
    o.family("tracks_missing", "gauge", "Tracks lacking a field, cover or lyrics.", missing);
    let small_n: f64 = c
        .query_row("SELECT COUNT(*) FROM tracks WHERE ?1>0 AND has_cover=1 AND cover_px>0 AND cover_px<?1", params![small], |r| r.get(0))
        .unwrap();
    o.one("tracks_small_cover", "gauge", "Tracks whose embedded cover is below the upgrade threshold.", small_n);
    o.family("proposals", "gauge", "Album proposals by status.", labeled(query(c, "SELECT status, COUNT(*) FROM proposals GROUP BY status", []), &["status"]));
    let dupes = web::duplicates(c);
    let groups = dupes["groups"].as_array().cloned().unwrap_or_default();
    // the first copy in each group is the best one; the rest is what could be freed
    let spare: f64 = groups.iter().flat_map(|g| g["dirs"].as_array().into_iter().flatten().skip(1)).map(|d| d["bytes"].as_f64().unwrap_or(0.0)).sum();
    o.one("duplicate_albums", "gauge", "Albums present more than once.", groups.len() as f64);
    o.one("duplicate_bytes", "gauge", "Space used by the extra copies of duplicate albums.", spare);
    o.one("undo_log_entries", "gauge", "File changes that undo can still restore.", c.query_row("SELECT COUNT(*) FROM undo_log", [], |r| r.get(0)).unwrap());

    // jobs
    let jobs = job::status().jobs;
    // one series per running job: `job` is its id, never a path
    let id = |j: &job::JobStatus| vec![("kind", j.kind.clone()), ("job", j.id.to_string())];
    o.one("job_running", "gauge", "Jobs running.", jobs.len() as f64);
    o.family(
        "job_info",
        "gauge",
        "Each running job's kind and phase (1 while running).",
        jobs.iter().map(|j| (vec![("kind", j.kind.clone()), ("job", j.id.to_string()), ("phase", j.phase.clone())], 1.0)),
    );
    o.family(
        "job_progress_ratio",
        "gauge",
        "Progress of each running job's current phase, 0..1.",
        jobs.iter().map(|j| (id(j), if j.total > 0 { j.done as f64 / j.total as f64 } else { 0.0 })),
    );
    o.family("job_eta_seconds", "gauge", "Estimated seconds until each running job finishes.", jobs.iter().map(|j| (id(j), j.eta)));
    o.family(
        "jobs_total",
        "counter",
        "Finished jobs by kind and result.",
        labeled(
            query(c, "SELECT kind, CASE WHEN summary LIKE 'failed:%' THEN 'failed' ELSE 'ok' END, COUNT(*) FROM jobs GROUP BY 1,2", []),
            &["kind", "result"],
        ),
    );
    o.family(
        "last_job_timestamp_seconds",
        "gauge",
        "When the last job of each kind finished.",
        labeled(query(c, "SELECT kind, MAX(finished) FROM jobs GROUP BY kind", []), &["kind"]),
    );
    o.family(
        "last_job_duration_seconds",
        "gauge",
        "How long the last job of each kind took.",
        labeled(query(c, "SELECT kind, finished-started FROM jobs j WHERE id=(SELECT MAX(id) FROM jobs WHERE kind=j.kind)", []), &["kind"]),
    );

    // process counters
    let remote: Vec<_> = REMOTE.lock().unwrap().iter().map(|((s, r), n)| (vec![("service", s.to_string()), ("result", r.to_string())], *n as f64)).collect();
    o.family("remote_requests_total", "counter", "Outbound API calls by service and outcome (since start).", remote);
    o.one("files_written_total", "counter", "Music files xmusic changed (since start).", FILES_WRITTEN.load(Ordering::Relaxed) as f64);
    o.one("files_restored_total", "counter", "Music files restored by undo (since start).", FILES_RESTORED.load(Ordering::Relaxed) as f64);
    o.0
}

#[test]
fn renders() {
    let c = Connection::open_in_memory().unwrap();
    crate::db::init(&c);
    c.execute_batch(
        "INSERT INTO libraries(id,name,path) VALUES(1,'My \"FLAC\"','/m/flac');
         INSERT INTO tracks(library_id,path,dir,size,format,bit_depth,sample_rate,bitrate,duration,has_cover,has_lyrics,tags)
           VALUES(1,'/m/flac/a/1.flac','/m/flac/a',100,'FLAC',16,44100,900,60,1,0,'{\"TITLE\":\"x\"}'),
                 (1,'/m/flac/a/2.flac','/m/flac/a',50,'FLAC',16,44100,900,30,0,0,'{}');
         INSERT INTO jobs(kind,started,finished,summary) VALUES('scan',10,15,'2 read'),('scan',20,21,'failed: boom');",
    )
    .unwrap();
    remote::<()>("musicbrainz", &Err(ureq::Error::StatusCode(503)));
    let out = render(&c);
    for want in [
        r#"xmusic_tracks{library="My \"FLAC\"",format="FLAC"} 2"#,
        r#"xmusic_size_bytes{library="My \"FLAC\"",format="FLAC"} 150"#,
        r#"xmusic_albums{library="My \"FLAC\""} 1"#,
        r#"xmusic_tracks_missing{field="cover"} 1"#,
        r#"xmusic_tracks_missing{field="title"} 1"#,
        r#"xmusic_tracks_by_quality{format="FLAC",quality="16-bit / 44.1 kHz"} 2"#,
        r#"xmusic_jobs_total{kind="scan",result="failed"} 1"#,
        r#"xmusic_last_job_duration_seconds{kind="scan"} 1"#,
        r#"xmusic_remote_requests_total{service="musicbrainz",result="rate_limited"} 1"#,
        "# TYPE xmusic_jobs_total counter",
        "xmusic_job_running 0",
        "# TYPE xmusic_job_eta_seconds gauge",
    ] {
        assert!(out.contains(want), "missing {want}\n{out}");
    }
    assert!(!out.contains("/m/flac"), "no paths in labels");
}
