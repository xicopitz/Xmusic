//! HTTP clients: MusicBrainz, AcoustID, Cover Art Archive, LRCLIB. Responses stay serde_json::Value.

use serde_json::Value;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

static AGENT: LazyLock<Mutex<(String, ureq::Agent)>> = LazyLock::new(|| Mutex::new((String::new(), agent(""))));
// ponytail: one global MusicBrainz slot; MB allows 1 req/s per client anyway.
static MB_SLOT: Mutex<Option<Instant>> = Mutex::new(None);
// LRCLIB is a small free service that answers 503 when parallel albums hammer it
static LRCLIB_SLOT: Mutex<Option<Instant>> = Mutex::new(None);
static LRCLIB_PAUSED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Waits until `gap` has passed since the slot's last request, then claims it.
fn pace(slot: &Mutex<Option<Instant>>, gap: Duration) {
    let mut last = slot.lock().unwrap();
    if let Some(t) = *last {
        std::thread::sleep(gap.saturating_sub(t.elapsed()));
    }
    *last = Some(Instant::now());
}

/// LRCLIB kept failing after retries: skip lyrics until the pause ends (the tracks are retried next run).
pub fn lyrics_paused() -> bool {
    LRCLIB_PAUSED_UNTIL.lock().unwrap().is_some_and(|t| Instant::now() < t)
}

fn agent(contact: &str) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(format!("xmusic/{} ( {} )", env!("CARGO_PKG_VERSION"), if contact.is_empty() { "https://github.com/" } else { contact }))
        .build()
        .into()
}

/// MusicBrainz requires a contact in the User-Agent; rebuilt when the setting changes.
pub fn http(contact: &str) -> ureq::Agent {
    let mut a = AGENT.lock().unwrap();
    if a.0 != contact {
        *a = (contact.to_string(), agent(contact));
    }
    a.1.clone()
}

pub type Res<T> = Result<T, String>;

/// Pass-through that records the call for /metrics.
fn counted<T>(service: &'static str, r: Result<T, ureq::Error>) -> Result<T, ureq::Error> {
    crate::metrics::remote(service, &r);
    r
}

fn mb_get(contact: &str, path: &str, query: &[(&str, &str)]) -> Res<Value> {
    for attempt in 0..3 {
        pace(&MB_SLOT, Duration::from_millis(1100));
        let mut req = http(contact).get(format!("https://musicbrainz.org/ws/2/{path}")).query("fmt", "json");
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        match counted("musicbrainz", req.call()) {
            Ok(mut r) => return r.body_mut().read_json().map_err(|e| e.to_string()),
            Err(ureq::Error::StatusCode(503)) if attempt < 2 => std::thread::sleep(Duration::from_secs(2)),
            Err(e) => return Err(format!("musicbrainz {path}: {e}")),
        }
    }
    unreachable!()
}

fn lucene(s: &str) -> String {
    s.chars()
        .map(|c| if "+-&|!(){}[]^\"~*?:\\/".contains(c) { ' ' } else { c })
        .collect()
}

