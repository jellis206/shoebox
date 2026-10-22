# Duplicate Detection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shoebox dupes` finds exact and similar duplicates and queues them
into week-stamped Shoebox albums.

**Architecture:** Read `Photos.sqlite` read-only by shelling out to
`/usr/bin/sqlite3 -json` and parsing with `serde_json` (no SQLite crate). The
grouping, scoring and packing logic is pure Rust in `src/dupes.rs`. Photos is
changed only through the existing AppleScript layer.

**Tech Stack:** Rust 2024, clap, serde_json, chrono (+`clock`), `image`
(jpeg only) for dHash.

**Spec:** `docs/superpowers/specs/2026-10-22-duplicate-detection-design.md`

## Global Constraints

- DB opened as `file:<db>?mode=ro`. Never `immutable=1` (it skips the WAL).
- Core Data join tables are resolved from `Z_PRIMARYKEY` entity ids. Never
  hardcode `Z_34ASSETS` and the like.
- Albums are `Shoebox Delete YYYY-Www` and `Shoebox Review YYYY-Www #n`, in
  top-level folder `Shoebox`. Review cap is 32 by default.
- ISO-week year (`iso_week().year()`), not the calendar year.
- `ZDATECREATED` counts seconds from 2001-01-01 UTC.

## Review Focus

- Late-December and early-January dates get the right ISO week year (test in Task 2).
- A loser in a non-user album (shared, etc.) makes its group unmergeable (Task 2).
- A Review group bigger than the cap gets its own album and never splits (Task 2).
- Cleanup counts only non-trashed members when deciding an album is empty (Task 1 query).
- A merge step failing partway means that group's losers are not queued (Task 3).

---

### Task 1: Library reader (`src/library.rs`)

**Files:** Create `src/library.rs`. Modify `src/lib.rs` (`pub mod library;`,
plus `Album::add_ids`).

**Produces:**
- `pub struct Asset { uuid, video: bool, created: f64, added: f64, favorite: bool, edit: Option<String> /* adjusted stable hash or timestamp */, title: String, description: String, keywords: Vec<String>, location: Option<(f64,f64)>, hash: Option<String>, albums: Vec<String> /* user album uuids */, other_albums: bool }`
- `pub struct ShoeboxAlbum { uuid, title, members: usize /* non-trashed */ }`
- `pub struct Library { pub path: PathBuf }`, with
  `fn assets(&self) -> Result<Vec<Asset>>` (visible filter from the spec),
  `fn album_members(&self, name_or_uuid) -> Result<Vec<String>>`,
  `fn shoebox_albums(&self) -> Result<Vec<ShoeboxAlbum>>` and
  `fn thumbnail(&self, uuid) -> Option<PathBuf>`
  (`resources/derivatives/masters/<c>/<UUID>_4_5005_c.jpeg`).
- `Album::add_ids(&self, ids: &[String]) -> Result<()>` takes suffixed ids,
  sends them in chunks of 200, and does no per-photo validation.

- [ ] Unit test `entity_tables` builds the join-table names from a
  `Z_PRIMARYKEY` sample: Album=34, Asset=3 → `Z_34ASSETS(Z_34ALBUMS, Z_3ASSETS)`.
- [ ] Implement it, and check it against the live library (13,299 assets, 54 hash groups).
- [ ] Commit.

### Task 2: Pure logic (`src/dupes.rs`)

**Produces:**
- `iso_week_label(NaiveDate) -> String` (`"2026-W43"`)
- `exact_groups(&[Asset]) -> Vec<Exact>`, where
  `enum Exact { Queue { survivor, losers, merge: Merge }, Skip { members, reason: &'static str } }`
  and `Merge { albums, keywords, favorite, title, description }`. Each field
  holds only what the survivor lacks.
- `dhash(&Path) -> Result<u64>`
- `similar_groups(&[(Asset, u64)], window: usize, max_dist: u32) -> Vec<Vec<usize>>`.
  Assets are sorted by date. Compare same kind only, and skip pairs with the
  same original hash. Union-find, dropping singletons.
- `pack_review(sizes: &[usize], cap: usize, last_fill: Option<usize>) -> Vec<Vec<usize>>`.
  The first bucket continues the existing album when `last_fill` is `Some`.

- [ ] Tests:
  - `iso_week_label(2026-12-31) == "2026-W53"` and `2027-01-01 → "2026-W53"`.
  - Survivor priority runs edited > favorite > metadata > albums > oldest added.
  - Two different edits → `Skip("different-edits")`. Date differs → `Skip("different-date")`.
    Location differs → `Skip("different-location")`. `other_albums` on a loser → `Skip("non-user-album")`.
  - `pack_review([20,20,40,5], 32, Some(30)) == [[], [0], [1], [2], [3]]`. An
    empty continuation bucket is allowed, but a group is never split.
  - A window test where items 25 positions apart are not grouped with `window=20`.
- [ ] Implement, run `cargo test`, commit.

### Task 3: `shoebox dupes` command (`src/main.rs`)

Flags: `--match exact|near|similar`, `--threshold N`, `--window 20`,
`--album`, `--from/--to YYYY-MM-DD`, `--review-size 32`, `--dry-run`,
`--library PATH` (default `~/Pictures/Photos Library.photoslibrary`). Presets
use dHash Hamming distance: `near` = 4, `similar` = 10.

Flow:
1. Assets are the DB set ∩ `all_photo_ids()`, minus anything in a Shoebox
   album, narrowed by the scope. Exact mode matches library-wide on the
   scope's hashes. Similar mode pads the date window by `--window` past each
   edge.
2. Print the groups as JSON lines.
3. Unless `--dry-run`:
   - Merge each group through AppleScript. On error, skip the group.
   - Bulk `add_ids` the losers into the Delete album.
   - Pack the similar groups into Review albums.
4. Clean up empty Shoebox albums from earlier weeks (in dry-run, report them only).
5. Write a summary JSON to stderr.

- [ ] Run `--dry-run` across the full library and check that it reports 54 exact groups.
- [ ] Do one real run on a narrow date slice, then check it in Photos.
- [ ] Update the README, then commit.
