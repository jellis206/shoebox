//! Read-only view of a library's `Photos.sqlite`, through `/usr/bin/sqlite3 -json`.
//! Photos itself is only ever changed through AppleScript.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value as Json;

use crate::{Error, Result};

/// Seconds between the Unix epoch and Core Data's (2001-01-01 UTC).
pub const CORE_DATA_EPOCH: i64 = 978_307_200;

/// The folder and album-name prefixes `shoebox dupes` writes to.
pub const SHOEBOX_FOLDER: &str = "Shoebox";
pub const DELETE_PREFIX: &str = "Shoebox Delete ";
pub const REVIEW_PREFIX: &str = "Shoebox Review ";

#[derive(Debug, Clone, Default)]
pub struct Asset {
    pub uuid: String,
    pub video: bool,
    /// Capture time, Core Data seconds.
    pub created: f64,
    /// Import time, Core Data seconds.
    pub added: f64,
    pub favorite: bool,
    /// Identifies the edit (adjusted hash, else adjustment time); None if unedited.
    pub edit: Option<String>,
    pub title: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub location: Option<(f64, f64)>,
    /// Hash of the original file.
    pub hash: Option<String>,
    /// Uuids of the user albums holding this asset.
    pub albums: Vec<String>,
    /// In some other kind of album (shared, ...) that AppleScript can't add to.
    pub other_albums: bool,
}

#[derive(Debug)]
pub struct ShoeboxAlbum {
    pub uuid: String,
    pub title: String,
    /// Non-trashed members.
    pub members: usize,
}

/// Core Data many-to-many join tables, named after entity ids that vary per library.
#[derive(Debug, PartialEq)]
struct Joins {
    album_assets: String,
    album_col: String,
    asset_col: String,
    keywords: String,
    attrs_col: String,
    keyword_col: String,
}

