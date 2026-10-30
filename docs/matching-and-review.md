# Matching and review

## What an album is

xmusic works per album, and an album is the set of tracks in one folder. Disc subfolders (`CD1/`, `Disc 2/`,
`disk_03 - Bonus/`) count as part of their parent folder, so a two-disc album stays one album. A folder like
`Disc 1 Flow State` is *not* a disc folder, since those usually sit in an artist folder.

## How an album is matched

1. **Which albums.** A full run only matches albums that have no MusicBrainz release id. Albums that already have
   a pending proposal, or that you ignored, are skipped.
2. **Candidates.** MusicBrainz is searched with the album's artist and title tags. If the tags are too poor and
   an AcoustID key is set, xmusic fingerprints up to three tracks with `fpcalc` and looks the recordings up.
3. **Scoring.** Each candidate gets a confidence from 0 to 1 based on how well artist, album title, track count
   and track titles/lengths line up. Releases from your preferred countries and media (`XW`, `US`; Digital
   Media, CD by default) win ties.
4. **Track alignment.** Local files are matched to release tracks, so a file named `track 2` gets the right title
   even when the order of names does not help.
5. **Proposal.** The best candidate becomes a proposal: per file, the tag changes needed.

The search stops at the first candidate that scores above the auto-apply threshold with every track aligned,
which saves MusicBrainz calls. MusicBrainz is queried at most once per second, as their rules require.

## What happens next

| Confidence | Result |
|------------|--------|
| At or above **auto-apply above** (default 0.95) | Written without review (set it to 0 to always review) |
| Between **match threshold** (0.80) and auto-apply | **Needs review**: you approve or ignore it |
| Below the match threshold | **Needs manual**: pick a candidate or paste a release link |

Jobs never wait for a person. Albums that need you are parked in Review and reported when the run ends (a
notification, if configured).

## Reviewing

![Review queue](img/review.png)

Open a row to see the proposal:

![Proposal](img/proposal.png)

- **Candidates & manual pick** lists other releases with their score. **Use** switches the proposal to that
  release; you can also paste a MusicBrainz release link or id.
- **Changes** show each file's old values struck through and the new ones in editable fields. Edit a value and
  it is saved with the proposal.
- **Approve & apply** writes the album. **Ignore** hides it for good. **Re-match** searches again.

Bulk actions work from the list: tick rows, then approve, ignore or re-match them together.

### Re-matching an album that already has an id

**Re-match** (in the album view or on a proposal) scores the album's own release too. The result always goes to
Review and is never auto-applied, so you can see what would change. If nothing differs, the album is marked
applied.

## Which tags are written

Only the fields in **Write fields** (default `TITLE, ARTIST, ALBUM, ALBUMARTIST, DATE, ORIGINALDATE, TRACKNUMBER,
TRACKTOTAL, DISCNUMBER, DISCTOTAL, GENRE, LABEL, CATALOGNUMBER, MUSICBRAINZ_*`) are changed, and fields in **Preserve fields** (`COMMENT`,
`REPLAYGAIN_*`) are never overwritten. Patterns end in `*`.

- `GENRE` is the release's most voted MusicBrainz genre, or its release group's when the release has none.
- `ORIGINALDATE` is the release group's first release date; `DATE` is this release's date.
- `LABEL` and `CATALOGNUMBER` come from the release's first label entry.

Albums applied before these fields existed get them with a **Re-match**.

### Rewrite rules and custom tags

- **Rewrite rules** edit values with a regular expression, per field (for example turn `feat.` into `feat`).
- **Custom tags** add a fixed tag, for example `COLLECTION=Vinyl rips`. The scope is `all`, `path:/music/x` (a
  folder prefix) or `FIELD=value` (case-insensitive match on another tag).

**Jobs → Apply custom tags & rewrites** applies both everywhere without matching.
