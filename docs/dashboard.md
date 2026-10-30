# The dashboard

The whole UI is one page served by the binary (daisyUI, no external requests). The sidebar has Overview,
Library, Review, Jobs, Settings and Users. A badge next to **Jobs** shows when a job is running, and the
Overview carries a bar with its progress and a Cancel button. The clock icon drops down the five most recent jobs, and the
sun/moon switches between the light and dark theme. The theme starts out matching your system setting,
and the screenshots here use the dark one.

## Overview

![Overview](img/overview.png)

Totals, a format catalog, library health (share of tracks with a MusicBrainz id, cover and lyrics), charts of
formats and audio quality, and the review queue. The yellow banner appears whenever albums wait for you.

## Library

![Library](img/library.png)

Four tabs: **Albums** (cover grid, with badges such as `untagged`), **Artists**, **Duplicates** and **Health**. The search
box matches album, artist and track titles.

Opening an album shows its tracks with a lyrics badge for each, the cover size, a link to the MusicBrainz
release, a link to its proposal, and three buttons:

- **Re-match** looks the album up again; the result lands in Review.
- **Fetch cover** embeds the Cover Art Archive front cover when the album has none, or a too small or too big one
  (see [Covers and lyrics](covers-and-lyrics.md)). It needs a MusicBrainz release id.
- **Fetch lyrics** asks LRCLIB for every track without lyrics, including tracks that weren't found before.

Each one starts a small job for just that album, shown on the Jobs page.

![Album](img/album.png)

An album with a `cover.jpg` or `folder.jpg` next to its tracks counts as having a cover and shows a
*cover file* badge.

Duplicates are covered in [Duplicates](duplicates.md).

### Health

![Health](img/health.png)

A report built from the tags of the last scan (no file reads, no network), one section per problem:

- **Missing track numbers**: tracks without `TRACKNUMBER`.
- **Gaps in track numbers**: numbers missing between 1 and the last track (or `TRACKTOTAL`), per disc.
- **Disc numbers**: albums with discs in subfolders (`CD1/`, `Disc 2/`) whose `DISCNUMBER`, `DISCTOTAL` or
  `TRACKTOTAL` don't match the folders, for example disc 2's files all saying disc 1, or the whole album's
  track count as each disc's total.
- **Mixed album artists**: an album folder whose tracks disagree on `ALBUMARTIST`.
- **Artist spellings**: names that differ only in case, accents, punctuation, `&`/`and` or a leading "The"
  (`Mötley Crüe` / `Motley Crue`).
- **Lossy files in FLAC albums**: MP3, AAC or Ogg tracks in a folder that also holds FLAC.

Album folders open the album view; artist names open their albums.

Fixing from the report:

- **Use** next to a spelling (admins) renames the other spellings to it in `ARTIST` and `ALBUMARTIST`, as a job
  that only writes the affected files (undo restores them). It also adds rewrite rules for both fields, so a
  later match or a new file can't bring the old spelling back. Picking a different spelling later replaces those
  rules.
- **Fix** on a disc numbers row (admins), or **Fix all**, writes the numbers from the folders: `DISCNUMBER`
  from the folder name, `DISCTOTAL` from the number of disc folders, and `TRACKTOTAL` from the tracks on that
  disc. A total is only lowered when it can't be right, so a release's count isn't "fixed" just because a track
  file is missing. Undo restores the old values, and fields under *Never overwrite* are left alone.
- **Re-match** on an album row (missing numbers, gaps, mixed album artists) matches it again; the result lands
  in Review. A gap caused by a missing file needs the file.

## Review

![Review](img/review.png)

The queue of albums that need a person: **Needs review** (a release was found, but not confidently),
**Needs manual** (nothing scored high enough), **Approved** (waiting to be written), **Applied** and **Ignored**.
Select several rows to approve, ignore or re-match them in bulk.

Opening one shows the candidates and the exact tag changes:

![Proposal](img/proposal.png)

Old values are struck through, new ones are editable. See [Matching and review](matching-and-review.md).

## Jobs

![Jobs](img/jobs.png)

While a job runs, the top card shows its stages as steps: finished stages get a ✓ and the time they
took, the current one shows its progress (for example `match (12 / 80)`), and the stages still to come follow
in grey. The progress bar and the estimates below are for the current stage and the whole run.

**Run now** does a full run. The four buttons below it run one step: **Scan**, **Apply approved**,
**Fetch covers**, **Fetch lyrics**. *Maintenance* holds **Apply custom tags & rewrites** and **Undo all
changes**. Below are the open issues (albums missing an id, cover or lyrics) and the job history with how
long each phase took. See [Jobs and automation](jobs-and-automation.md).

## Settings

Six tabs: General, Libraries, Metadata, Cover & lyrics, Integrations, Backup. Admin only. See the
[Settings reference](settings.md).

## Users

Admins create users (admin or regular), change their passwords and delete them.

## On a phone

The layout adapts to narrow screens: the sidebar becomes a menu and some table columns fold into the row.

<img src="img/phone.png" alt="Library on a phone" width="320">
