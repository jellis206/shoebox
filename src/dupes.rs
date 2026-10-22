//! Duplicate finding: exact (same original file) and similar (same-looking thumbnail).
//! Pure logic; `shoebox dupes` does the reading and the album writes.

use std::collections::HashMap;
use std::path::Path;

use chrono::{Datelike, NaiveDate};

use crate::library::{Asset, DELETE_PREFIX, REVIEW_PREFIX};
use crate::{Error, Result};

/// "2026-W43": ISO week, so late-December days can belong to next year's week 1.
pub fn iso_week_label(day: NaiveDate) -> String {
    let w = day.iso_week();
    format!("{}-W{:02}", w.year(), w.week())
}

pub fn delete_album_name(week: &str) -> String {
    format!("{DELETE_PREFIX}{week}")
}

pub fn review_album_name(week: &str, n: usize) -> String {
    format!("{REVIEW_PREFIX}{week} #{n}")
}

/// The week label of a Shoebox Delete/Review album title, if it is one.
pub fn album_week(title: &str) -> Option<&str> {
    let rest = title
        .strip_prefix(DELETE_PREFIX)
        .or_else(|| title.strip_prefix(REVIEW_PREFIX))?;
    let week = rest.split(" #").next()?;
    let b = week.as_bytes();
    let ok = b.len() == 8
        && b[..4].iter().all(u8::is_ascii_digit)
        && &b[4..6] == b"-W"
        && b[6..].iter().all(u8::is_ascii_digit);
    ok.then_some(week)
}

/// What a survivor needs copied onto it so deleting the losers loses nothing.
#[derive(Debug, Default, PartialEq)]
pub struct Merge {
    pub albums: Vec<String>,
    /// The full new keyword set, if it grew.
    pub keywords: Option<Vec<String>>,
    pub favorite: bool,
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: Option<(f64, f64)>,
}

impl Merge {
    pub fn is_empty(&self) -> bool {
        *self == Merge::default()
    }
}

#[derive(Debug)]
pub enum Exact<'a> {
    Queue {
        survivor: &'a Asset,
        losers: Vec<&'a Asset>,
        merge: Merge,
    },
    Skip {
        members: Vec<&'a Asset>,
        reason: &'static str,
    },
}

impl<'a> Exact<'a> {
    pub fn members(&self) -> Vec<&'a Asset> {
        match self {
            Exact::Queue {
                survivor, losers, ..
            } => std::iter::once(*survivor).chain(losers.iter().copied()).collect(),
            Exact::Skip { members, .. } => members.clone(),
        }
    }
}

/// Groups assets sharing an original-file hash, ordered by capture time.
pub fn exact_groups(assets: &[Asset]) -> Vec<Exact<'_>> {
    let mut by_hash: HashMap<&str, Vec<&Asset>> = HashMap::new();
    for a in assets {
        if let Some(h) = &a.hash {
            by_hash.entry(h).or_default().push(a);
        }
    }
    let mut groups: Vec<Vec<&Asset>> = by_hash.into_values().filter(|g| g.len() > 1).collect();
    for g in &mut groups {
        g.sort_by(|x, y| x.created.total_cmp(&y.created).then(x.uuid.cmp(&y.uuid)));
    }
    groups.sort_by(|x, y| x[0].created.total_cmp(&y[0].created));
    groups.into_iter().map(judge).collect()
}

fn judge(members: Vec<&Asset>) -> Exact<'_> {
    let skip = |reason| Exact::Skip {
        members: members.clone(),
        reason,
    };
    let mut edits: Vec<&str> = members.iter().filter_map(|a| a.edit.as_deref()).collect();
    edits.sort();
    edits.dedup();
    if edits.len() > 1 {
        return skip("different-edits");
    }
    // Dates are ignored: the bytes match, and the survivor keeps its own date.
    // A missing location is filled in from a copy; only two conflicting ones skip.
    let same_place = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5;
    let places: Vec<(f64, f64)> = members.iter().filter_map(|a| a.location).collect();
    if places.iter().any(|&p| !same_place(p, places[0])) {
        return skip("different-location");
    }

    let rank = |a: &Asset| {
        let meta = !(a.title.is_empty() && a.description.is_empty() && a.keywords.is_empty());
        (a.edit.is_some(), a.favorite, meta, a.albums.len())
    };
    // highest rank wins; ties go to the oldest import
    let survivor = *members
        .iter()
        .max_by(|x, y| rank(x).cmp(&rank(y)).then(y.added.total_cmp(&x.added)))
        .expect("group has members");
    let losers: Vec<&Asset> = members
        .iter()
        .copied()
        .filter(|a| a.uuid != survivor.uuid)
        .collect();
    if losers.iter().any(|a| a.other_albums) {
        return skip("non-user-album");
    }

    let mut merge = Merge::default();
    let mut keywords = survivor.keywords.clone();
    for l in &losers {
        for album in &l.albums {
            if !survivor.albums.contains(album) && !merge.albums.contains(album) {
                merge.albums.push(album.clone());
            }
        }
        for k in &l.keywords {
            if !keywords.contains(k) {
                keywords.push(k.clone());
            }
        }
        merge.favorite |= l.favorite && !survivor.favorite;
        if survivor.title.is_empty() && merge.title.is_none() && !l.title.is_empty() {
            merge.title = Some(l.title.clone());
        }
        if survivor.description.is_empty()
            && merge.description.is_none()
            && !l.description.is_empty()
        {
            merge.description = Some(l.description.clone());
        }
    }
    if keywords.len() > survivor.keywords.len() {
        merge.keywords = Some(keywords);
    }
    if survivor.location.is_none() {
        merge.location = places.first().copied();
    }
    Exact::Queue {
        survivor,
        losers,
        merge,
    }
}

