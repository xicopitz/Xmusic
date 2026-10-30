//! HTTP server: embedded dashboard + JSON API. Routing is a plain match on (method, path segments).
// ponytail: no TLS here; put it behind a reverse proxy for anything beyond the LAN.

use crate::auth::{self, User};
use crate::db::{self, Settings};
use crate::{apply, job, matcher};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::io::{Cursor, Read};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, ResponseBox, Server};

type Resp = Response<Cursor<Vec<u8>>>;

const ASSETS: &[(&str, &str, &[u8])] = &[
    ("daisyui.css", "text/css", include_bytes!("../assets/daisyui.css")),
    ("tailwind-browser.js", "text/javascript", include_bytes!("../assets/tailwind-browser.js")),
    ("chart.umd.min.js", "text/javascript", include_bytes!("../assets/chart.umd.min.js")),
];

pub fn serve() {
    let port = std::env::var("XMUSIC_PORT").unwrap_or_else(|_| "8337".into());
    let server = Arc::new(Server::http(format!("0.0.0.0:{port}")).expect("bind"));
    println!("xmusic listening on :{port}");
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let server = server.clone();
            std::thread::spawn(move || {
                while let Ok(mut req) = server.recv() {
                    let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| files(&mut req).unwrap_or_else(|| handle(&mut req).boxed())))
                        .unwrap_or_else(|_| err(500, "internal error").boxed());
                    let _ = req.respond(resp);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k, v).unwrap()
}

fn json_resp(code: u16, v: Value) -> Resp {
    Response::from_data(v.to_string()).with_status_code(code).with_header(header("Content-Type", "application/json"))
}

fn ok(v: Value) -> Resp {
    json_resp(200, v)
}

fn err(code: u16, msg: &str) -> Resp {
    json_resp(code, json!({ "error": msg }))
}

/// A job start as a reply: 409 says why it couldn't start.
fn started(r: Result<(), String>) -> Resp {
    match r {
        Ok(()) => ok(json!({"ok": true})),
        Err(e) => err(409, &e),
    }
}

fn with_session(r: Resp, token: &str) -> Resp {
    r.with_header(header("Set-Cookie", &format!("{}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000", auth::COOKIE)))
}

fn cookie(req: &Request) -> Option<String> {
    req.headers()
        .iter()
        .filter(|h| h.field.equiv("Cookie"))
        .flat_map(|h| h.value.as_str().split(';'))
        .find_map(|kv| kv.trim().strip_prefix(&format!("{}=", auth::COOKIE)).map(String::from))
}

