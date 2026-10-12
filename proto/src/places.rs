//! The places database (built by `scripts/build-places-db.py`): towns, regions and countries from
//! GeoNames, Wikidata "fame" (Wikipedia sitelink counts) and famous landmarks. The server reads the
//! full build; clients keep a small towns-only build (same schema) for previews and fallbacks.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::{GpsPoint, Name};

/// The server's full database.
pub const FILE_NAME: &str = "places.sqlite";
/// The clients' small towns-only database.
pub const TOWNS_FILE_NAME: &str = "towns.sqlite";
/// Folder beside the client program holding its bundled files.
pub const SUPPORT_DIR: &str = "photo-stamp-files";
/// Like Immich: the nearest populated place within this distance names the town.
const TOWN_RADIUS_KM: f64 = 25.0;
const KM_PER_DEG: f64 = 111.32;

/// The nearest town, with its region and country.
#[derive(Debug, Clone, PartialEq)]
pub struct Town {
    pub town: Name,
    pub region: Option<Name>,
    pub country: Option<Name>,
    pub distance_km: f64,
}

/// A famous landmark near a point.
#[derive(Debug, Clone, PartialEq)]
pub struct Landmark {
    pub qid: u32,
    pub name: Name,
    pub sitelinks: u32,
    pub distance_m: f64,
}

pub struct PlacesDb {
    conn: Mutex<Connection>,
}

/// Where to find `file`: the path in environment variable `env`, then beside the program
/// (`photo-stamp-files/<file>`), then `/usr/share/photo-stamp/<file>`.
pub fn locate(file: &str, env: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os(env) {
        candidates.push(p.into());
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_owned)) {
        candidates.push(dir.join(SUPPORT_DIR).join(file));
    }
    candidates.push(PathBuf::from("/usr/share/photo-stamp").join(file));
    candidates.into_iter().find(|p| p.is_file())
}

/// Cities at least this big can name the town for photos in their districts ([`PlacesDb::big_city`]).
pub const BIG_CITY_MIN_POPULATION: u64 = 40_000;

/// How far a city of this size "reaches": 6 km at 40k people, +3 km per tenfold, at most 15 km.
pub fn city_reach_km(population: u64) -> f64 {
    let ratio = population.max(BIG_CITY_MIN_POPULATION) as f64 / BIG_CITY_MIN_POPULATION as f64;
    (6.0 + 3.0 * ratio.log10()).min(15.0)
}

