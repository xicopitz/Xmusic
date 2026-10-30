//! Library health report: tag problems found from the tracks table alone (no file reads, no network).

use crate::tags::Fields;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Shown per list; the rest is counted.
const LIMIT: usize = 500;

/// Lowercase, accents folded, only letters and digits, no leading "the": "The Mötley Crüe" == "motley crue".
pub fn fold(name: &str) -> String {
    let lower = name.to_lowercase().replace('&', "and");
    let s: String = lower
        .chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            c => c,
        })
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.strip_prefix("the ").unwrap_or(&s).replace(' ', "")
}

fn num(v: Option<&String>) -> Option<u32> {
    v?.split('/').next()?.trim().parse().ok()
}

/// Missing positions per disc, e.g. "2, 5" or "disc 2: 3". `nums` = (disc, track), `total` = TRACKTOTAL per disc.
pub fn gaps(nums: &[(u32, u32)], total: &BTreeMap<u32, u32>) -> String {
    let discs: BTreeSet<u32> = nums.iter().map(|n| n.0).collect();
    let mut out = vec![];
    for d in &discs {
        let have: BTreeSet<u32> = nums.iter().filter(|n| n.0 == *d).map(|n| n.1).collect();
        let last = have.last().copied().unwrap_or(0).max(total.get(d).copied().unwrap_or(0));
        let missing: Vec<String> = (1..=last).filter(|n| !have.contains(n)).map(|n| n.to_string()).collect();
        if !missing.is_empty() {
            let list = missing.join(", ");
            out.push(if discs.len() > 1 { format!("disc {d}: {list}") } else { list });
        }
    }
    out.join("; ")
}

/// Tag fixes for an album whose discs sit in subfolders (`CD1/`, `Disc 2/`): DISCNUMBER from the folder,
/// DISCTOTAL and TRACKTOTAL where they're missing or plainly wrong. Totals are never lowered below what a
/// release claims unless they can't be right (a TRACKTOTAL below the highest track number, or the whole
/// album's count on one disc), since a smaller count may just mean files are missing.
/// `tracks` = (id, path, tags). Returns the changes per track id.
pub fn disc_changes<'a>(dir: &str, tracks: impl IntoIterator<Item = (i64, &'a str, &'a Fields)>) -> BTreeMap<i64, Fields> {
    let dir = Path::new(dir);
    // (id, tags, disc from its folder)
    let on_disc: Vec<(i64, &Fields, u32)> = tracks
        .into_iter()
        .filter_map(|(id, path, tags)| {
            let parent = Path::new(path).parent()?;
            (parent.parent() == Some(dir)).then_some(())?;
            Some((id, tags, crate::scan::disc_folder(parent.file_name()?.to_str()?)? as u32))
        })
        .collect();
    let discs: BTreeSet<u32> = on_disc.iter().map(|t| t.2).collect();
    if discs.len() < 2 {
        return BTreeMap::new(); // one disc folder isn't a multi-disc album to repair
    }
    let disc_total = discs.iter().max().copied().unwrap_or(1).max(discs.len() as u32);
    let mut out = BTreeMap::new();
    for &(id, tags, disc) in &on_disc {
        let mut set = Fields::new();
        if num(tags.get("DISCNUMBER")) != Some(disc) {
            set.insert("DISCNUMBER".into(), disc.to_string());
        }
        if num(tags.get("DISCTOTAL")).is_none_or(|t| t < disc_total) {
            set.insert("DISCTOTAL".into(), disc_total.to_string());
        }
        let here: Vec<&&Fields> = on_disc.iter().filter(|t| t.2 == disc).map(|t| &t.1).collect();
        let highest = here.iter().filter_map(|t| num(t.get("TRACKNUMBER"))).max().unwrap_or(0);
        let want = highest.max(here.len() as u32);
        let wrong = match num(tags.get("TRACKTOTAL")) {
            None => true,
            Some(t) => t < highest || (t as usize == on_disc.len() && t > want),
        };
        if wrong && tags.get("TRACKTOTAL") != Some(&want.to_string()) {
            set.insert("TRACKTOTAL".into(), want.to_string());
        }
        if !set.is_empty() {
            out.insert(id, set);
        }
    }
    out
}