/// Any query result as a JSON array of objects keyed by column name.
pub fn rows(c: &Connection, sql: &str, p: impl rusqlite::Params) -> Vec<Value> {
    let mut st = c.prepare(sql).unwrap();
    let names: Vec<String> = st.column_names().map(String::from).collect();
    st.query_map(p, |r| {
        let mut o = serde_json::Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match r.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(x) => json!(x),
                ValueRef::Real(x) => json!(x),
                ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                ValueRef::Blob(_) => Value::Null,
            };
            o.insert(n.clone(), v);
        }
        Ok(Value::Object(o))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// Lidarr events whose files moved or went away: a library scan catches up with them.
const HOOK_SCAN_EVENTS: [&str; 5] = ["Rename", "Retag", "TrackFileDelete", "AlbumDelete", "ArtistDelete"];

/// Album folders an import hook call is about: Lidarr's `trackFiles[].path`, or `{"path": …}` from a script,
/// kept only inside a library. None = nothing was imported (any other event).
// ponytail: paths are taken as xmusic sees them; Lidarr mounts the music at the same /music. Add a prefix
// map setting if the two containers ever mount it differently.
fn hook_dirs(body: &Value, libraries: &[String]) -> Option<Vec<String>> {
    let paths: Vec<&str> = match body["path"].as_str() {
        Some(p) => vec![p],
        None => body["trackFiles"].as_array()?.iter().filter_map(|f| f["path"].as_str()).collect(),
    };
    let mut dirs: Vec<String> = paths
        .iter()
        .map(std::path::Path::new)
        .filter(|p| libraries.iter().any(|l| p.starts_with(l)))
        .map(|p| if body["path"].is_string() { p.to_string_lossy().trim_end_matches('/').to_string() } else { crate::scan::album_dir(p) })
        .collect();
    dirs.sort();
    dirs.dedup();
    Some(dirs)
}

/// Standard base64 (as in a Basic auth header) → bytes; None if malformed.
fn base64(s: &str) -> Option<Vec<u8>> {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut out, mut acc, mut bits) = (vec![], 0u32, 0);
    for c in s.trim().trim_end_matches('=').bytes() {
        acc = acc << 6 | ABC.iter().position(|&a| a == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The hook token: `?token=` in the URL, or the password of a Basic auth header (Lidarr's webhook has
/// Username/Password fields but no header field); the username is ignored.
fn hook_token(query: &str, authorization: Option<&str>) -> String {
    let basic = || {
        let raw = base64(authorization?.strip_prefix("Basic ")?)?;
        Some(String::from_utf8(raw).ok()?.split_once(':')?.1.to_string())
    };
    query.split('&').find_map(|kv| kv.strip_prefix("token=")).map(String::from).or_else(basic).unwrap_or_default()
}

fn hook(c: &Connection, query: &str, authorization: Option<&str>, body: &Value) -> Resp {
    let want = Settings::load(c).hook_token;
    if want.is_empty() {
        return err(404, "the import hook is off: set a token in Settings → Integrations");
    }
    let got = hook_token(query, authorization);
    // compare every byte, so the time taken doesn't tell how much of a guess was right
    if got.len() != want.len() || got.bytes().zip(want.bytes()).fold(0, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return err(403, "wrong token");
    }
    let event = body["eventType"].as_str().unwrap_or_default();
    if event == "Test" {
        return ok(json!({"ok": true}));
    }
    let libraries: Vec<String> = c
        .prepare("SELECT path FROM libraries WHERE enabled=1")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    match hook_dirs(body, &libraries) {
        Some(dirs) if dirs.is_empty() => err(400, "no file of this import is inside an enabled library"),
        Some(dirs) => {
            let n = dirs.len();
            let started = job::import(dirs);
            ok(json!({ "albums": n, "started": started, "queued": !started }))
        }
        None if HOOK_SCAN_EVENTS.contains(&event) => ok(json!({ "scan": job::start(job::Kind::Scan).is_ok() })),
        None => ok(json!({ "ignored": event })),
    }
}

#[test]
fn hook_payloads() {
    let libs = vec!["/music".to_string()];
    let lidarr = json!({ "eventType": "Download", "trackFiles": [
        { "path": "/music/Tuff/Fire/CD1/01.flac" }, { "path": "/music/Tuff/Fire/CD2/01.flac" },
        { "path": "/music/Tuff/Water/01.flac" }, { "path": "/downloads/x.flac" } ] });
    assert_eq!(hook_dirs(&lidarr, &libs).unwrap(), vec!["/music/Tuff/Fire", "/music/Tuff/Water"], "disc folders fold, outside paths drop");
    assert_eq!(hook_dirs(&json!({ "path": "/music/Tuff/Fire/" }), &libs).unwrap(), vec!["/music/Tuff/Fire"]);
    assert!(hook_dirs(&json!({ "eventType": "Grab" }), &libs).is_none());
    // token in the URL, or as the Basic auth password ("lidarr:s3cret")
    assert_eq!(hook_token("a=1&token=abc", None), "abc");
    assert_eq!(hook_token("", Some("Basic bGlkYXJyOnMzY3JldA==")), "s3cret");
    assert_eq!(hook_token("", Some("Basic !!")), "");
}

/// A pasted release: an MBID or musicbrainz.org/release/<mbid> link, a discogs.com/…/release/<n>-slug link,
/// or an id as stored (`discogs:<n>`). Discogs ids come back as `discogs:<n>`.
fn release_id(input: &str) -> Option<String> {
    let input = input.trim();
    let discogs = input.strip_prefix(matcher::DISCOGS).or_else(|| input.contains("discogs.com").then(|| input.split("/release/").nth(1)).flatten());
    if let Some(rest) = discogs {
        let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
        return (!n.is_empty()).then(|| format!("{}{n}", matcher::DISCOGS));
    }
    let mbid = input.split(['?', '#']).next()?.trim_end_matches('/').rsplit('/').next()?;
    (mbid.len() == 36).then(|| mbid.to_string())
}

#[test]
fn parses_pasted_release() {
    let mb = "1c3b758d-6620-42e9-bfaf-15b63e0b0128";
    assert_eq!(release_id(mb).as_deref(), Some(mb));
    assert_eq!(release_id(&format!("https://musicbrainz.org/release/{mb}/")).as_deref(), Some(mb));
    assert_eq!(release_id("https://www.discogs.com/release/2094476-The-White-Stripes-3").as_deref(), Some("discogs:2094476"));
    assert_eq!(release_id("https://www.discogs.com/pt_BR/release/2094476").as_deref(), Some("discogs:2094476"));
    assert_eq!(release_id("discogs:42").as_deref(), Some("discogs:42"));
    assert_eq!(release_id("https://www.discogs.com/master/123-x"), None);
    assert_eq!(release_id("hello"), None);
}

/// Database backup (download) and restore (upload). Whole files, possibly large, so they're streamed and
/// skip the JSON handling. Admin only. None for every other route.
fn files(req: &mut Request) -> Option<ResponseBox> {
    let path = req.url().split('?').next().unwrap_or_default();
    let backup = *req.method() == Method::Get && path == "/api/backup";
    if !backup && !(*req.method() == Method::Post && path == "/api/restore") {
        return None;
    }
    let fail = |code, msg: &str| Some(err(code, msg).boxed());
    let c = db::open();
    if !auth::session_user(&c, &cookie(req).unwrap_or_default()).is_some_and(|u| u.admin) {
        return fail(403, "admin only");
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let tmp = db::data_dir().join(format!("{}-{nanos}.tmp", if backup { "backup" } else { "restore" }));
    if backup {
        if let Err(e) = db::backup(&c, &tmp) {
            return fail(500, &e);
        }
        let file = std::fs::File::open(&tmp);
        std::fs::remove_file(&tmp).ok(); // the open handle keeps the data until the download ends
        let name = format!("xmusic-{}.db", chrono::Local::now().format("%Y-%m-%d"));
        return Some(
            Response::from_file(file.ok()?)
                .with_header(header("Content-Type", "application/vnd.sqlite3"))
                .with_header(header("Content-Disposition", &format!("attachment; filename=\"{name}\"")))
                .boxed(),
        );
    }
    // like JSON, a cross-site form can't send this type without a CORS preflight
    if !req.headers().iter().any(|h| h.field.equiv("Content-Type") && h.value.as_str() == "application/octet-stream") {
        return fail(415, "application/octet-stream required");
    }
    if job::running() {
        return fail(409, "a job is running; restore when it has finished");
    }
    let res = std::fs::File::create(&tmp)
        .and_then(|mut f| std::io::copy(req.as_reader(), &mut f))
        .map_err(|e| format!("upload: {e}"))
        .and_then(|_| db::restore(&tmp));
    std::fs::remove_file(&tmp).ok();
    if let Err(e) = res {
        return fail(400, &e);
    }
    job::reschedule();
    crate::watch::reload();
    job::start(job::Kind::Scan).ok(); // the library may have changed since the backup
    Some(ok(json!({"ok": true})).boxed())
}

fn handle(req: &mut Request) -> Resp {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or_default().to_string();
    let query = url.split_once('?').map(|x| x.1.to_string()).unwrap_or_default();
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();

    // public on purpose: Prometheus can't log in. Counts and library names only (see metrics.rs)
    if method == Method::Get && path == "/metrics" {
        return Response::from_data(crate::metrics::render(&db::open())).with_header(header("Content-Type", "text/plain; version=0.0.4"));
    }
    if method == Method::Get && !path.starts_with("/api/") {
        if let ["assets", name] = seg.as_slice() {
            return match ASSETS.iter().find(|a| a.0 == *name) {
                Some((_, ct, data)) => Response::from_data(data.to_vec())
                    .with_header(header("Content-Type", ct))
                    .with_header(header("Cache-Control", "public, max-age=604800")),
                None => err(404, "not found"),
            };
        }
        // no-cache: after a deploy the browser must not keep running the old UI
        return Response::from_data(include_str!("dashboard.html"))
            .with_header(header("Content-Type", "text/html; charset=utf-8"))
            .with_header(header("Cache-Control", "no-cache"));
    }

    // JSON-only writes: a cross-site form can't send application/json without a CORS preflight
    let body: Value = if method == Method::Get {
        Value::Null
    } else {
        let is_json = req.headers().iter().any(|h| h.field.equiv("Content-Type") && h.value.as_str().starts_with("application/json"));
        if !is_json {
            return err(415, "application/json required");
        }
        let mut buf = String::new();
        if req.as_reader().take(1 << 20).read_to_string(&mut buf).is_err() {
            return err(400, "bad body");
        }
        if buf.trim().is_empty() { json!({}) } else {
            match serde_json::from_str(&buf) {
                Ok(v) => v,
                Err(_) => return err(400, "invalid json"),
            }
        }
    };
    let s = |k: &str| body[k].as_str().unwrap_or_default().to_string();
    let c = db::open();
    let ip = req.remote_addr().map(|a| a.ip()).unwrap_or([0, 0, 0, 0].into());

    // public routes
    match (&method, seg.as_slice()) {
        // public so Lidarr can call it; the token in the URL is the login
        (Method::Post, ["api", "hook", "import"]) => {
            let basic = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.as_str().to_string());
            return hook(&c, &query, basic.as_deref(), &body);
        }
        (Method::Get, ["api", "setup"]) => return ok(json!({ "needs_setup": auth::user_count(&c) == 0 })),
        (Method::Post, ["api", "setup"]) => {
            if auth::user_count(&c) > 0 {
                return err(403, "already set up");
            }
            return match auth::create_user(&c, &s("name"), &s("password"), "admin") {
                Ok(id) => with_session(ok(json!({"ok": true})), &auth::new_session(&c, id)),
                Err(e) => err(400, &e),
            };
        }
        (Method::Post, ["api", "login"]) => {
            if auth::throttled(ip) {
                return err(429, "too many attempts, wait a minute");
            }
            let tok = auth::login(&c, &s("name"), &s("password"));
            auth::record(ip, tok.is_some());
            return match tok {
                Some(t) => with_session(ok(json!({"ok": true})), &t),
                None => err(401, "wrong user or password"),
            };
        }
        _ => {}
    }

    let token = cookie(req).unwrap_or_default();
    let Some(user) = auth::session_user(&c, &token) else { return err(401, "login required") };
    api(&c, &method, &seg, &query, &body, &user, &token)
}

fn api(c: &Connection, method: &Method, seg: &[&str], query: &str, body: &Value, user: &User, token: &str) -> Resp {
    let s = |k: &str| body[k].as_str().unwrap_or_default().to_string();
    let admin_only = !user.admin;
    match (method, seg) {
        (Method::Post, ["api", "logout"]) => {
            auth::logout(c, token);
            ok(json!({"ok": true})).with_header(header("Set-Cookie", &format!("{}=; Path=/; Max-Age=0", auth::COOKIE)))
        }
        (Method::Get, ["api", "me"]) => ok(json!({ "name": user.name, "admin": user.admin })),
        (Method::Get, ["api", "stats"]) => ok(stats(c)),
        (Method::Get, ["api", "history"]) => ok(json!({
            "snapshots": rows(c, "SELECT * FROM (SELECT * FROM stats_snapshots ORDER BY ts DESC LIMIT 60) ORDER BY ts", []),
            "jobs": rows(c, "SELECT * FROM jobs ORDER BY id DESC LIMIT 30", []),
        })),
        (Method::Get, ["api", "job"]) => {
            let mut st = serde_json::to_value(job::status()).unwrap();
            st["max"] = json!(job::max_jobs());
            ok(st)
        }
        (Method::Post, ["api", "job", "cancel"]) => if job::cancel(body["id"].as_u64()) { ok(json!({"ok": true})) } else { err(409, "no such job is running") },
        (Method::Post, ["api", "run"]) => {
            let kind = match s("kind").as_str() {
                "full" => job::Kind::Full,
                "apply" => job::Kind::Apply,
                "scan" => job::Kind::Scan,
                "covers" => job::Kind::Covers(None),
                "lyrics" => job::Kind::Lyrics(None),
                "artists" => job::Kind::ArtistImages,
                "rules" if user.admin => job::Kind::Rules,
                _ => return err(400, "unknown or forbidden job kind"),
            };
            started(job::start(kind))
        }
        (Method::Post, ["api", "undo"]) => {
            let dir = body["dir"].as_str().map(String::from);
            if dir.is_none() && admin_only {
                return err(403, "admin only");
            }
            started(job::start(job::Kind::Undo(dir)))
        }
        (Method::Get, ["api", "issues"]) => ok(issues(c)),
        (Method::Post, ["api", "rematch"]) => {
            let dir = s("dir");
            if c.query_row("SELECT 1 FROM tracks WHERE dir=?1 LIMIT 1", [&dir], |_| Ok(())).is_err() {
                return err(404, "album folder not found in the library");
            }
            started(job::start(job::Kind::Rematch(vec![dir])))
        }
        (Method::Post, ["api", "album", "fetch"]) => {
            let dir = s("dir");
            if c.query_row("SELECT 1 FROM tracks WHERE dir=?1 LIMIT 1", [&dir], |_| Ok(())).is_err() {
                return err(404, "album folder not found in the library");
            }
            let kind = match s("what").as_str() {
                "covers" => job::Kind::Covers(Some(vec![dir])),
                "lyrics" => job::Kind::Lyrics(Some(vec![dir])),
                _ => return err(400, "what must be covers or lyrics"),
            };
            started(job::start(kind))
        }
        (Method::Post, ["api", "library"]) => ok(library(c, body)),
        (Method::Post, ["api", "library", "album"]) => ok(album(c, &s("dir"))),
        (Method::Get, ["api", "duplicates"]) => ok(duplicates(c)),
        (Method::Get, ["api", "health"]) => ok(crate::health::report(c)),
        (Method::Get, ["api", "cover", id]) => cover(c, id),
        (Method::Get, ["api", "proposals"]) => {
            let status = query.split('&').find_map(|kv| kv.strip_prefix("status=")).unwrap_or("review");
            ok(json!(rows(
                c,
                "SELECT id,dir,status,confidence,release_id,release_title,note,updated,
                        (SELECT COUNT(*) FROM tracks t WHERE t.dir=p.dir) AS tracks
                 FROM proposals p WHERE status=?1 ORDER BY confidence DESC LIMIT 500",
                [status]
            )))
        }
        (Method::Post, ["api", "proposals", "bulk"]) => bulk(c, body),
        (Method::Get, ["api", "proposals", id]) => proposal_detail(c, id),
        (Method::Post, ["api", "proposals", id, action]) => proposal_action(c, id, action, body),

        // admin
        _ if admin_only => err(403, "admin only"),
        (Method::Get, ["api", "settings"]) => ok(Settings::load(c).masked()),
        (Method::Put, ["api", "settings"]) => {
            let new: Settings = match serde_json::from_value(Settings::load(c).unmask(body.clone())) {
                Ok(v) => v,
                Err(e) => return err(400, &e.to_string()),
            };
            if !new.schedule.trim().is_empty()
                && let Err(e) = job::parse_cron(&new.schedule) {
                    return err(400, &format!("schedule: {e}"));
                }
            if let Some(r) = new.rewrites.iter().find(|r| regex::Regex::new(&r.pattern).is_err()) {
                return err(400, &format!("invalid regex: {}", r.pattern));
            }
            new.save(c);
            job::reschedule();
            crate::watch::reload();
            ok(json!({"ok": true}))
        }
        (Method::Post, ["api", "notify", "test"]) => {
            let st = Settings::load(c);
            let link = (!st.public_url.trim().is_empty()).then(|| format!("{}/#review", st.public_url.trim().trim_end_matches('/')));
            match crate::remote::gotify(&st.contact_email, &st.gotify_url, &st.gotify_token, st.gotify_priority, "xmusic test", "Notifications work.", link.as_deref()) {
                Ok(()) => ok(json!({"ok": true})),
                Err(e) => err(502, &e),
            }
        }
        (Method::Post, ["api", "acoustid", "test"]) => {
            let st = Settings::load(c);
            let fpcalc = crate::remote::fpcalc_version();
            let key = if st.acoustid_key.trim().is_empty() { Err("no key set".to_string()) } else { crate::remote::acoustid_check(&st.contact_email, st.acoustid_key.trim()) };
            ok(json!({ "key_ok": key.is_ok(), "key": key.err(), "fpcalc": fpcalc }))
        }
        (Method::Post, ["api", "navidrome", "test"]) => {
            let st = Settings::load(c);
            match crate::remote::navidrome_scan(&st.contact_email, &st.navidrome_url, &st.navidrome_user, &st.navidrome_password) {
                Ok(()) => ok(json!({"ok": true})),
                Err(e) => err(502, &e),
            }
        }
        (Method::Post, ["api", "health", "discs"]) => {
            let dirs = body["dir"].as_str().map(|d| vec![d.to_string()]);
            started(job::start(job::Kind::Discs(dirs)))
        }
        (Method::Post, ["api", "health", "spelling"]) => {
            let to = s("to");
            let from: Vec<String> = body["from"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).filter(|v| *v != to).map(String::from).collect();
            if to.trim().is_empty() || from.is_empty() {
                return err(400, "pick a spelling and at least one other");
            }
            if let Err(e) = job::start(job::Kind::Rename(from.clone(), to.clone())) {
                return err(409, &e);
            }
            // a rewrite rule keeps the spelling when a match or a new file brings an old one back
            let mut st = Settings::load(c);
            // drop earlier rules that renamed something *to* a spelling now being replaced, or the two would fight
            st.rewrites.retain(|r| !(apply::ARTIST_FIELDS.contains(&r.field.as_str()) && from.iter().any(|f| f.replace('$', "$$") == r.replace)));
            let pattern = format!("^(?:{})$", from.iter().map(|f| regex::escape(f)).collect::<Vec<_>>().join("|"));
            for field in apply::ARTIST_FIELDS {
                let rule = crate::db::Rewrite { field: field.into(), pattern: pattern.clone(), replace: to.replace('$', "$$") };
                if !st.rewrites.iter().any(|r| r.field == rule.field && r.pattern == rule.pattern) {
                    st.rewrites.push(rule);
                }
            }
            st.save(c);
            ok(json!({"ok": true}))
        }
        (Method::Post, ["api", "browse"]) => browse(&s("path")),
        (Method::Post, ["api", "duplicates", "keep"]) => keep_copy(c, &s("dir")),
        (Method::Post, ["api", "trash", id, "restore"]) => match apply::restore(c, id.parse().unwrap_or(0)) {
            Ok(()) => {
                job::start(job::Kind::Scan).ok();
                job::navidrome(&Settings::load(c));
                ok(json!({"ok": true}))
            }
            Err(e) => err(409, &e),
        },
        (Method::Get, ["api", "libraries"]) => ok(json!(rows(
            c,
            "SELECT l.*, (SELECT COUNT(*) FROM tracks t WHERE t.library_id=l.id) AS tracks FROM libraries l ORDER BY name",
            []
        ))),
        (Method::Post, ["api", "libraries"]) => {
            let path = s("path").trim_end_matches('/').to_string();
            let p = std::path::Path::new(&path);
            if !p.is_absolute() || !p.is_dir() {
                return err(400, "path must be an existing absolute directory inside the container");
            }
            if std::fs::read_dir(p).is_err() || std::fs::metadata(p).is_ok_and(|m| m.permissions().readonly()) {
                return err(400, "path must be readable and writable");
            }
            let name = Some(s("name")).filter(|n| !n.trim().is_empty()).unwrap_or(path.clone());
            match c.execute("INSERT INTO libraries(name,path) VALUES(?1,?2)", params![name, path]) {
                Ok(_) => {
                    crate::watch::reload();
                    ok(json!({"ok": true}))
                }
                Err(_) => err(400, "library already exists"),
            }
        }
        (Method::Post, ["api", "libraries", id, "toggle"]) => {
            c.execute("UPDATE libraries SET enabled=1-enabled WHERE id=?1", [id]).unwrap();
            crate::watch::reload();
            ok(json!({"ok": true}))
        }
        (Method::Delete, ["api", "libraries", id]) => {
            c.execute("DELETE FROM proposals WHERE dir IN (SELECT dir FROM tracks WHERE library_id=?1)", [id]).unwrap();
            c.execute("DELETE FROM libraries WHERE id=?1", [id]).unwrap();
            crate::watch::reload();
            ok(json!({"ok": true}))
        }
        (Method::Get, ["api", "users"]) => ok(json!(rows(c, "SELECT id,name,role FROM users ORDER BY name", []))),
        (Method::Post, ["api", "users"]) => {
            let role = if s("role") == "admin" { "admin" } else { "user" };
            match auth::create_user(c, &s("name"), &s("password"), role) {
                Ok(_) => ok(json!({"ok": true})),
                Err(e) => err(400, &e),
            }
        }
        (Method::Post, ["api", "users", id, "password"]) => {
            let name: String = match c.query_row("SELECT name FROM users WHERE id=?1", [id], |r| r.get(0)) {
                Ok(n) => n,
                Err(_) => return err(404, "no such user"),
            };
            match auth::set_password(c, &name, &s("password")) {
                Ok(()) => ok(json!({"ok": true})),
                Err(e) => err(400, &e),
            }
        }
        (Method::Delete, ["api", "users", id]) => {
            if id.parse::<i64>().ok() == Some(user.id) {
                return err(400, "you can't delete yourself");
            }
            c.execute("DELETE FROM users WHERE id=?1", [id]).unwrap();
            ok(json!({"ok": true}))
        }
        _ => err(404, "not found"),
    }
}

/// Folder picker for adding libraries. Never lists anything outside the music root.
fn browse(path: &str) -> Resp {
    let root = std::env::var("XMUSIC_MUSIC").unwrap_or_else(|_| "/music".into());
    let root = std::fs::canonicalize(&root).or_else(|_| std::fs::canonicalize("/")).unwrap();
    let want = if path.is_empty() { root.clone() } else { std::path::PathBuf::from(path) };
    // canonicalize resolves `..` and symlinks, so the prefix check can't be walked around
    let dir = match std::fs::canonicalize(&want) {
        Ok(d) if d.starts_with(&root) && d.is_dir() => d,
        _ => return err(400, "not a folder inside the music root"),
    };
    let Ok(entries) = std::fs::read_dir(&dir) else { return err(400, "folder is not readable") };
    let (mut dirs, mut audio) = (vec![], 0);
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if !e.file_name().to_string_lossy().starts_with('.') {
                dirs.push(e.file_name().to_string_lossy().into_owned());
            }
        } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| crate::tags::AUDIO_EXT.contains(&x.to_ascii_lowercase().as_str())) {
            audio += 1;
        }
    }
    dirs.sort_by_key(|d| d.to_lowercase());
    let parent = (dir != root).then(|| dir.parent().map(|p| p.to_string_lossy().into_owned())).flatten();
    ok(json!({ "path": dir.to_string_lossy(), "root": root.to_string_lossy(), "parent": parent, "dirs": dirs, "audio_files": audio }))
}

fn stats(c: &Connection) -> Value {
    json!({
        "totals": rows(c, &format!("SELECT COUNT(*) AS tracks, COUNT(DISTINCT dir) AS albums, COALESCE(SUM(size),0) AS bytes, COALESCE(SUM(duration),0) AS seconds,
            SUM(json_extract(tags,'$.MUSICBRAINZ_ALBUMID') IS NOT NULL) AS with_mbid, SUM({}) AS with_cover, SUM(has_lyrics) AS with_lyrics FROM tracks", db::HAS_COVER), [])[0],
        "formats": rows(c, "SELECT format, COUNT(*) AS tracks, SUM(size) AS bytes, SUM(duration) AS seconds, CAST(AVG(bitrate) AS INT) AS avg_bitrate
            FROM tracks GROUP BY format ORDER BY tracks DESC", []),
        "quality": rows(c, "SELECT format, CASE WHEN bit_depth>0 THEN printf('%d-bit / %g kHz', bit_depth, sample_rate/1000.0)
                ELSE printf('~%d kbps', ((bitrate+16)/32)*32) END AS quality, COUNT(*) AS tracks
            FROM tracks GROUP BY 1,2 ORDER BY tracks DESC", []),
        "libraries": rows(c, "SELECT l.id, l.name, l.path, l.enabled, l.last_scan, t.format, COUNT(t.id) AS tracks, COALESCE(SUM(t.size),0) AS bytes
            FROM libraries l LEFT JOIN tracks t ON t.library_id=l.id GROUP BY l.id, t.format ORDER BY l.name, tracks DESC", []),
        "decades": rows(c, "SELECT (CAST(substr(json_extract(tags,'$.DATE'),1,4) AS INT)/10)*10 AS decade, COUNT(DISTINCT dir) AS albums
            FROM tracks WHERE json_extract(tags,'$.DATE') GLOB '[12][0-9][0-9][0-9]*' GROUP BY 1 ORDER BY 1", []),
        "queue": rows(c, "SELECT status, COUNT(*) AS n FROM proposals GROUP BY status", []),
    })
}

fn issues(c: &Connection) -> Value {
    let small = Settings::load(c).cover_upgrade_below;
    let missing = |k: &str| format!("SUM(json_extract(tags,'$.{k}') IS NULL) AS no_{}", k.to_lowercase());
    let has_cover = db::HAS_COVER;
    json!({
        "counts": rows(c, &format!("SELECT {}, {}, {}, {}, SUM(NOT {has_cover}) AS no_cover, SUM(has_lyrics=0) AS no_lyrics,
            SUM(has_cover=1 AND cover_px>0 AND cover_px<{small}) AS small_cover FROM tracks",
            missing("TITLE"), missing("ARTIST"), missing("ALBUM"), missing("MUSICBRAINZ_ALBUMID")), [])[0],
        "albums": rows(c, &format!("SELECT dir, COUNT(*) AS tracks,
                SUM(json_extract(tags,'$.MUSICBRAINZ_ALBUMID') IS NULL) AS no_mbid, SUM(NOT {has_cover}) AS no_cover, SUM(has_lyrics=0) AS no_lyrics,
                COUNT(DISTINCT json_extract(tags,'$.ALBUMARTIST')) > 1 OR COUNT(DISTINCT json_extract(tags,'$.DATE')) > 1 AS inconsistent,
                MAX(CAST(json_extract(tags,'$.TRACKNUMBER') AS INT)) > COUNT(*) AS gaps
            FROM tracks GROUP BY dir HAVING no_mbid>0 OR no_cover>0 OR inconsistent OR gaps ORDER BY dir LIMIT 500"), []),
    })
}

const ARTIST: &str = "COALESCE(json_extract(tags,'$.ALBUMARTIST'), json_extract(tags,'$.ARTIST'))";

/// Albums (one per folder) or artists, optionally filtered by a search string or one artist. 60 per page.
fn library(c: &Connection, body: &Value) -> Value {
    let q = format!("%{}%", body["q"].as_str().unwrap_or_default().trim().to_lowercase());
    let artist = body["artist"].as_str().unwrap_or_default();
    let offset = body["offset"].as_i64().unwrap_or(0).max(0);
    // a folder matches when any of its tracks matches, then the whole album is listed
    let matching = format!(
        "SELECT dir FROM tracks WHERE (?1='%%' OR lower(json_extract(tags,'$.ALBUM')) LIKE ?1 OR lower(json_extract(tags,'$.ARTIST')) LIKE ?1
           OR lower(json_extract(tags,'$.ALBUMARTIST')) LIKE ?1 OR lower(json_extract(tags,'$.TITLE')) LIKE ?1 OR lower(dir) LIKE ?1)
         AND (?2='' OR {ARTIST}=?2)"
    );
    if body["view"] == "artists" {
        return json!(rows(
            c,
            &format!("SELECT {ARTIST} AS artist, COUNT(DISTINCT dir) AS albums, COUNT(*) AS tracks FROM tracks
                      WHERE dir IN ({matching}) GROUP BY 1 ORDER BY lower(COALESCE(artist,'')) LIMIT 200 OFFSET ?3"),
            params![q, artist, offset]
        ));
    }
    let has_release = db::HAS_RELEASE;
    json!(rows(
        c,
        &format!("SELECT dir, MAX(json_extract(tags,'$.ALBUM')) AS album, MAX({ARTIST}) AS artist,
                    MAX(substr(json_extract(tags,'$.DATE'),1,4)) AS year, GROUP_CONCAT(DISTINCT format) AS formats,
                    COUNT(*) AS tracks, SUM(size) AS bytes, SUM(duration) AS seconds,
                    MIN({has_release}) AS tagged,
                    COALESCE(MIN(CASE WHEN has_cover=1 THEN id END), MIN(id)) AS cover_id
                  FROM tracks WHERE dir IN ({matching}) GROUP BY dir
                  ORDER BY lower(COALESCE(artist,'')), year, lower(COALESCE(album, dir)) LIMIT 61 OFFSET ?3"),
        params![q, artist, offset]
    ))
}

fn album(c: &Connection, dir: &str) -> Value {
    let tracks = rows(
        c,
        "SELECT id, path, format, bitrate, sample_rate, bit_depth, duration, size, has_cover, cover_px, has_lyrics, tags
         FROM tracks WHERE dir=?1
         ORDER BY CAST(json_extract(tags,'$.DISCNUMBER') AS INT), CAST(json_extract(tags,'$.TRACKNUMBER') AS INT), path",
        [dir],
    );
    let proposal = rows(c, "SELECT id, status FROM proposals WHERE dir=?1", [dir]).into_iter().next();
    // a cover.jpg next to the files counts too: `cover()` serves it when no track has embedded art
    let cover_file: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM cover_files WHERE dir=?1)", [dir], |r| r.get(0)).unwrap();
    json!({ "dir": dir, "tracks": tracks, "proposal": proposal, "cover_file": cover_file })
}

/// Embedded front cover of a track, else cover.jpg / folder.jpg next to it.
fn cover(c: &Connection, id: &str) -> Resp {
    let Ok(path) = c.query_row("SELECT path FROM tracks WHERE id=?1", [id], |r| r.get::<_, String>(0)) else { return err(404, "no such track") };
    let path = std::path::Path::new(&path);
    // ponytail: full-size covers, no thumbnails; the grid lazy-loads and the browser caches them
    let found = crate::tags::read_cover(path).or_else(|| {
        crate::scan::COVER_FILES.iter().find_map(|n| {
            let p = path.parent()?.join(n);
            let mime = if n.ends_with("png") { "image/png" } else { "image/jpeg" };
            std::fs::read(p).ok().map(|b| (b, mime.to_string()))
        })
    });
    match found {
        Some((bytes, mime)) => Response::from_data(bytes).with_header(header("Content-Type", &mime)).with_header(header("Cache-Control", "private, max-age=3600")),
        None => err(404, "no cover"),
    }
}

/// Albums present more than once: same MusicBrainz release, or same artist + album name
/// (e.g. a FLAC and an MP3 copy), plus folders holding more than one format.
pub fn duplicates(c: &Connection) -> Value {
    let dirs = rows(
        c,
        &format!("SELECT dir, MAX(json_extract(tags,'$.MUSICBRAINZ_ALBUMID')) AS mbid, MAX({ARTIST}) AS artist,
                    MAX(json_extract(tags,'$.ALBUM')) AS album, GROUP_CONCAT(DISTINCT format) AS formats,
                    COUNT(DISTINCT format) AS nformats, COUNT(*) AS tracks, SUM(size) AS bytes, AVG(bitrate) AS bitrate,
                    MAX(bit_depth) AS bit_depth, COALESCE(MIN(CASE WHEN has_cover=1 THEN id END), MIN(id)) AS cover_id,
                    GROUP_CONCAT(DISTINCT CAST(COALESCE(json_extract(tags,'$.DISCNUMBER'),1) AS INTEGER)) AS discs
                  FROM tracks GROUP BY dir"),
        [],
    );
    // union folders that share a release id or a normalized artist + album name, so a tagged
    // FLAC copy and an untagged MP3 copy of the same album still end up together
    let norm = |v: &Value| v.as_str().unwrap_or_default().trim().to_lowercase();
    let mut parent: Vec<usize> = (0..dirs.len()).collect();
    fn root(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    let mut first: std::collections::HashMap<String, usize> = Default::default();
    for (i, d) in dirs.iter().enumerate() {
        // disc set in the key: "Disc 1 X" and "Disc 2 X" folders are halves of one album, not copies
        let mut discs: Vec<&str> = d["discs"].as_str().unwrap_or_default().split(',').collect();
        discs.sort();
        let discs = discs.join(",");
        let mut keys = vec![];
        if let Some(id) = d["mbid"].as_str() {
            keys.push(format!("id:{id}|{discs}"));
        }
        if !norm(&d["album"]).is_empty() {
            keys.push(format!("name:{}|{}|{discs}", norm(&d["artist"]), norm(&d["album"])));
        }
        for k in keys {
            match first.get(&k) {
                Some(&j) => {
                    let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                    parent[a] = b;
                }
                None => {
                    first.insert(k, i);
                }
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Vec<&Value>> = Default::default();
    for (i, d) in dirs.iter().enumerate() {
        groups.entry(root(&mut parent, i)).or_default().push(d);
    }
    let mut out: Vec<Value> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            g.sort_by(|a, b| quality(b).total_cmp(&quality(a)));
            let same_id = g[0]["mbid"].is_string() && g.iter().all(|d| d["mbid"] == g[0]["mbid"]);
            let reason = if same_id { "same MusicBrainz release" } else { "same artist and album name" };
            json!({ "reason": reason, "artist": g[0]["artist"], "album": g[0]["album"], "dirs": g })
        })
        .collect();
    out.sort_by_key(|g| norm(&g["artist"]) + &norm(&g["album"]));
    let mixed: Vec<&Value> = dirs.iter().filter(|d| d["nformats"].as_i64() > Some(1)).collect();
    json!({ "groups": out, "mixed_folders": mixed, "trash": rows(c, "SELECT * FROM trash ORDER BY ts DESC", []) })
}

/// Keep `dir`, move the other copies in its duplicate group to the library's trash folder.
fn keep_copy(c: &Connection, dir: &str) -> Resp {
    if job::running() {
        return err(409, "wait for the running job to finish");
    }
    let dups = duplicates(c);
    let Some(group) = dups["groups"].as_array().into_iter().flatten().find(|g| g["dirs"].as_array().is_some_and(|d| d.iter().any(|x| x["dir"] == dir))) else {
        return err(404, "not part of a duplicate group");
    };
    let mut moved = 0;
    for other in group["dirs"].as_array().unwrap().iter().filter_map(|x| x["dir"].as_str()).filter(|d| *d != dir) {
        if let Err(e) = apply::trash(c, other) {
            return err(409, &format!("{e} ({moved} other copies already moved)"));
        }
        moved += 1;
    }
    job::navidrome(&Settings::load(c));
    ok(json!({ "ok": true, "moved": moved }))
}

/// Rank copies: lossless beats lossy, then higher bit depth, then bitrate.
fn quality(d: &Value) -> f64 {
    let lossless = d["bit_depth"].as_i64().unwrap_or(0) > 0;
    (if lossless { 1e6 } else { 0.0 }) + d["bit_depth"].as_f64().unwrap_or(0.0) * 1e4 + d["bitrate"].as_f64().unwrap_or(0.0)
}

fn proposal_detail(c: &Connection, id: &str) -> Resp {
    let Some(p) = rows(c, "SELECT * FROM proposals WHERE id=?1", [id]).into_iter().next() else { return err(404, "not found") };
    let changes: matcher::Changes = serde_json::from_str(p["changes"].as_str().unwrap_or("{}")).unwrap_or_default();
    let tracks: Vec<Value> = db::tracks_where(c, "dir=?1", [p["dir"].as_str().unwrap_or_default()])
        .into_iter()
        .map(|t| {
            let new = changes.get(&t.id).cloned().unwrap_or_default();
            let old: serde_json::Map<String, Value> = new.keys().map(|k| (k.clone(), json!(t.tags.get(k)))).collect();
            json!({ "id": t.id, "file": t.path.rsplit('/').next(), "title": t.tags.get("TITLE"), "duration": t.duration, "old": old, "new": new })
        })
        .collect();
    let mut p = p;
    p["candidates"] = serde_json::from_str(p["candidates"].as_str().unwrap_or("[]")).unwrap_or_default();
    p["changes"] = Value::Null;
    p["tracks"] = json!(tracks);
    ok(p)
}

/// Approve, reject or re-match many proposals: {action, ids: [..]}. One job for the lot.
fn bulk(c: &Connection, body: &Value) -> Resp {
    let ids: Vec<i64> = body["ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
    let dirs: Vec<String> = ids.iter().filter_map(|id| c.query_row("SELECT dir FROM proposals WHERE id=?1", [id], |r| r.get(0)).ok()).collect();
    if dirs.is_empty() {
        return err(400, "no proposals selected");
    }
    let set = |st: &str| {
        for id in &ids {
            c.execute("UPDATE proposals SET status=?1, updated=?2 WHERE id=?3", params![st, db::now(), id]).unwrap();
        }
    };
    match body["action"].as_str().unwrap_or_default() {
        "approve" => {
            set("approved");
            ok(json!({ "ok": true, "applying": job::start(job::Kind::Apply).is_ok() }))
        }
        "reject" => {
            set("rejected");
            ok(json!({"ok": true}))
        }
        "rematch" => started(job::start(job::Kind::Rematch(dirs))),
        _ => err(400, "unknown action"),
    }
}

fn proposal_action(c: &Connection, id: &str, action: &str, body: &Value) -> Resp {
    let set_status = |st: &str| c.execute("UPDATE proposals SET status=?1, updated=?2 WHERE id=?3", params![st, db::now(), id]).unwrap();
    match action {
        "approve" => {
            set_status("approved");
            // applies now, or with the next run if a job is busy
            let started = job::start(job::Kind::Apply).is_ok();
            ok(json!({ "ok": true, "applying": started }))
        }
        "reject" => {
            set_status("rejected");
            ok(json!({"ok": true}))
        }
        "edit" => {
            let (Some(track), Some(key)) = (body["track_id"].as_i64(), body["key"].as_str()) else { return err(400, "track_id and key required") };
            let raw: String = match c.query_row("SELECT changes FROM proposals WHERE id=?1", [id], |r| r.get(0)) {
                Ok(v) => v,
                Err(_) => return err(404, "not found"),
            };
            let mut ch: matcher::Changes = serde_json::from_str(&raw).unwrap_or_default();
            let entry = ch.entry(track).or_default();
            match body["value"].as_str() {
                Some(v) => entry.insert(key.trim().to_ascii_uppercase(), v.to_string()),
                None => entry.remove(key),
            };
            c.execute("UPDATE proposals SET changes=?1, updated=?2 WHERE id=?3", params![serde_json::to_string(&ch).unwrap(), db::now(), id]).unwrap();
            ok(json!({"ok": true}))
        }
        "pick" => {
            let Some(release) = release_id(body["release_id"].as_str().unwrap_or_default()) else {
                return err(400, "paste a MusicBrainz or Discogs release link, or an MBID");
            };
            let dir: String = match c.query_row("SELECT dir FROM proposals WHERE id=?1", [id], |r| r.get(0)) {
                Ok(d) => d,
                Err(_) => return err(404, "not found"),
            };
            let tracks = db::tracks_where(c, "dir=?1", [&dir]);
            match matcher::for_release(&tracks, &release, &Settings::load(c)) {
                Ok((conf, rel, ch)) => {
                    c.execute(
                        "UPDATE proposals SET status='review', confidence=?1, release_id=?2, release_title=?3, changes=?4, updated=?5 WHERE id=?6",
                        params![conf, rel.id, format!("{} – {}", rel.artist, rel.title), serde_json::to_string(&ch).unwrap(), db::now(), id],
                    )
                    .unwrap();
                    ok(json!({"ok": true}))
                }
                Err(e) => err(502, &e),
            }
        }
        _ => err(404, "unknown action"),
    }
}