pub fn distance_m(a: GpsPoint, b: GpsPoint) -> f64 {
    let (la1, la2) = (a.lat.to_radians(), b.lat.to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2)
        + la1.cos() * la2.cos() * ((b.lon - a.lon).to_radians() / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

/// A lat/lon box around `at` covering `radius_m` (good enough away from the poles).
fn bbox(at: GpsPoint, radius_m: f64) -> (f64, f64, f64, f64) {
    let dlat = radius_m / 1000.0 / KM_PER_DEG;
    let dlon = dlat / at.lat.to_radians().cos().max(0.01);
    (at.lat - dlat, at.lat + dlat, at.lon - dlon, at.lon + dlon)
}

impl PlacesDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        // Fail now, not at the first photo, if this isn't our database.
        conn.query_row("SELECT count(*) FROM city LIMIT 1", [], |_| Ok(()))
            .with_context(|| format!("{} isn't a photo-stamp places database", path.display()))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// When the database was built (YYYY-MM-DD).
    pub fn built(&self) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM meta WHERE key = 'built'", [], |r| r.get(0)).ok()
    }

    /// The nearest town within 25 km, as Immich does.
    pub fn town(&self, at: GpsPoint) -> anyhow::Result<Option<Town>> {
        let conn = self.conn.lock().unwrap();
        let (lat0, lat1, lon0, lon1) = bbox(at, TOWN_RADIUS_KM * 1000.0);
        let mut stmt = conn.prepare_cached(
            "SELECT c.name, c.local, c.lat, c.lon, c.country, c.region
             FROM city_idx i JOIN city c ON c.id = i.id
             WHERE i.min_lat <= ?2 AND i.max_lat >= ?1 AND i.min_lon <= ?4 AND i.max_lon >= ?3",
        )?;
        let mut best: Option<(f64, Name, String, Option<String>)> = None;
        let rows = stmt.query_map(params![lat0, lat1, lon0, lon1], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        for row in rows {
            let (name, local, lat, lon, country, region) = row?;
            let d = distance_m(at, GpsPoint { lat, lon });
            if d <= TOWN_RADIUS_KM * 1000.0
                && best.as_ref().is_none_or(|b| d < b.0)
                && let Some(n) = Name::new(Some(&name), local.as_deref())
            {
                best = Some((d, n, country, region));
            }
        }
        let Some((d, town, country, region)) = best else { return Ok(None) };
        let region = match region.filter(|r| !r.is_empty()) {
            Some(code) => conn
                .prepare_cached("SELECT name, local FROM region WHERE country = ?1 AND code = ?2")?
                .query_row(params![country, code], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .optional()?
                .and_then(|(n, l)| Name::new(Some(&n), l.as_deref())),
            None => None,
        };
        let country = conn
            .prepare_cached("SELECT name FROM country WHERE code = ?1")?
            .query_row(params![country], |r| r.get::<_, String>(0))
            .optional()?
            // Country names are recognizable enough in English alone.
            .and_then(|n| Name::new(Some(&n), None));
        Ok(Some(Town { town, region, country, distance_km: d / 1000.0 }))
    }

    /// The biggest city (at least [`BIG_CITY_MIN_POPULATION`]) whose reach covers `at`, for naming the
    /// town when you're in one of its districts. Bigger cities reach further: ~6 km at 40k people,
    /// ~9 km at 400k, ~12 km at 4M.
    pub fn big_city(&self, at: GpsPoint) -> anyhow::Result<Option<Name>> {
        let conn = self.conn.lock().unwrap();
        let (lat0, lat1, lon0, lon1) = bbox(at, city_reach_km(u64::MAX) * 1000.0);
        let mut stmt = conn.prepare_cached(
            "SELECT c.name, c.local, c.lat, c.lon, c.population FROM city_idx i JOIN city c ON c.id = i.id
             WHERE i.min_lat <= ?2 AND i.max_lat >= ?1 AND i.min_lon <= ?4 AND i.max_lon >= ?3 AND c.population >= ?5
             ORDER BY c.population DESC",
        )?;
        let rows = stmt.query_map(params![lat0, lat1, lon0, lon1, BIG_CITY_MIN_POPULATION as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (name, local, lat, lon, population) = row?;
            if distance_m(at, GpsPoint { lat, lon }) <= city_reach_km(population.max(0) as u64) * 1000.0 {
                return Ok(Name::new(Some(&name), local.as_deref()));
            }
        }
        Ok(None)
    }

    /// English and local names for a Wikidata item, if it's one of the bundled landmarks.
    pub fn landmark_name(&self, qid: &str) -> Option<Name> {
        let id = qid.trim_start_matches(['Q', 'q']).parse::<i64>().ok()?;
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("SELECT name, local FROM landmark WHERE qid = ?1")
            .and_then(|mut s| {
                s.query_row(params![id], |r| {
                    Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .optional()
            })
            .ok()
            .flatten()
            .and_then(|(en, local)| Name::new(en.as_deref(), local.as_deref()))
    }

    /// Wikipedia sitelink count for a Wikidata item ("Q243" or "243"); 0 if it isn't notable.
    pub fn fame(&self, qid: &str) -> u32 {
        let Ok(id) = qid.trim_start_matches(['Q', 'q']).parse::<i64>() else { return 0 };
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("SELECT sitelinks FROM fame WHERE qid = ?1")
            .and_then(|mut s| s.query_row(params![id], |r| r.get::<_, u32>(0)).optional())
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    /// Famous landmarks within `radius_m`, most famous first. A landmark's reach grows with its fame
    /// (you can see and name the Eiffel Tower from further away than a local monument).
    pub fn landmarks_near(&self, at: GpsPoint, radius_m: f64) -> anyhow::Result<Vec<Landmark>> {
        let conn = self.conn.lock().unwrap();
        let (lat0, lat1, lon0, lon1) = bbox(at, radius_m);
        let mut stmt = conn.prepare_cached(
            "SELECT l.qid, l.name, l.local, l.sitelinks, l.lat, l.lon
             FROM landmark_idx i JOIN landmark l ON l.qid = i.id
             WHERE i.min_lat <= ?2 AND i.max_lat >= ?1 AND i.min_lon <= ?4 AND i.max_lon >= ?3",
        )?;
        let mut out: Vec<Landmark> = stmt
            .query_map(params![lat0, lat1, lon0, lon1], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, f64>(4)?,
                    r.get::<_, f64>(5)?,
                ))
            })?
            .filter_map(Result::ok)
            .filter_map(|(qid, en, local, sitelinks, lat, lon)| {
                let distance_m = distance_m(at, GpsPoint { lat, lon });
                let name = Name::new(en.as_deref(), local.as_deref())?;
                (distance_m <= radius_m).then_some(Landmark { qid, name, sitelinks, distance_m })
            })
            .collect();
        out.sort_by(|a, b| b.sitelinks.cmp(&a.sitelinks).then(a.distance_m.total_cmp(&b.distance_m)));
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small real-data database around the test locations (tests/fixtures/places.sqlite).
    pub fn fixture_db() -> PlacesDb {
        PlacesDb::open(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/places.sqlite"))
            .unwrap()
    }

    #[test]
    fn towns_regions_countries() {
        let db = fixture_db();
        // GeoNames' populated places (500+ people) lack Pebble Beach; the nearest is Carmel.
        // (With Overture online, its locality points supply "Pebble Beach".)
        let pebble = db.town(GpsPoint { lat: 36.565462, lon: -121.945278 }).unwrap().unwrap();
        assert!(pebble.town.preferred.starts_with("Carmel"), "{pebble:?}");
        assert_eq!(pebble.region.unwrap().preferred, "California");
        assert_eq!(pebble.country.unwrap().preferred, "United States");

        let tokyo = db.town(GpsPoint { lat: 35.6586, lon: 139.7454 }).unwrap().unwrap();
        assert!(tokyo.town.local.is_some(), "local name expected for Tokyo: {tokyo:?}");
        assert_eq!(tokyo.country.unwrap().preferred, "Japan");

        // Mid-Pacific: nothing within 25 km.
        assert_eq!(db.town(GpsPoint { lat: 0.0, lon: -150.0 }).unwrap(), None);
    }

    #[test]
    fn fame_and_landmarks() {
        let db = fixture_db();
        assert!(db.fame("Q243") > 150, "Eiffel Tower");
        assert_eq!(db.fame("Q86252392"), 0, "a bike-share dock isn't famous");
        assert_eq!(db.fame("not a qid"), 0);

        let near = db.landmarks_near(GpsPoint { lat: 48.8556, lon: 2.2986 }, 800.0).unwrap();
        assert_eq!(near.first().map(|l| l.name.preferred.as_str()), Some("Eiffel Tower"), "{near:?}");
        assert!(near[0].distance_m > 300.0 && near[0].distance_m < 800.0);
    }
}
