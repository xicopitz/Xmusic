//! Library health report: tag problems found from the tracks table alone (no file reads, no network).

use crate::tags::Fields;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

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

fn lossy(format: &str, bit_depth: i64) -> bool {
    matches!(format, "MP3" | "AAC" | "OGG" | "Opus") || (format == "M4A" && bit_depth == 0) // ALAC has a bit depth
}

struct Row {
    path: String,
    format: String,
    bitrate: i64,
    bit_depth: i64,
    tags: Fields,
}

pub fn report(c: &Connection) -> Value {
    let mut albums: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    let mut stmt = c.prepare("SELECT dir,path,format,bitrate,bit_depth,tags FROM tracks").unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Row {
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

    let (mut numbers, mut gap_list, mut artists, mut low) = (vec![], vec![], vec![], vec![]);
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
    fn finds_gaps() {
        assert_eq!(gaps(&[(1, 1), (1, 2), (1, 4)], &BTreeMap::new()), "3");
        assert_eq!(gaps(&[(1, 1), (1, 2)], &BTreeMap::from([(1, 4)])), "3, 4", "TRACKTOTAL counts");
        assert_eq!(gaps(&[(1, 1), (2, 2)], &BTreeMap::new()), "disc 2: 1");
        assert_eq!(gaps(&[(1, 1), (1, 2)], &BTreeMap::new()), "");
    }
}