/// Release search hits (id + score) for album/artist/track count.
pub fn search_releases(contact: &str, artist: &str, album: &str, tracks: usize) -> Res<Vec<String>> {
    let mut q = format!("release:\"{}\"", lucene(album));
    if !artist.is_empty() {
        q += &format!(" AND artist:\"{}\"", lucene(artist));
    }
    q += &format!(" AND tracks:{tracks}");
    let v = mb_get(contact, "release", &[("query", &q), ("limit", "5")])?;
    Ok(v["releases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(String::from))
        .collect())
}

pub fn release(contact: &str, mbid: &str) -> Res<Value> {
    mb_get(contact, &format!("release/{mbid}"), &[("inc", "recordings artist-credits release-groups genres")])
}

/// Runs chromaprint's fpcalc → (duration secs, fingerprint). None if fpcalc is missing or fails.
pub fn fpcalc(path: &str) -> Option<(u64, String)> {
    // lowest CPU priority: decoding audio is the one CPU-heavy thing a run does
    let out = std::process::Command::new("nice").args(["-n", "19", "fpcalc", "-json"]).arg(path).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((v["duration"].as_f64()? as u64, v["fingerprint"].as_str()?.to_string()))
}

/// AcoustID lookup → [(score, recording id, release ids)].
pub fn acoustid(contact: &str, key: &str, dur: u64, fp: &str) -> Res<Vec<(f64, String, Vec<String>)>> {
    std::thread::sleep(Duration::from_millis(340)); // 3 req/s limit
    let dur = dur.to_string();
    let sent = http(contact)
        .post("https://api.acoustid.org/v2/lookup")
        .send_form([("client", key), ("duration", &dur), ("fingerprint", fp), ("meta", "recordings releaseids")]);
    let v: Value = counted("acoustid", sent)
        .map_err(|e| format!("acoustid: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| e.to_string())?;
    let mut out = vec![];
    for r in v["results"].as_array().into_iter().flatten() {
        let score = r["score"].as_f64().unwrap_or(0.0);
        for rec in r["recordings"].as_array().into_iter().flatten() {
            let rels = rec["releases"].as_array().into_iter().flatten().filter_map(|x| x["id"].as_str().map(String::from)).collect();
            if let Some(id) = rec["id"].as_str() {
                out.push((score, id.to_string(), rels));
            }
        }
    }
    Ok(out)
}

/// Front cover from the Cover Art Archive. None when the release has no art.
pub fn cover(contact: &str, release: &str, size: &str) -> Res<Option<Vec<u8>>> {
    let suffix = if size == "original" { String::new() } else { format!("-{size}") };
    match counted("coverart", http(contact).get(format!("https://coverartarchive.org/release/{release}/front{suffix}")).call()) {
        Ok(mut r) => r.body_mut().with_config().limit(50 << 20).read_to_vec().map(Some).map_err(|e| e.to_string()),
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(e) => Err(format!("coverart: {e}")),
    }
}

/// LRCLIB → (synced, plain). Exact match first, then search filtered to ±2s duration.
pub fn lyrics(contact: &str, artist: &str, title: &str, album: &str, dur: f64) -> Res<Option<(Option<String>, Option<String>)>> {
    let pick = |v: &Value| {
        let s = v["syncedLyrics"].as_str().filter(|s| !s.is_empty()).map(String::from);
        let p = v["plainLyrics"].as_str().filter(|s| !s.is_empty()).map(String::from);
        (s.is_some() || p.is_some()).then_some((s, p))
    };
    let d = (dur.round() as i64).to_string();
    let a = http(contact);
    // LRCLIB answers 429/503 when busy: back off and retry rather than recording a miss; still failing
    // = pause lyrics for 10 min so every remaining track doesn't wait out the retries too
    let call = |q: &[(&str, &str)], path: &str| {
        let mut wait = 2;
        loop {
            pace(&LRCLIB_SLOT, Duration::from_millis(250));
            let mut req = a.get(format!("https://lrclib.net/api/{path}"));
            for (k, v) in q {
                req = req.query(*k, *v);
            }
            match counted("lrclib", req.call()) {
                Err(ureq::Error::StatusCode(429 | 502 | 503 | 504)) if wait <= 8 => {
                    std::thread::sleep(Duration::from_secs(wait));
                    wait *= 2;
                }
                Err(ureq::Error::StatusCode(code @ (429 | 502 | 503 | 504))) => {
                    *LRCLIB_PAUSED_UNTIL.lock().unwrap() = Some(Instant::now() + Duration::from_secs(600));
                    eprintln!("lrclib: still answering {code} after retries; pausing lyrics for 10 minutes");
                    return Err(ureq::Error::StatusCode(code));
                }
                r => return r,
            }
        }
    };
    let got = call(&[("artist_name", artist), ("track_name", title), ("album_name", album), ("duration", &d)], "get");
    match got {
        Ok(mut r) => {
            let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
            if let Some(x) = pick(&v) {
                return Ok(Some(x));
            }
        }
        Err(ureq::Error::StatusCode(404)) => {}
        Err(e) => return Err(format!("lrclib: {e}")),
    }
    let v: Value = call(&[("artist_name", artist), ("track_name", title)], "search")
        .map_err(|e| format!("lrclib: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| e.to_string())?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|x| (x["duration"].as_f64().unwrap_or(-99.0) - dur).abs() <= 2.0)
        .find_map(pick))
}

/// `link`: tapping the notification opens it (Android app), and it's appended as a markdown link.
pub fn gotify(contact: &str, url: &str, token: &str, priority: i64, title: &str, message: &str, link: Option<&str>) -> Res<()> {
    let mut body = serde_json::json!({"title": title, "message": message, "priority": priority});
    if let Some(l) = link {
        body["message"] = format!("{message}\n\n[Open xmusic]({l})").into();
        body["extras"] = serde_json::json!({
            "client::display": {"contentType": "text/markdown"},
            "client::notification": {"click": {"url": l}},
        });
    }
    let sent = http(contact).post(format!("{}/message", url.trim_end_matches('/'))).header("X-Gotify-Key", token).send_json(body);
    counted("gotify", sent)
        .map(|_| ())
        .map_err(|e| format!("gotify: {e}"))
}

/// Ask a Navidrome/Subsonic server to rescan its library.
// ponytail: Subsonic `p=enc:` sends the password hex-encoded (not hashed); use HTTPS if Navidrome isn't on the LAN.
pub fn navidrome_scan(contact: &str, url: &str, user: &str, password: &str) -> Res<()> {
    let r = http(contact)
        .get(format!("{}/rest/startScan", url.trim_end_matches('/')))
        .query("u", user)
        .query("p", format!("enc:{}", hex::encode(password)))
        .query("v", "1.16.1")
        .query("c", "xmusic")
        .query("f", "json")
        .call();
    let v: Value = counted("navidrome", r).map_err(|e| format!("navidrome: {e}"))?.body_mut().read_json().map_err(|e| format!("navidrome: {e}"))?;
    let resp = &v["subsonic-response"];
    if resp["status"] == "ok" { Ok(()) } else { Err(format!("navidrome: {}", resp["error"]["message"].as_str().unwrap_or("unexpected response"))) }
}

#[test]
fn pacing() {
    let slot = Mutex::new(None);
    let t = Instant::now();
    (0..3).for_each(|_| pace(&slot, Duration::from_millis(50)));
    assert!(t.elapsed() >= Duration::from_millis(100)); // first call is free, the next two wait
}