/// 64-bit difference hash of an image: robust to scaling and recompression.
pub fn dhash(path: &Path) -> Result<u64> {
    let img = image::open(path)
        .map_err(|e| Error::new(format!("{}: {e}", path.display())))?
        .to_luma8();
    let small = image::imageops::resize(&img, 9, 8, image::imageops::FilterType::Triangle);
    let mut bits = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            bits = bits << 1 | u64::from(small.get_pixel(x, y)[0] < small.get_pixel(x + 1, y)[0]);
        }
    }
    Ok(bits)
}

/// Groups of indices into `items` (sorted by capture time) whose thumbnails are within
/// `max_dist` bits, comparing each item with the next `window` items of the same kind.
/// Pairs that are exact duplicates of each other don't link; exact mode handles them.
pub fn similar_groups(items: &[(&Asset, u64)], window: usize, max_dist: u32) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..items.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for i in 0..items.len() {
        let (a, ha) = items[i];
        for j in i + 1..items.len().min(i + 1 + window) {
            let (b, hb) = items[j];
            let exact = a.hash.is_some() && a.hash == b.hash;
            if a.video == b.video && !exact && (ha ^ hb).count_ones() <= max_dist {
                let (ri, rj) = (root(&mut parent, i), root(&mut parent, j));
                parent[rj] = ri;
            }
        }
    }
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut slot: HashMap<usize, usize> = HashMap::new();
    for i in 0..items.len() {
        let r = root(&mut parent, i);
        let k = *slot.entry(r).or_insert_with(|| {
            groups.push(vec![]);
            groups.len() - 1
        });
        groups[k].push(i);
    }
    groups.retain(|g| g.len() > 1);
    groups
}

