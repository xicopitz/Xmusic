//! Album = tracks in one directory. Find MusicBrainz candidates (tags, then AcoustID), score them,
//! align tracks, and turn the best one into per-track field changes.
//! Disc subfolders (CD1/, Disc 2/) belong to their parent's album, see `scan::album_dir`.

use crate::db::{CustomTag, Settings, Track};
use crate::remote;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use strsim::normalized_levenshtein;

pub type Changes = BTreeMap<i64, BTreeMap<String, String>>;

#[derive(Default, Clone)]
pub struct RelTrack {
    pub disc: usize,
    pub pos: usize,
    pub disc_total: usize,
    pub track_total: usize,
    pub title: String,
    pub length: f64,
    pub recording: String,
    pub id: String,
    pub artist: String,
    pub artist_id: String,
}

#[derive(Default, Clone)]
pub struct Release {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: String,
    pub date: String,
    pub country: String,
    pub formats: Vec<String>,
    pub rg_id: String,
    pub genre: Option<String>,
    pub label: String,
    pub catalog: String,
    /// first release date of the release group
    pub original_date: String,
    pub tracks: Vec<RelTrack>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Cand {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub date: String,
    pub country: String,
    pub tracks: usize,
    pub score: f64,
}

pub struct Proposal {
    pub status: &'static str,
    pub confidence: f64,
    pub release: Option<Release>,
    pub candidates: Vec<Cand>,
    pub changes: Changes,
    pub note: String,
}

fn credit(v: &Value) -> (String, String) {
    let mut name = String::new();
    for c in v.as_array().into_iter().flatten() {
        name += c["name"].as_str().unwrap_or_default();
        name += c["joinphrase"].as_str().unwrap_or_default();
    }
    (name, v[0]["artist"]["id"].as_str().unwrap_or_default().to_string())
}

pub fn parse_release(v: &Value) -> Release {
    let s = |x: &Value| x.as_str().unwrap_or_default().to_string();
    let (artist, artist_id) = credit(&v["artist-credit"]);
    let media = v["media"].as_array().cloned().unwrap_or_default();
    let mut tracks = vec![];
    for m in &media {
        let n = m["tracks"].as_array().map_or(0, |t| t.len());
        for t in m["tracks"].as_array().into_iter().flatten() {
            let (ta, taid) = credit(&t["artist-credit"]);
            tracks.push(RelTrack {
                disc: m["position"].as_u64().unwrap_or(1) as usize,
                pos: t["position"].as_u64().unwrap_or(0) as usize,
                disc_total: media.len(),
                track_total: n,
                title: s(&t["title"]),
                length: t["length"].as_f64().or(t["recording"]["length"].as_f64()).unwrap_or(0.0) / 1000.0,
                recording: s(&t["recording"]["id"]),
                id: s(&t["id"]),
                artist: if ta.is_empty() { artist.clone() } else { ta },
                artist_id: if taid.is_empty() { artist_id.clone() } else { taid },
            });
        }
    }
    // most voted genre; releases rarely have their own, so fall back to the release group's
    let top = |g: &Value| g.as_array().and_then(|g| g.iter().max_by_key(|x| x["count"].as_i64().unwrap_or(0))).map(|g| s(&g["name"]));
    let genre = top(&v["genres"]).or_else(|| top(&v["release-group"]["genres"]));
    let li = &v["label-info"][0];
    Release {
        id: s(&v["id"]),
        title: s(&v["title"]),
        artist,
        artist_id,
        date: s(&v["date"]),
        country: s(&v["country"]),
        formats: media.iter().map(|m| s(&m["format"])).collect(),
        rg_id: s(&v["release-group"]["id"]),
        genre,
        label: s(&li["label"]["name"]),
        catalog: Some(s(&li["catalog-number"])).filter(|c| c != "[none]").unwrap_or_default(),
        original_date: s(&v["release-group"]["first-release-date"]),
        tracks,
    }
}

fn sim(a: &str, b: &str) -> f64 {
    normalized_levenshtein(&a.trim().to_lowercase(), &b.trim().to_lowercase())
}

fn num(s: Option<&String>) -> Option<usize> {
    s?.split('/').next()?.trim().parse().ok()
}

/// DISCNUMBER tag, else the disc subfolder's number, else 1.
fn disc(t: &Track) -> usize {
    num(t.tags.get("DISCNUMBER"))
        .or_else(|| std::path::Path::new(&t.path).parent()?.file_name()?.to_str().and_then(crate::scan::disc_folder))
        .unwrap_or(1)
}

pub fn majority<'a>(tracks: &'a [Track], key: &str) -> &'a str {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for t in tracks {
        if let Some(v) = t.tags.get(key).filter(|v| !v.trim().is_empty()) {
            *counts.entry(v).or_default() += 1;
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map_or("", |(v, _)| v)
}

