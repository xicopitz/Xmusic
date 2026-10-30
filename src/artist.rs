//! Artist images: `artist.jpg` next to each artist's albums, which Navidrome shows on the artist page.
//! Source: the artist's Wikidata image on Wikimedia Commons, found through its MusicBrainz id. A separate
//! small file, like cover.jpg: music files are never touched.
// ponytail: Wikidata only; artists without a Commons photo get none. Deezer or fanart.tv would cover more,
// with a key or looser terms.

use crate::db::{self, Settings};
use crate::{job, remote};
use rusqlite::params;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// MusicBrainz's "Various Artists": not a person, no picture.
const VARIOUS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";
/// Lookups that found nothing are asked again after this long.
const RETRY_DAYS: i64 = 30;

pub struct Done {
    pub saved: usize,
    pub missing: usize,
}

impl std::fmt::Display for Done {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} artist image(s) saved, {} artist(s) without one", self.saved, self.missing)
    }
}

fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    a.components().zip(b.components()).take_while(|(x, y)| x == y).map(|(x, _)| x).collect()
}

/// Folders that should hold each artist's image, from (album dir, album artist id, library root) rows.
/// That's the folder holding all of the artist's albums, when it is inside the library and holds no
/// other artist's albums (`Music/Artist/Album`). Otherwise, as in a flat `Music/Artist - Album` library,
/// each album folder gets one: Navidrome reads `artist.*` there too.
pub fn targets(albums: &[(String, String, String)]) -> BTreeMap<String, Vec<PathBuf>> {
    let mut by_artist: BTreeMap<&str, Vec<&(String, String, String)>> = BTreeMap::new();
    for a in albums.iter().filter(|a| !a.1.is_empty() && a.1 != VARIOUS) {
        by_artist.entry(&a.1).or_default().push(a);
    }
    by_artist
        .into_iter()
        .map(|(artist, own)| {
            let parents: Vec<&Path> = own.iter().filter_map(|a| Path::new(&a.0).parent()).collect();
            let folder = parents.iter().skip(1).fold(parents.first().map(|p| p.to_path_buf()).unwrap_or_default(), |acc, p| common_ancestor(&acc, p));
            let root = Path::new(&own[0].2);
            let alone = own.iter().all(|a| a.2 == own[0].2)
                && folder.starts_with(root)
                && folder != root
                && !albums.iter().any(|o| o.1 != artist && Path::new(&o.0).starts_with(&folder));
            let places = if alone { vec![folder] } else { own.iter().map(|a| PathBuf::from(&a.0)).collect() };
            (artist.to_string(), places)
        })
        .collect()
}

fn has_image(dir: &Path) -> bool {
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        let name = e.file_name().to_string_lossy().to_lowercase();
        name.strip_prefix("artist.").is_some_and(|ext| ["jpg", "jpeg", "png", "webp"].contains(&ext))
    })
}

/// Fetches and saves the image of every album artist whose folders lack one. Never overwrites a file.
pub fn images(s: &Settings) -> Done {
    let c = db::open();
    let rows: Vec<(String, String, String)> = c
        .prepare(
            "SELECT t.dir, COALESCE(MAX(substr(json_extract(t.tags,'$.MUSICBRAINZ_ALBUMARTISTID'),1,36)),''), l.path
             FROM tracks t JOIN libraries l ON l.id=t.library_id GROUP BY t.dir",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let recent = |id: &str| -> bool {
        c.query_row("SELECT 1 FROM artist_images WHERE artist_id=?1 AND checked>?2", params![id, db::now() - RETRY_DAYS * 86400], |_| Ok(()))
            .is_ok()
    };
    let todo: Vec<(String, Vec<PathBuf>)> = targets(&rows)
        .into_iter()
        .filter(|(id, _)| !recent(id))
        .filter_map(|(id, places)| {
            let missing: Vec<PathBuf> = places.into_iter().filter(|p| !has_image(p)).collect();
            (!missing.is_empty()).then_some((id, missing))
        })
        .collect();
    job::phase("artist images", todo.len() as u64);
    let mut done = Done { saved: 0, missing: 0 };
    for (id, places) in todo {
        let found = match remote::artist_image(&s.contact_email, &id, 1000) {
            Ok(Some(bytes)) => {
                let ext = if bytes.starts_with(b"\x89PNG") { "png" } else { "jpg" };
                for p in &places {
                    let file = p.join(format!("artist.{ext}"));
                    // create_new: a file that appeared meanwhile is kept
                    let wrote = std::fs::OpenOptions::new().write(true).create_new(true).open(&file).and_then(|mut f| std::io::Write::write_all(&mut f, &bytes));
                    match wrote {
                        Ok(()) => done.saved += 1,
                        Err(e) => eprintln!("{}: {e}", file.display()),
                    }
                }
                true
            }
            Ok(None) => {
                done.missing += 1;
                false
            }
            Err(e) => {
                eprintln!("artist image {id}: {e}");
                job::tick();
                continue; // not recorded: asked again next run
            }
        };
        c.execute("INSERT OR REPLACE INTO artist_images(artist_id,checked,found) VALUES(?1,?2,?3)", params![id, db::now(), found]).unwrap();
        job::tick();
    }
    if done.saved > 0 {
        job::navidrome(s); // so the artist pages pick the images up now
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_folders() {
        let row = |d: &str, a: &str| (d.to_string(), a.to_string(), "/music".to_string());
        let rows = vec![
            row("/music/Tuff/Fire", "a"),
            row("/music/Tuff/Water", "a"),
            row("/music/Solo/Only", "b"),
            row("/music/Mixed/One", "c"),
            row("/music/Mixed/Two", "d"),
            row("/music/Flat - Album", "e"),
            row("/music/Comp", VARIOUS),
        ];
        let t = targets(&rows);
        assert_eq!(t["a"], vec![PathBuf::from("/music/Tuff")], "the folder holding all of an artist's albums");
        assert_eq!(t["b"], vec![PathBuf::from("/music/Solo")]);
        assert_eq!(t["c"], vec![PathBuf::from("/music/Mixed/One")], "a shared folder isn't one artist's");
        assert_eq!(t["e"], vec![PathBuf::from("/music/Flat - Album")], "never the library root");
        assert!(!t.contains_key(VARIOUS));
    }
}