fn joins(entities: &HashMap<String, i64>) -> Result<Joins> {
    let ent = |name: &str| {
        entities
            .get(name)
            .copied()
            .ok_or_else(|| Error::new(format!("Photos.sqlite: no {name} entity")))
    };
    let (album, asset) = (ent("Album")?, ent("Asset")?);
    let (attrs, keyword) = (ent("AdditionalAssetAttributes")?, ent("Keyword")?);
    Ok(Joins {
        album_assets: format!("Z_{album}ASSETS"),
        album_col: format!("Z_{album}ALBUMS"),
        asset_col: format!("Z_{asset}ASSETS"),
        keywords: format!("Z_{attrs}KEYWORDS"),
        attrs_col: format!("Z_{attrs}ASSETATTRIBUTES"),
        keyword_col: format!("Z_{keyword}KEYWORDS"),
    })
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub struct Library {
    pub path: PathBuf,
}

impl Library {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Library { path: path.into() }
    }

    /// `~/Pictures/Photos Library.photoslibrary`
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME").unwrap_or_default();
        Path::new(&home).join("Pictures/Photos Library.photoslibrary")
    }

    fn query(&self, sql: &str) -> Result<Vec<Json>> {
        let db = self.path.join("database/Photos.sqlite");
        // mode=ro reads through the WAL; immutable=1 would skip it and see stale data.
        let uri = format!(
            "file:{}?mode=ro",
            db.display()
                .to_string()
                .replace('%', "%25")
                .replace('?', "%3f")
                .replace('#', "%23")
        );
        let out = Command::new("/usr/bin/sqlite3")
            .args(["-json", "-bail", &uri, sql])
            .output()
            .map_err(|e| Error::new(format!("sqlite3: {e}")))?;
        if !out.status.success() {
            return Err(Error::new(format!(
                "reading {}: {} (grant your terminal Full Disk Access?)",
                db.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        if out.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(vec![]);
        }
        serde_json::from_slice(&out.stdout).map_err(|e| Error::new(format!("sqlite3 output: {e}")))
    }

    fn joins(&self) -> Result<Joins> {
        let entities = self
            .query("select Z_NAME name, Z_ENT ent from Z_PRIMARYKEY")?
            .iter()
            .filter_map(|r| Some((r["name"].as_str()?.to_owned(), r["ent"].as_i64()?)))
            .collect();
        joins(&entities)
    }

    /// Every non-trashed, non-shared asset in the library. Hidden and system-saved
    /// assets are included; intersect with `Photos::all_photo_ids` for what AppleScript sees.
    pub fn assets(&self) -> Result<Vec<Asset>> {
        let j = self.joins()?;
        let sql = format!(
            "select a.ZUUID uuid, a.ZKIND kind, a.ZDATECREATED created, a.ZADDEDDATE added,
                a.ZFAVORITE favorite,
                case when a.ZADJUSTMENTTIMESTAMP is null then null
                     else coalesce(aa.ZADJUSTEDSTABLEHASH, cast(a.ZADJUSTMENTTIMESTAMP as text)) end edit,
                coalesce(aa.ZTITLE, '') title, coalesce(d.ZLONGDESCRIPTION, '') description,
                a.ZLATITUDE lat, a.ZLONGITUDE lon, aa.ZORIGINALSTABLEHASH hash,
                (select json_group_array(k.ZTITLE) from {kw} jk join ZKEYWORD k on k.Z_PK = jk.{kwcol}
                  where jk.{attrcol} = aa.Z_PK) keywords,
                (select json_group_array(g.ZUUID) from {aj} j join ZGENERICALBUM g on g.Z_PK = j.{albcol}
                  where j.{assetcol} = a.Z_PK and g.ZTRASHEDSTATE = 0 and g.ZKIND = 2) albums,
                -- 1510 is Photos' own duplicate-merge bookkeeping, not a real album
                exists(select 1 from {aj} j join ZGENERICALBUM g on g.Z_PK = j.{albcol}
                  where j.{assetcol} = a.Z_PK and g.ZTRASHEDSTATE = 0 and g.ZKIND not in (2, 1510)) other
             from ZASSET a
             join ZADDITIONALASSETATTRIBUTES aa on aa.ZASSET = a.Z_PK
             left join ZASSETDESCRIPTION d on d.ZASSETATTRIBUTES = aa.Z_PK
             where a.ZTRASHEDSTATE = 0 and a.ZVISIBILITYSTATE = 0 and a.ZCOLLECTIONSHARE is null",
            kw = j.keywords,
            kwcol = j.keyword_col,
            attrcol = j.attrs_col,
            aj = j.album_assets,
            albcol = j.album_col,
            assetcol = j.asset_col,
        );
        let list = |v: &Json| -> Vec<String> {
            serde_json::from_str::<Vec<Option<String>>>(v.as_str().unwrap_or("[]"))
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .collect()
        };
        let text = |v: &Json| v.as_str().unwrap_or_default().to_owned();
        Ok(self
            .query(&sql)?
            .iter()
            .map(|r| {
                let (lat, lon) = (r["lat"].as_f64(), r["lon"].as_f64());
                Asset {
                    uuid: text(&r["uuid"]),
                    video: r["kind"].as_i64() == Some(1),
                    created: r["created"].as_f64().unwrap_or_default(),
                    added: r["added"].as_f64().unwrap_or_default(),
                    favorite: r["favorite"].as_i64() == Some(1),
                    edit: r["edit"].as_str().map(str::to_owned),
                    title: text(&r["title"]),
                    description: text(&r["description"]),
                    keywords: list(&r["keywords"]),
                    // Photos stores "no location" as -180,-180
                    location: match (lat, lon) {
                        (Some(lat), Some(lon)) if lat > -180.0 => Some((lat, lon)),
                        _ => None,
                    },
                    hash: r["hash"].as_str().map(str::to_owned),
                    albums: list(&r["albums"]),
                    other_albums: r["other"].as_i64() == Some(1),
                }
            })
            .collect())
    }

    /// Uuids of the assets in the user album named `name_or_uuid`.
    pub fn album_members(&self, name_or_uuid: &str) -> Result<Vec<String>> {
        let j = self.joins()?;
        let key = sql_str(name_or_uuid.split('/').next().unwrap_or(name_or_uuid));
        let albums = self.query(&format!(
            "select Z_PK pk from ZGENERICALBUM
             where ZKIND = 2 and ZTRASHEDSTATE = 0 and (ZUUID = {key} or ZTITLE = {key})"
        ))?;
        let pk = match albums.as_slice() {
            [one] => one["pk"].as_i64().unwrap_or_default(),
            [] => return Err(Error::new(format!("no album {name_or_uuid:?}"))),
            _ => return Err(Error::new(format!("several albums named {name_or_uuid:?}; use its uuid"))),
        };
        Ok(self
            .query(&format!(
                "select a.ZUUID uuid from {aj} j join ZASSET a on a.Z_PK = j.{assetcol}
                 where j.{albcol} = {pk} and a.ZTRASHEDSTATE = 0",
                aj = j.album_assets,
                assetcol = j.asset_col,
                albcol = j.album_col,
            ))?
            .iter()
            .filter_map(|r| r["uuid"].as_str().map(str::to_owned))
            .collect())
    }

    /// Delete/Review albums directly inside the top-level Shoebox folder.
    pub fn shoebox_albums(&self) -> Result<Vec<ShoeboxAlbum>> {
        let j = self.joins()?;
        Ok(self
            .query(&format!(
                "select g.ZUUID uuid, g.ZTITLE title,
                    (select count(*) from {aj} j join ZASSET a on a.Z_PK = j.{assetcol}
                      where j.{albcol} = g.Z_PK and a.ZTRASHEDSTATE = 0) members
                 from ZGENERICALBUM g join ZGENERICALBUM f on f.Z_PK = g.ZPARENTFOLDER
                 where g.ZKIND = 2 and g.ZTRASHEDSTATE = 0 and f.ZTRASHEDSTATE = 0
                   and f.ZTITLE = {folder}
                   and (g.ZTITLE like {del} or g.ZTITLE like {rev})",
                aj = j.album_assets,
                assetcol = j.asset_col,
                albcol = j.album_col,
                folder = sql_str(SHOEBOX_FOLDER),
                del = sql_str(&format!("{DELETE_PREFIX}%")),
                rev = sql_str(&format!("{REVIEW_PREFIX}%")),
            ))?
            .iter()
            .map(|r| ShoeboxAlbum {
                uuid: r["uuid"].as_str().unwrap_or_default().to_owned(),
                title: r["title"].as_str().unwrap_or_default().to_owned(),
                members: r["members"].as_u64().unwrap_or_default() as usize,
            })
            .collect())
    }

    /// The small full-frame thumbnail Photos keeps for every asset (a poster frame for videos).
    pub fn thumbnail(&self, uuid: &str) -> Option<PathBuf> {
        let first = uuid.get(..1)?;
        let path = self
            .path
            .join("resources/derivatives/masters")
            .join(first)
            .join(format!("{uuid}_4_5005_c.jpeg"));
        path.exists().then_some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_tables_follow_entity_ids() {
        let ents = [
            ("Album", 34),
            ("Asset", 3),
            ("AdditionalAssetAttributes", 1),
            ("Keyword", 53),
        ]
        .map(|(n, e)| (n.to_owned(), e))
        .into();
        assert_eq!(
            joins(&ents).unwrap(),
            Joins {
                album_assets: "Z_34ASSETS".into(),
                album_col: "Z_34ALBUMS".into(),
                asset_col: "Z_3ASSETS".into(),
                keywords: "Z_1KEYWORDS".into(),
                attrs_col: "Z_1ASSETATTRIBUTES".into(),
                keyword_col: "Z_53KEYWORDS".into(),
            }
        );
        assert!(joins(&HashMap::new()).is_err());
        assert_eq!(sql_str("it's"), "'it''s'");
    }
}
