# Covers and lyrics

Both are fetched per album during the album pass of a full run, or on their own with **Jobs → Fetch covers** and
**Jobs → Fetch lyrics**. Either way, every track is still written at most once, with tags, cover and lyrics
together.

## Covers

Source: the [Cover Art Archive](https://coverartarchive.org), using the album's MusicBrainz release id, so an
album needs a release id first.

- **Embed covers** (`cover_embed`, on by default) writes the picture into each track that has none.
- **Cover size**: `250`, `500`, `1200` (default) or `original`.
- **Replace smaller covers** (`cover_upgrade_below`, default 500): an embedded cover whose shorter side is below
  this many pixels is replaced, but only by a bigger one. The old picture is stored in the undo log, so undo
  restores it. `0` disables upgrades.
- **Shrink larger covers** (`cover_shrink_above`, default 0 = off): an embedded cover whose shorter side is above
  this many pixels (a 3000 px scan, say) is replaced by the Cover Art Archive one at **Cover size**, if that is
  smaller. The album needs a release id. Undo restores the original. Only the front cover is touched; other
  embedded pictures (back, booklet) stay.
- **Save cover.jpg** (`cover_save_file`) also writes the fetched cover next to the tracks.

### Cover files next to the music

A `cover.jpg`, `folder.jpg`, `cover.png` or `folder.png` in the album folder counts as the album's cover. Such an
album is not reported as missing a cover, and xmusic does not embed a picture into its tracks, because players
like Navidrome already use the file. The album view shows a *cover file* badge. This is detected during scans,
so run a scan after adding cover files.

## Artist images

Navidrome shows an `artist.*` file on the artist page. xmusic can save one for every album artist that has a
MusicBrainz id: it follows the artist's MusicBrainz entry to Wikidata and downloads the photo listed there from
Wikimedia Commons, 1000 px wide.

- Run it with **Jobs → Fetch artist images**, or tick **Settings → Cover & lyrics → Save artist.jpg** to do it
  at the end of every full run.
- The image goes in the folder that holds all of the artist's albums (`Music/Artist/`). In a flat library
  (`Music/Artist - Album/`), or when that folder also holds other artists' albums, each of the artist's album
  folders gets one instead; Navidrome reads it there too. The library root never gets one.
- An existing `artist.jpg`, `.jpeg`, `.png` or `.webp` is never replaced. Various Artists is skipped.
- Artists without a photo on Wikidata are asked again after 30 days. Navidrome is asked to rescan when images
  were saved.
- Commons photos are freely licensed, most under Creative Commons with attribution; fine for a personal
  library, but check the photo's page before publishing it elsewhere.
- Bios aren't saved: Navidrome has no local file for them and gets them from Last.fm itself.

## Lyrics

Source: [LRCLIB](https://lrclib.net), searched with the track's artist, title and album.

- **Lyrics** (`lyrics_enabled`) turns lookups on.
- **Prefer synced** picks time-stamped (LRC) lyrics when available, plain text otherwise.
- **Embed** writes a `LYRICS` tag into the file.
- **Save .lrc** writes a `.lrc` file beside the track. It is a separate small file, not a rewrite of the audio.

Tracks without a match are remembered so they are not asked about again on every run. Lookups are spaced out
(about four per second), and if LRCLIB answers with a server error (429, 502, 503, 504) xmusic retries and then
pauses lyrics for ten minutes instead of hammering it; the rest of the run carries on.

The album view shows which tracks have lyrics:

![Album](img/album.png)
