# Duplicate detection (`shoebox dupes`)

## Why

Jay wants to clean duplicate photos and videos out of albums, and eventually
the whole library (24k assets) in batches. Sometimes only byte-identical copies
should go; sometimes "basically the same shot" should be surfaced too.

Photos' AppleScript cannot delete media items, only albums and folders. So
shoebox never deletes anything. It sorts candidates into albums, and Jay
deletes or reviews them in Photos.

## Two modes

**Exact (`--match exact`, default).** Byte-identical originals. Fast, no image
decoding.

- Group assets by `ZADDITIONALASSETATTRIBUTES.ZORIGINALSTABLEHASH` from
  `Photos.sqlite`. Exact copies can be imported years apart, so there's no
  window. The scope only decides which hashes to look up. Matching is always
  library-wide. With `--album`, a copy elsewhere in the library still
  counts. If the survivor is outside the album, it gets added to the album
  (part of the merge below).
- One **survivor** per group. Every other copy goes to the delete album.
- Survivor = the copy with the most on it, in strict priority order
  *(Claude's call)*: edited (has adjustments), favorite,
  title/description/keywords present, number of albums it's in. Tie: oldest
  `ZADDEDDATE`.
- Some differences between copies can't be merged through AppleScript: two
  *different* edits, or differing date or location *(Claude's call)*. A group
  with any of these is skipped and counted as `skipped-unmergeable`. If a merge
  step fails, that group's losers aren't queued.
- Before the losers are queued, merge onto the survivor *(Claude's call)*:
  add the survivor to every album a loser is in, union keywords, set favorite
  if any copy is favorited, and take the first non-empty title/description if
  the survivor has none. Deleting a copy then loses nothing.
- Assets with no stable hash are skipped and counted in the summary (none in
  the visible library today). `--match similar` still covers them.
- Only assets AppleScript can address are in play. Filter the DB to
  non-trashed, `ZVISIBILITYSTATE=0`, and not shared-album content
  (`ZCOLLECTIONSHARE IS NULL`), then intersect with `all_photo_ids()`. That
  drops hidden and system-saved rows: 13,299 assets today, against 24k DB
  rows.

**Similar (`--match similar`).** Visually near-identical images/videos. Slower.

- Sort the scope by capture date (`ZDATECREATED`). Compare each asset with its
  neighbours within a sliding window: `--window N` assets either side
  (default 20, *Claude's call*).
- Compare by image similarity on the thumbnails Photos already keeps in
  `resources/derivatives/` (most originals are iCloud-only; the derivatives are
  local). For videos, compare the poster frame. *Claude's call:* Apple Vision's
  feature print (`VNGenerateImageFeaturePrintRequest`, distance between prints)
  is the suggested method. The builder may pick a perceptual hash instead if
  it's simpler and good enough. Either way, compare videos only with videos and
  photos only with photos.
- Pairs under the threshold are joined into groups (transitively).
- **No survivor.** Every member of a similar group goes to a review album, so
  Jay picks what stays.
- `--threshold` sets how close counts as similar. Named presets
  `--match near` (tight: re-saves, crops of the same frame) and
  `--match similar` (loose: same scene a second apart) map to default
  thresholds that the builder tunes on the real library. Raw `--threshold`
  overrides either one.
- Groups that are fully exact duplicates are dropped from similar results, so
  identical copies don't flood the review albums. Run exact first.

## Scope and batches

Scope flags exist on `dupes` only *(Claude's call; existing commands stay as
they are)*: `--album NAME|UUID`, or the whole library narrowed by
`--from DATE` / `--to DATE` (capture date, inclusive). Batching = running date slices one at a
time. In similar mode, read `--window` extra assets past each edge, so a group
that straddles a slice boundary is still found. Trashed assets are always
excluded.

## Output albums

All albums go in a top-level folder `Shoebox` *(Claude's call)*. ISO-week
stamps let runs in the same week accumulate, and a new week starts fresh.

- `Shoebox Delete 2026-W43`: exact-match losers. No size cap. Jay
  select-alls and deletes.
- `Shoebox Review 2026-W43 #1`, `#2`, …: similar groups, about 32 assets per
  album (`--review-size`, default 32). A group is never split. If adding a
  group would exceed the cap, it starts the next album. A group bigger than the
  cap gets an album of its own. Same-week runs fill the last unfilled `#n`
  before opening a new one.
- Earlier weeks' albums never get new assets.
- Assets already in any `Shoebox Delete/Review` album are skipped, so re-runs
  don't re-queue the same things *(Claude's call)*.
- **Final cleanup, at the end of every run (no scheduler):** delete any
  `Shoebox Delete/Review` album from an earlier ISO week that is now empty.
  Jay's workflow is to delete the unwanted photos, take keepers out of the
  album, and let the next run clear it away. Non-empty albums and current-week
  albums are left alone. Deleting an album never deletes photos. `--dry-run`
  reports which albums it would delete, without deleting them.

## CLI shape

```sh
shoebox dupes --album "Trip 2024"                     # exact, album scope
shoebox dupes --from 2019-01-01 --to 2019-06-30 --match similar
shoebox dupes --match near --dry-run | jq .           # report only, no albums
```

`--dry-run` prints one JSON line per group:
`{match, survivor?, members:[uuid…], distance?}`. A normal run prints the same
lines plus a final summary line (groups, queued for delete, queued for review,
skipped-no-hash, skipped-no-thumbnail, albums written to stderr).

## Constraints

- Read `Photos.sqlite` read-only (`mode=ro`), never write it. All library
  changes go through the existing AppleScript layer (album create/add, keyword
  and favorite setters).
- The terminal needs read access to the library package. It works today; if
  it's denied, say "grant Full Disk Access to your terminal".
- Keep to the repo's style: a single binary, the existing `-j` worker fan-out
  where AppleScript round trips dominate, and JSON lines output.
- Album-scope `remove` recreates albums (uuid changes). Dupes never removes
  from albums; it only adds to Shoebox albums.

## Non-goals

- Deleting assets, or emptying Recently Deleted.
- Burst/Live Photo special handling beyond what falls out naturally.
- Cross-library comparison.
- A GUI review step. Photos itself is the review UI.

## Anti-goals

- Queueing a copy for deletion that holds data the survivor lacks (albums,
  keywords, favorite, title).
- Any similar match ending up in the delete album.
- Review albums so big that reviewing is a chore.

## Left to the builder

Library choice for SQLite and image comparison; the default thresholds (tune
on this library: 54 exact groups exist today, almost all pairs); how "edited"
is detected; how the library path is found (default system library vs. the one
Photos has open); and test shape (a fixture SQLite for exact grouping/survivor
scoring, a unit test for review-album packing).