fn lossy(format: &str, bit_depth: i64) -> bool {
    matches!(format, "MP3" | "AAC" | "OGG" | "Opus") || (format == "M4A" && bit_depth == 0) // ALAC has a bit depth
}

struct Row {
    id: i64,
    path: String,
    format: String,
    bitrate: i64,
    bit_depth: i64,
    tags: Fields,
}

pub fn report(c: &Connection) -> Value {
    let mut albums: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    let mut stmt = c.prepare("SELECT dir,path,format,bitrate,bit_depth,tags,id FROM tracks").unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Row {
                    id: r.get(6)?,
                    path: r.get(1)?,
                    format: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    bitrate: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    bit_depth: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    tags: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                },
            ))
        })
        .unwrap();
    for r in rows.map(Result::unwrap) {
        albums.entry(r.0).or_default().push(r.1);
    }

    let (mut numbers, mut gap_list, mut artists, mut low, mut disc_list) = (vec![], vec![], vec![], vec![], vec![]);
    // folded name → spelling → tracks
    let mut spellings: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for (dir, tracks) in &albums {
        let no_num = tracks.iter().filter(|t| num(t.tags.get("TRACKNUMBER")).is_none()).count();
        if no_num > 0 {
            numbers.push(json!({ "dir": dir, "tracks": tracks.len(), "missing": no_num }));
        }
        let nums: Vec<(u32, u32)> = tracks.iter().filter_map(|t| Some((num(t.tags.get("DISCNUMBER")).unwrap_or(1), num(t.tags.get("TRACKNUMBER"))?))).collect();
        let totals: BTreeMap<u32, u32> =
            tracks.iter().filter_map(|t| Some((num(t.tags.get("DISCNUMBER")).unwrap_or(1), num(t.tags.get("TRACKTOTAL"))?))).collect();
        let g = gaps(&nums, &totals);
        if !g.is_empty() {
            gap_list.push(json!({ "dir": dir, "missing": g }));
        }
        let fixes = disc_changes(dir, tracks.iter().map(|t| (t.id, t.path.as_str(), &t.tags)));
        if !fixes.is_empty() {
            let mut fields: BTreeMap<&str, usize> = BTreeMap::new();
            for k in fixes.values().flat_map(|f| f.keys()) {
                *fields.entry(k).or_default() += 1;
            }
            disc_list.push(json!({ "dir": dir, "tracks": fixes.len(), "fields": fields }));
        }
        let values: BTreeSet<&str> = tracks.iter().filter_map(|t| t.tags.get("ALBUMARTIST").map(String::as_str)).collect();
        let without = tracks.iter().filter(|t| !t.tags.contains_key("ALBUMARTIST")).count();
        if values.len() > 1 || (!values.is_empty() && without > 0) {
            artists.push(json!({ "dir": dir, "values": values, "without": without }));
        }
        if tracks.iter().any(|t| t.format == "FLAC") {
            let files: Vec<Value> = tracks
                .iter()
                .filter(|t| lossy(&t.format, t.bit_depth))
                .map(|t| json!({ "path": t.path, "format": t.format, "bitrate": t.bitrate }))
                .collect();
            if !files.is_empty() {
                low.push(json!({ "dir": dir, "files": files }));
            }
        }
        for t in tracks {
            for k in ["ARTIST", "ALBUMARTIST"] {
                if let Some(v) = t.tags.get(k) {
                    *spellings.entry(fold(v)).or_default().entry(v.clone()).or_default() += 1;
                }
            }
        }
    }
    let spell: Vec<Value> = spellings
        .into_values()
        .filter(|m| m.len() > 1)
        .map(|m| json!(m.into_iter().map(|(name, tracks)| json!({ "name": name, "tracks": tracks })).collect::<Vec<_>>()))
        .collect();
    let cap = |v: Vec<Value>| json!({ "total": v.len(), "items": v.into_iter().take(LIMIT).collect::<Vec<_>>() });
    json!({
        "albums": albums.len(),
        "missing_numbers": cap(numbers),
        "gaps": cap(gap_list),
        "disc_numbers": cap(disc_list),
        "album_artist": cap(artists),
        "spellings": cap(spell),
        "lossy_in_flac": cap(low),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_spellings() {
        assert_eq!(fold("Mötley Crüe"), fold("Motley Crue"));
        assert_eq!(fold("The Beatles"), fold("Beatles"));
        assert_eq!(fold("Simon & Garfunkel"), fold("Simon and Garfunkel"));
        assert_eq!(fold("AC/DC"), fold("ACDC"));
        assert_ne!(fold("Queen"), fold("Queens"));
    }

    #[test]
    fn repairs_disc_folders() {
        let f = |pairs: &[(&str, &str)]| -> Fields { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        // disc 2's files claim disc 1 and the album's 4 tracks as their total
        let a = f(&[("DISCNUMBER", "1"), ("TRACKNUMBER", "1"), ("TRACKTOTAL", "2"), ("DISCTOTAL", "2")]);
        let b = f(&[("DISCNUMBER", "1"), ("TRACKNUMBER", "2"), ("TRACKTOTAL", "2"), ("DISCTOTAL", "2")]);
        let c = f(&[("DISCNUMBER", "1"), ("TRACKNUMBER", "1"), ("TRACKTOTAL", "4")]);
        let d = f(&[("TRACKNUMBER", "2"), ("TRACKTOTAL", "4")]);
        let tracks = [(1, "/m/X/CD1/01.flac", &a), (2, "/m/X/CD1/02.flac", &b), (3, "/m/X/CD2/01.flac", &c), (4, "/m/X/CD2/02.flac", &d)];
        let out = disc_changes("/m/X", tracks);
        assert!(!out.contains_key(&1) && !out.contains_key(&2), "disc 1 is right");
        assert_eq!(out[&3], f(&[("DISCNUMBER", "2"), ("DISCTOTAL", "2"), ("TRACKTOTAL", "2")]));
        assert_eq!(out[&4]["DISCNUMBER"], "2");

        // a release total above the files present is kept: the missing track is missing, not wrong
        let e = f(&[("DISCNUMBER", "2"), ("DISCTOTAL", "2"), ("TRACKNUMBER", "1"), ("TRACKTOTAL", "3")]);
        let g = f(&[("DISCNUMBER", "2"), ("DISCTOTAL", "2"), ("TRACKNUMBER", "3"), ("TRACKTOTAL", "3")]);
        let tracks = [(1, "/m/X/CD1/01.flac", &a), (2, "/m/X/CD1/02.flac", &b), (3, "/m/X/Disc 2/01.flac", &e), (4, "/m/X/Disc 2/03.flac", &g)];
        assert!(disc_changes("/m/X", tracks).is_empty());
        // a single disc folder is left alone
        assert!(disc_changes("/m/X", [(1, "/m/X/CD1/01.flac", &d)]).is_empty());
    }

    #[test]
    fn finds_gaps() {
        assert_eq!(gaps(&[(1, 1), (1, 2), (1, 4)], &BTreeMap::new()), "3");
        assert_eq!(gaps(&[(1, 1), (1, 2)], &BTreeMap::from([(1, 4)])), "3, 4", "TRACKTOTAL counts");
        assert_eq!(gaps(&[(1, 1), (2, 2)], &BTreeMap::new()), "disc 2: 1");
        assert_eq!(gaps(&[(1, 1), (1, 2)], &BTreeMap::new()), "");
    }
}