/// Packs groups (by size, in order) into review albums of about `cap` assets without
/// splitting a group. Bucket 0 continues the current album holding `last_fill` assets
/// (None: there is none, so bucket 0 stays empty); the rest are new albums.
pub fn pack_review(sizes: &[usize], cap: usize, last_fill: Option<usize>) -> Vec<Vec<usize>> {
    let mut out = vec![vec![]];
    let mut fill = last_fill.unwrap_or(cap);
    for (i, &n) in sizes.iter().enumerate() {
        if fill > 0 && fill + n > cap {
            out.push(vec![]);
            fill = 0;
        }
        out.last_mut().expect("never empty").push(i);
        fill += n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(uuid: &str) -> Asset {
        Asset {
            uuid: uuid.into(),
            hash: Some("h".into()),
            ..Asset::default()
        }
    }

    fn survivor<'a>(g: &'a Exact) -> &'a str {
        match g {
            Exact::Queue { survivor, .. } => &survivor.uuid,
            Exact::Skip { reason, .. } => panic!("skipped: {reason}"),
        }
    }

    fn skipped(assets: &[Asset]) -> &'static str {
        match &exact_groups(assets)[0] {
            Exact::Skip { reason, .. } => *reason,
            Exact::Queue { .. } => panic!("not skipped"),
        }
    }

    #[test]
    fn week_labels_use_the_iso_year() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(iso_week_label(d(2026, 10, 22)), "2026-W43");
        assert_eq!(iso_week_label(d(2026, 12, 31)), "2026-W53");
        assert_eq!(iso_week_label(d(2027, 1, 1)), "2026-W53");
        assert_eq!(iso_week_label(d(2024, 12, 30)), "2025-W01");
        assert_eq!(album_week("Shoebox Review 2026-W43 #2"), Some("2026-W43"));
        assert_eq!(album_week(&delete_album_name("2026-W01")), Some("2026-W01"));
        assert_eq!(album_week("Shoebox Delete stuff"), None);
        assert_eq!(album_week("Holiday 2026-W43"), None);
    }

    #[test]
    fn survivor_priority() {
        let mut a = asset("a");
        let mut b = asset("b");
        a.added = 1.0;
        b.added = 2.0;
        assert_eq!(survivor(&exact_groups(&[b.clone(), a.clone()])[0]), "a", "oldest import");
        b.albums = vec!["x".into()];
        assert_eq!(survivor(&exact_groups(&[a.clone(), b.clone()])[0]), "b", "more albums");
        a.keywords = vec!["k".into()];
        assert_eq!(survivor(&exact_groups(&[a.clone(), b.clone()])[0]), "a", "metadata");
        b.favorite = true;
        assert_eq!(survivor(&exact_groups(&[a.clone(), b.clone()])[0]), "b", "favorite");
        a.edit = Some("e".into());
        let pair = [a, b];
        let groups = exact_groups(&pair);
        assert_eq!(survivor(&groups[0]), "a", "edited");
        let Exact::Queue { merge, .. } = &groups[0] else { unreachable!() };
        assert_eq!(
            *merge,
            Merge {
                albums: vec!["x".into()],
                keywords: None,
                favorite: true,
                ..Merge::default()
            }
        );
    }

    #[test]
    fn unmergeable_groups_are_skipped() {
        let (mut a, mut b) = (asset("a"), asset("b"));
        a.edit = Some("e1".into());
        b.edit = Some("e2".into());
        assert_eq!(skipped(&[a, b]), "different-edits");

        let (mut a, mut b) = (asset("a"), asset("b"));
        a.location = Some((3.0, 4.0));
        b.location = Some((1.0, 2.0));
        assert_eq!(skipped(&[a, b]), "different-location");

        // different dates queue; a missing location is filled from the other copy
        let (mut a, mut b) = (asset("a"), asset("b"));
        a.favorite = true;
        b.created = 60.0;
        b.location = Some((1.0, 2.0));
        let pair = [a, b];
        let Exact::Queue { survivor, merge, .. } = &exact_groups(&pair)[0] else {
            panic!("skipped")
        };
        assert_eq!(survivor.uuid, "a");
        assert_eq!(merge.location, Some((1.0, 2.0)));

        let (mut a, mut b) = (asset("a"), asset("b"));
        a.favorite = true;
        b.other_albums = true;
        assert_eq!(skipped(&[a, b]), "non-user-album");

        let (mut a, b) = (asset("a"), asset("b"));
        a.hash = Some("other".into());
        assert!(exact_groups(&[a, b]).is_empty());
    }

    #[test]
    fn similar_groups_respect_window_kind_and_exact_pairs() {
        let assets: Vec<Asset> = (0..30)
            .map(|i| Asset {
                uuid: i.to_string(),
                hash: Some(i.to_string()),
                ..Asset::default()
            })
            .collect();
        // 0 and 25 look the same but sit outside a 20-item window
        // scrambled hashes sit ~32 bits apart from each other
        let mut items: Vec<(&Asset, u64)> = (1u64..)
            .zip(&assets)
            .map(|(i, a)| (a, i.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
            .collect();
        items[0].1 = 0;
        items[25].1 = 0;
        items[3].1 = 0b111; // 3 bits from item 0
        assert_eq!(similar_groups(&items, 20, 4), vec![vec![0, 3]]);
        assert_eq!(similar_groups(&items, 25, 4), vec![vec![0, 3, 25]]);
        assert_eq!(similar_groups(&items, 20, 2), Vec::<Vec<usize>>::new());

        let mut video = assets[3].clone();
        video.video = true;
        items[3].0 = &video;
        assert_eq!(similar_groups(&items, 20, 4), Vec::<Vec<usize>>::new());

        let twin = Asset {
            hash: assets[0].hash.clone(),
            ..assets[3].clone()
        };
        items[3].0 = &twin;
        assert_eq!(similar_groups(&items, 20, 4), Vec::<Vec<usize>>::new());
    }

    #[test]
    fn review_packing_never_splits_groups() {
        assert_eq!(
            pack_review(&[20, 20, 40, 5], 32, Some(30)),
            vec![vec![], vec![0], vec![1], vec![2], vec![3]]
        );
        assert_eq!(pack_review(&[2, 2], 32, Some(28)), vec![vec![0, 1]]);
        assert_eq!(pack_review(&[2, 2], 32, None), vec![vec![], vec![0, 1]]);
        assert_eq!(pack_review(&[40], 32, Some(0)), vec![vec![0]]);
    }
}
