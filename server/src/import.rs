//! `photo-stamp-server import`: bring the server's data up to date (run monthly).
//!
//! 1. Mirror the newest Overture release (the datasets the gazetteer reads) into `<data>/mirror`.
//!    Downloads resume, and a running server keeps answering from what's already there.
//! 2. Build `places.sqlite` (GeoNames + Wikidata) with `build-places-db.py`, or take a ready one.
//! 3. Derive `towns.sqlite`, the small database clients download for previews and fallbacks.
//!
//! New files are written beside the old ones and moved into place at the end, so a running server
//! switches over cleanly (it checks for a new `places.sqlite` every minute).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use rusqlite::Connection;

use crate::overture::{Dataset, Overture};
use crate::service::Layout;

/// Towns at least this big go into the clients' towns database.
pub const TOWN_MIN_POPULATION: u64 = 5_000;

pub struct ImportOptions {
    /// Only mirror Overture files with rows in this box (xmin, xmax, ymin, ymax).
    pub bbox: Option<[f64; 4]>,
    /// Only mirror these datasets (all of them when empty).
    pub datasets: Vec<Dataset>,
    pub skip_overture: bool,
    /// Use this places database instead of building one.
    pub places_from: Option<PathBuf>,
    pub skip_places: bool,
    /// `build-places-db.py`.
    pub places_script: PathBuf,
}

pub fn run(layout: &Layout, opts: &ImportOptions) -> anyhow::Result<()> {
    std::fs::create_dir_all(&layout.root)?;
    if !opts.skip_overture {
        // Its own index folder, so it doesn't disturb a running server's.
        let ov = Overture::new(&layout.root.join("import"))?.with_mirror(&layout.mirror());
        let release = ov.latest_remote_release()?;
        log::info!("Overture: newest release is {release}");
        let datasets = if opts.datasets.is_empty() { Dataset::ALL.to_vec() } else { opts.datasets.clone() };
        // Only a full copy of every dataset makes a release complete (usable without the network).
        let bbox = if datasets.len() < Dataset::ALL.len() && opts.bbox.is_none() {
            Some([-180.0, 180.0, -90.0, 90.0])
        } else {
            opts.bbox
        };
        let bytes = ov.mirror_release(&release, &datasets, bbox)?;
        log::info!("Overture: mirror up to date ({:.1} GB downloaded)", bytes as f64 / 1e9);
    }

    if !opts.skip_places {
        let next = layout.root.join("places.sqlite.new");
        match &opts.places_from {
            Some(src) => {
                std::fs::copy(src, &next).with_context(|| format!("copying {}", src.display()))?;
            }
            None => {
                log::info!("building the places database (GeoNames + Wikidata; takes a while)");
                let status = Command::new("python3")
                    .arg(&opts.places_script)
                    .arg(&next)
                    .arg("--cache")
                    .arg(layout.root.join("build-cache"))
                    .status()
                    .with_context(|| format!("running {}", opts.places_script.display()))?;
                anyhow::ensure!(status.success(), "building the places database failed");
            }
        }
        // Check it opens before switching to it.
        photo_stamp_proto::places::PlacesDb::open(&next)?;
        std::fs::rename(&next, layout.places())?;
        log::info!("places database ready: {}", layout.places().display());
    }

    if layout.places().is_file() {
        let next = layout.root.join("towns.sqlite.new");
        let n = make_towns(&layout.places(), &next)?;
        std::fs::rename(&next, layout.towns())?;
        let size = std::fs::metadata(layout.towns())?.len();
        log::info!("towns database for clients: {n} towns, {:.1} MB", size as f64 / 1_048_576.0);
    }
    Ok(())
}

const TOWNS_SCHEMA: &str = "
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE country(code TEXT PRIMARY KEY, name TEXT NOT NULL, local TEXT);
CREATE TABLE region(country TEXT NOT NULL, code TEXT NOT NULL, name TEXT NOT NULL, local TEXT,
                    PRIMARY KEY(country, code)) WITHOUT ROWID;
CREATE TABLE city(id INTEGER PRIMARY KEY, name TEXT NOT NULL, local TEXT, country TEXT NOT NULL,
                  region TEXT, population INTEGER NOT NULL, lat REAL NOT NULL, lon REAL NOT NULL);
CREATE VIRTUAL TABLE city_idx USING rtree(id, min_lat, max_lat, min_lon, max_lon);
CREATE TABLE fame(qid INTEGER PRIMARY KEY, sitelinks INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE landmark(qid INTEGER PRIMARY KEY, name TEXT, local TEXT, country TEXT,
                      sitelinks INTEGER NOT NULL, lat REAL NOT NULL, lon REAL NOT NULL);
CREATE VIRTUAL TABLE landmark_idx USING rtree(id, min_lat, max_lat, min_lon, max_lon);
";

/// The clients' towns database: the same schema as `places.sqlite`, with towns of
/// [`TOWN_MIN_POPULATION`]+ people and every region and country (no landmarks). Returns the
/// number of towns.
pub fn make_towns(full: &Path, out: &Path) -> anyhow::Result<u64> {
    let _ = std::fs::remove_file(out);
    let db = Connection::open(out)?;
    db.execute_batch(TOWNS_SCHEMA)?;
    db.execute("ATTACH DATABASE ?1 AS f", [full.to_string_lossy()])?;
    db.execute_batch(&format!(
        "INSERT INTO meta SELECT * FROM f.meta;
         INSERT OR REPLACE INTO meta VALUES('kind', 'towns');
         INSERT INTO country SELECT * FROM f.country;
         INSERT INTO region SELECT * FROM f.region;
         INSERT INTO city SELECT * FROM f.city WHERE population >= {TOWN_MIN_POPULATION};
         INSERT INTO city_idx SELECT id, lat, lat, lon, lon FROM city;"
    ))?;
    db.execute("DETACH DATABASE f", [])?;
    let n: i64 = db.query_row("SELECT count(*) FROM city", [], |r| r.get(0))?;
    db.execute("VACUUM", [])?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use photo_stamp_proto::GpsPoint;
    use photo_stamp_proto::places::PlacesDb;

    #[test]
    fn towns_database_keeps_bigger_towns() {
        let dir = std::env::temp_dir().join(format!("ps-towns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let full = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/places.sqlite");
        let out = dir.join("towns.sqlite");
        let n = make_towns(&full, &out).unwrap();
        assert!(n > 0);
        let towns = PlacesDb::open(&out).unwrap();
        // Paris is big enough; its region and country come along.
        let t = towns.town(GpsPoint { lat: 48.8556, lon: 2.2986 }).unwrap().unwrap();
        assert!(t.town.preferred.starts_with("Paris"), "{t:?}");
        assert_eq!(t.country.unwrap().preferred, "France");
        assert_eq!(towns.built(), PlacesDb::open(&full).unwrap().built());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