/// Score a release against local tracks. `fp[i]` = AcoustID recording ids for local track i (may be empty).
/// Returns (confidence 0..1, assignment local index → release track index).
pub fn score(local: &[Track], rel: &Release, fp: &[Vec<String>], s: &Settings) -> (f64, Vec<Option<usize>>) {
    let (nl, nr) = (local.len(), rel.tracks.len());
    let mut pairs = vec![];
    for (i, t) in local.iter().enumerate() {
        for (j, r) in rel.tracks.iter().enumerate() {
            let sc = if fp.get(i).is_some_and(|f| f.contains(&r.recording)) {
                1.0
            } else {
                let title = t.tags.get("TITLE").map_or(0.0, |x| sim(x, &r.title));
                let d = (t.duration - r.length).abs();
                let dur = if r.length == 0.0 { 0.5 } else if d <= 3.0 { 1.0 } else { (1.0 - (d - 3.0) / 10.0).max(0.0) };
                let pos_hit = num(t.tags.get("TRACKNUMBER")) == Some(r.pos) && disc(t) == r.disc;
                (0.6 * title + 0.4 * dur + if pos_hit { 0.2 } else { 0.0 }).min(1.0)
            };
            if sc > 0.3 {
                pairs.push((sc, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut assign = vec![None; nl];
    let mut used = vec![false; nr];
    let mut total = 0.0;
    for (sc, i, j) in pairs {
        if assign[i].is_none() && !used[j] {
            assign[i] = Some(j);
            used[j] = true;
            total += sc;
        }
    }
    let align = total / nl.max(nr).max(1) as f64;
    let count = 1.0 - (nl as f64 - nr as f64).abs() / nl.max(nr).max(1) as f64;
    let album = sim(majority(local, "ALBUM"), &rel.title);
    let artist = sim(
        Some(majority(local, "ALBUMARTIST")).filter(|a| !a.is_empty()).unwrap_or(majority(local, "ARTIST")),
        &rel.artist,
    );
    let mut conf = 0.25 * album + 0.15 * artist + 0.15 * count + 0.45 * align;
    if fp.iter().any(|f| !f.is_empty()) {
        conf = conf.max(0.2 * count + 0.8 * align);
    }
    if s.prefer_country.iter().any(|c| c.eq_ignore_ascii_case(&rel.country)) {
        conf += 0.02;
    }
    if rel.formats.iter().any(|f| s.prefer_media.iter().any(|p| p.eq_ignore_ascii_case(f))) {
        conf += 0.02;
    }
    (conf.min(1.0), assign)
}

pub fn scope_matches(c: &CustomTag, t: &Track) -> bool {
    let sc = c.scope.trim();
    if sc.is_empty() || sc.eq_ignore_ascii_case("all") {
        return true;
    }
    if let Some(prefix) = sc.strip_prefix("path:") {
        return t.path.starts_with(prefix.trim());
    }
    match sc.split_once('=') {
        Some((k, v)) => t.tags.get(&k.trim().to_ascii_uppercase()).is_some_and(|x| x.eq_ignore_ascii_case(v.trim())),
        None => false,
    }
}

/// Custom tags + rewrite rules, applied on top of `new` (falls back to the track's current value).
pub fn apply_rules(t: &Track, new: &mut BTreeMap<String, String>, s: &Settings, rules: &[(String, Regex, String)]) {
    for c in s.custom_tags.iter().filter(|c| !c.key.trim().is_empty() && scope_matches(c, t)) {
        new.insert(c.key.trim().to_ascii_uppercase(), c.value.clone());
    }
    for (field, re, rep) in rules {
        if let Some(cur) = new.get(field).or(t.tags.get(field)) {
            let out = re.replace_all(cur, rep.as_str()).into_owned();
            if &out != cur {
                new.insert(field.clone(), out);
            }
        }
    }
    new.retain(|k, v| t.tags.get(k) != Some(v));
}

pub fn compile_rules(s: &Settings) -> Vec<(String, Regex, String)> {
    s.rewrites
        .iter()
        .filter_map(|r| Some((r.field.trim().to_ascii_uppercase(), Regex::new(&r.pattern).ok()?, r.replace.clone())))
        .collect()
}

pub fn changes(local: &[Track], rel: &Release, assign: &[Option<usize>], s: &Settings) -> Changes {
    let rules = compile_rules(s);
    let mut out = Changes::new();
    for (i, t) in local.iter().enumerate() {
        let mut new = BTreeMap::new();
        if let Some(r) = assign[i].map(|j| &rel.tracks[j]) {
            let mut put = |k: &str, v: String| {
                if !v.is_empty() && s.writable(k) {
                    new.insert(k.to_string(), v);
                }
            };
            put("TITLE", r.title.clone());
            put("ARTIST", r.artist.clone());
            put("ALBUM", rel.title.clone());
            put("ALBUMARTIST", rel.artist.clone());
            put("DATE", rel.date.clone());
            put("ORIGINALDATE", rel.original_date.clone());
            put("LABEL", rel.label.clone());
            put("CATALOGNUMBER", rel.catalog.clone());
            put("TRACKNUMBER", r.pos.to_string());
            put("TRACKTOTAL", r.track_total.to_string());
            put("DISCNUMBER", r.disc.to_string());
            put("DISCTOTAL", r.disc_total.to_string());
            put("GENRE", rel.genre.clone().unwrap_or_default());
            put("MUSICBRAINZ_TRACKID", r.recording.clone());
            put("MUSICBRAINZ_RELEASETRACKID", r.id.clone());
            put("MUSICBRAINZ_ALBUMID", rel.id.clone());
            put("MUSICBRAINZ_ARTISTID", r.artist_id.clone());
            put("MUSICBRAINZ_ALBUMARTISTID", rel.artist_id.clone());
            put("MUSICBRAINZ_RELEASEGROUPID", rel.rg_id.clone());
        }
        apply_rules(t, &mut new, s, &rules);
        if !new.is_empty() {
            out.insert(t.id, new);
        }
    }
    out
}

fn cand(r: &Release, score: f64) -> Cand {
    Cand {
        id: r.id.clone(),
        title: r.title.clone(),
        artist: r.artist.clone(),
        date: r.date.clone(),
        country: r.country.clone(),
        tracks: r.tracks.len(),
        score,
    }
}

/// Full match for one album directory. Network-bound (~1 req/s).
pub fn match_album(local: &[Track], s: &Settings) -> Proposal {
    let ua = &s.contact_email;
    let mut rels: Vec<Release> = vec![];
    let mut notes = vec![];
    let album = majority(local, "ALBUM");
    let artist = Some(majority(local, "ALBUMARTIST")).filter(|a| !a.is_empty()).unwrap_or(majority(local, "ARTIST"));
    if !album.is_empty() {
        match remote::search_releases(ua, artist, album, local.len()) {
            Ok(ids) => {
                // each lookup costs ~1.1 s of MusicBrainz quota: stop once one is clearly right
                let good = if s.auto_apply_above > 0.0 { s.auto_apply_above } else { 0.95 };
                for id in ids.iter().take(3) {
                    match remote::release(ua, id) {
                        Ok(v) => {
                            let r = parse_release(&v);
                            let (sc, assign) = score(local, &r, &[], s);
                            rels.push(r);
                            if sc >= good && assign.iter().all(Option::is_some) {
                                break;
                            }
                        }
                        Err(e) => notes.push(e),
                    }
                }
            }
            Err(e) => notes.push(e),
        }
    }
    // already tagged (a re-match): score the release it claims to be, so wrong titles show up as changes
    let own = majority(local, "MUSICBRAINZ_ALBUMID");
    if !own.is_empty() && !rels.iter().any(|r| r.id == own) {
        match remote::release(ua, own) {
            Ok(v) => rels.push(parse_release(&v)),
            Err(e) => notes.push(e),
        }
    }
    let none: Vec<Vec<String>> = vec![];
    let best = |rels: &[Release], fp: &[Vec<String>]| rels.iter().map(|r| score(local, r, fp, s).0).fold(0.0, f64::max);

    let mut fp = none.clone();
    if best(&rels, &fp) < s.match_threshold && !s.acoustid_key.is_empty() {
        let mut votes: HashMap<String, usize> = HashMap::new();
        // 3 tracks' votes find the release; fpcalc decodes audio, so each one costs CPU and a read over the mount
        for t in local.iter().take(3) {
            let mut recs = vec![];
            if let Some((dur, print)) = remote::fpcalc(&t.path) {
                match remote::acoustid(ua, &s.acoustid_key, dur, &print) {
                    Ok(hits) => {
                        for (_, rec, releases) in hits.into_iter().filter(|h| h.0 > 0.5) {
                            recs.push(rec);
                            for r in releases {
                                *votes.entry(r).or_default() += 1;
                            }
                        }
                    }
                    Err(e) => notes.push(e),
                }
            } else {
                notes.push("fpcalc unavailable; skipped fingerprinting".into());
                break;
            }
            fp.push(recs);
        }
        let mut top: Vec<_> = votes.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        let fresh: Vec<String> = top.into_iter().map(|(id, _)| id).filter(|id| !rels.iter().any(|r| &r.id == id)).take(3).collect();
        for id in fresh {
            match remote::release(ua, &id) {
                Ok(v) => rels.push(parse_release(&v)),
                Err(e) => notes.push(e),
            }
        }
    }

    let mut scored: Vec<(f64, Vec<Option<usize>>, Release)> = rels
        .into_iter()
        .map(|r| {
            let (sc, a) = score(local, &r, &fp, s);
            (sc, a, r)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    notes.dedup();
    let candidates = scored.iter().map(|(sc, _, r)| cand(r, *sc)).collect();
    let Some((conf, assign, rel)) = scored.into_iter().next() else {
        notes.insert(0, "no candidates found".into());
        return Proposal { status: "manual", confidence: 0.0, release: None, candidates, changes: Changes::new(), note: notes.join("; ") };
    };
    let unmatched = assign.iter().filter(|a| a.is_none()).count();
    if unmatched > 0 {
        notes.push(format!("{unmatched} local track(s) not aligned"));
    }
    let status = if s.auto_apply_above > 0.0 && conf >= s.auto_apply_above && unmatched == 0 {
        "approved"
    } else if conf >= s.match_threshold {
        "review"
    } else {
        "manual"
    };
    Proposal { status, confidence: conf, changes: changes(local, &rel, &assign, s), release: Some(rel), candidates, note: notes.join("; ") }
}

/// User picked (or pasted) a release: rebuild the proposal against it.
pub fn for_release(local: &[Track], mbid: &str, s: &Settings) -> Result<(f64, Release, Changes), String> {
    let rel = parse_release(&remote::release(&s.contact_email, mbid)?);
    let (sc, assign) = score(local, &rel, &[], s);
    let ch = changes(local, &rel, &assign, s);
    Ok((sc, rel, ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, title: &str, n: usize, dur: f64) -> Track {
        let mut tags = crate::tags::Fields::new();
        tags.insert("TITLE".into(), title.into());
        tags.insert("ALBUM".into(), "Kind of Blue".into());
        tags.insert("ARTIST".into(), "Miles Davis".into());
        tags.insert("TRACKNUMBER".into(), n.to_string());
        Track { id, path: format!("/m/{n}.flac"), dir: "/m".into(), duration: dur, has_cover: false, cover_px: 0, tags }
    }

    fn release() -> Release {
        let t = |pos, title: &str, length| RelTrack {
            disc: 1, pos, disc_total: 1, track_total: 2, title: title.into(), length,
            recording: format!("rec{pos}"), id: format!("t{pos}"), artist: "Miles Davis".into(), artist_id: "a1".into(),
        };
        Release {
            id: "rel1".into(), title: "Kind of Blue".into(), artist: "Miles Davis".into(), artist_id: "a1".into(),
            date: "1959-08-17".into(), country: "US".into(), formats: vec!["CD".into()], rg_id: "rg1".into(),
            genre: Some("jazz".into()), label: "Columbia".into(), catalog: "CL 1355".into(), original_date: "1959".into(),
            tracks: vec![t(1, "So What", 562.0), t(2, "Freddie Freeloader", 586.0)],
        }
    }

    #[test]
    fn scores_and_aligns() {
        let s = Settings::default();
        // local order swapped, slight title typo, durations within tolerance
        let local = vec![track(10, "Freddie Freloader", 2, 587.0), track(11, "So What", 1, 561.0)];
        let (conf, assign) = score(&local, &release(), &[], &s);
        assert!(conf > 0.9, "conf {conf}");
        assert_eq!(assign, vec![Some(1), Some(0)]);

        let ch = changes(&local, &release(), &assign, &s);
        assert_eq!(ch[&10]["TITLE"], "Freddie Freeloader");
        assert!(!ch[&11].contains_key("TITLE"), "unchanged values are dropped");
        assert_eq!(ch[&11]["MUSICBRAINZ_ALBUMID"], "rel1");
        assert_eq!(ch[&11]["LABEL"], "Columbia");
        assert_eq!(ch[&11]["ORIGINALDATE"], "1959");
    }

    #[test]
    fn parses_label_and_group_genre() {
        let v = serde_json::json!({
            "id": "r", "genres": [], "media": [],
            "release-group": { "first-release-date": "1993", "genres": [{"name": "rock", "count": 1}, {"name": "glam metal", "count": 3}] },
            "label-info": [{ "catalog-number": "[none]", "label": { "name": "Elektra" } }],
        });
        let r = parse_release(&v);
        assert_eq!(r.genre.as_deref(), Some("glam metal"));
        assert_eq!((r.label.as_str(), r.catalog.as_str(), r.original_date.as_str()), ("Elektra", "", "1993"));
    }

    #[test]
    fn fingerprint_beats_missing_tags() {
        let s = Settings::default();
        let mut local = vec![track(1, "", 0, 562.0), track(2, "", 0, 586.0)];
        for t in &mut local {
            t.tags.clear();
        }
        let fp = vec![vec!["rec1".to_string()], vec!["rec2".to_string()]];
        assert!(score(&local, &release(), &[], &s).0 < s.match_threshold);
        assert!(score(&local, &release(), &fp, &s).0 > 0.95);
    }

    #[test]
    fn custom_tags_and_rewrites() {
        let mut s = Settings::default();
        s.custom_tags.push(CustomTag { key: "source".into(), value: "vinyl".into(), scope: "artist=miles davis".into() });
        s.custom_tags.push(CustomTag { key: "X".into(), value: "y".into(), scope: "path:/other".into() });
        s.rewrites.push(crate::db::Rewrite { field: "title".into(), pattern: "(?i) feat\\. ".into(), replace: " ft. ".into() });
        let mut t = track(1, "Song Feat. Someone", 1, 1.0);
        t.tags.insert("ARTIST".into(), "Miles Davis".into());
        let mut new = BTreeMap::new();
        apply_rules(&t, &mut new, &s, &compile_rules(&s));
        assert_eq!(new["SOURCE"], "vinyl");
        assert_eq!(new["TITLE"], "Song ft. Someone");
        assert!(!new.contains_key("X"));
    }
}
