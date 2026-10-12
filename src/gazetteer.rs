//! Naming where a photo was taken, offline-first:
//!
//! * town, region and country — bundled GeoNames data ([`PlacesDb`]); never needs the network;
//! * famous landmarks nearby — bundled Wikidata (also gives every feature a "fame" score:
//!   how many Wikipedia language editions describe it);
//! * the venue you're inside (golf course, park, stadium, tower…), notable structures and the
//!   neighbourhood — Overture Maps, read on demand and cached on disk ([`Overture`]).
//!
//! If Overture can't be reached the result is marked incomplete (callers that can wait retry
//! later), but town/region/country and bundled landmarks are still there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::geo::{Geocoder, Name, Place};
use crate::metadata::GpsPoint;
use crate::overture::{Dataset, Feature, Overture};
use crate::places::{Landmark, PlacesDb, Town};

/// How far to look for venues and structures around the point.
const SEARCH_M: f64 = 250.0;
/// How far to look for neighbourhood and town names (Overture division points).
const DIVISION_SEARCH_M: f64 = 5000.0;
const TOWN_M: f64 = 5000.0;
/// How far a neighbourhood point can be and still name where you are.
const NEIGHBOURHOOD_M: f64 = 1500.0;
const MICROHOOD_M: f64 = 600.0;
/// Bundled landmarks: the most famous are named from further away.
const LANDMARK_MAX_REACH_M: f64 = 700.0;
const MIN_VENUE_SCORE: f64 = 4.0;
const MIN_LANDMARK_SCORE: f64 = 5.0;
/// A landmark must be at least this famous (sitelinks) to be added as "near …" to a venue.
const MIN_NEAR_SITELINKS: u32 = 40;

/// How notable a feature is from its Wikipedia coverage: 0 for none, ~6 for 50 languages, ~8 for 190.
fn fame_score(sitelinks: u32) -> f64 {
    1.5 * (1.0 + f64::from(sitelinks)).ln()
}

/// How far a landmark of this fame can be and still describe where you are.
fn landmark_reach_m(sitelinks: u32) -> f64 {
    (150.0 + 3.0 * f64::from(sitelinks)).min(LANDMARK_MAX_REACH_M)
}

/// How good an area is as "the place this photo was taken" (Overture land-use / infrastructure class).
fn venue_weight(f: &Feature) -> f64 {
    match f.category.as_deref().unwrap_or("") {
        "golf_course" | "stadium" | "theme_park" | "zoo" | "aquarium" | "castle" | "palace" => 7.0,
        "national_park"
        | "water_park"
        | "beach_resort"
        | "resort"
        | "university"
        | "aerodrome"
        | "airport"
        | "communication_tower"
        | "observation_tower"
        | "tower"
        | "lighthouse"
        | "monument" => 6.0,
        "park" | "botanical_garden" | "marina" | "college" | "museum" | "cathedral" | "bridge" | "dam" => 5.0,
        "garden" | "nature_reserve" | "protected_area" | "cemetery" | "camp_site" | "sports_centre"
        | "pier" | "place_of_worship" | "church" | "temple" | "mosque" | "shrine" | "beach" => 4.0,
        "plaza" | "recreation_ground" | "fountain" => 3.0,
        "school" | "hospital" | "pitch" | "track" | "artwork" | "playground" | "dog_park" => 2.0,
        _ => 0.0,
    }
}

/// Places (points of interest) categories worth naming a photo after when nothing better is around.
fn place_weight(category: &str) -> f64 {
    match category {
        "landmark_and_historical_building"
        | "monument"
        | "castle"
        | "palace"
        | "tourist_attraction"
        | "science_attraction"
        | "museum"
        | "zoo"
        | "aquarium"
        | "amusement_park"
        | "stadium_arena" => 6.0,
        "historic_site" | "golf_course" | "national_park" | "beach" | "viewpoint" | "lighthouse"
        | "art_museum" | "botanical_garden" | "public_plaza" => 5.0,
        "park"
        | "christian_place_of_worship"
        | "buddhist_place_of_worship"
        | "hindu_place_of_worship"
        | "muslim_place_of_worship"
        | "jewish_place_of_worship"
        | "theatre_venue"
        | "library" => 4.0,
        _ => 0.0,
    }
}

/// What the bundled data knows about Wikidata items.
pub trait Knowledge {
    /// Wikipedia sitelinks (0 if not notable).
    fn fame(&self, qid: &str) -> u32;
    /// Wikidata's English and local names, which read better than Overture's for famous places
    /// ("Champ de Mars" rather than a literal "Field of Mars").
    fn wikidata_name(&self, qid: &str) -> Option<Name>;
}

impl Knowledge for PlacesDb {
    fn fame(&self, qid: &str) -> u32 {
        PlacesDb::fame(self, qid)
    }
    fn wikidata_name(&self, qid: &str) -> Option<Name> {
        self.landmark_name(qid)
    }
}

/// Everything known about the spot, from the bundled data and (if reachable) Overture.
pub struct Surroundings<'a> {
    pub town: Option<Town>,
    /// A large city (100k+) close by: names the town when we're in one of its districts.
    pub big_city: Option<Name>,
    pub landmarks: &'a [Landmark],
    /// Overture land use + infrastructure within [`SEARCH_M`].
    pub features: &'a [Feature],
    /// Overture places (POIs) within [`SEARCH_M`].
    pub places: &'a [Feature],
    /// Overture division points (neighbourhoods, localities) within [`DIVISION_SEARCH_M`].
    pub divisions: &'a [Feature],
}

/// Combine what's around into caption lines.
pub fn build_place(s: &Surroundings, k: &dyn Knowledge) -> Option<Place> {
    let fame_of = |f: &Feature| f.wikidata.as_deref().map_or(0, |q| k.fame(q));
    let name_of = |f: &Feature| {
        f.wikidata.as_deref().and_then(|q| k.wikidata_name(q)).unwrap_or_else(|| f.name.clone())
    };

    // 1. The venue we're inside: notable type, famous, not enormous (the stadium beats the park
    //    around it; the course beats a garden in it).
    let venue = s
        .features
        .iter()
        .filter(|f| f.contains)
        .filter_map(|f| {
            let w = venue_weight(f);
            (w > 0.0).then(|| (w + fame_score(fame_of(f)) - f.bbox_km2().max(1.0).log10() * 1.5, f))
        })
        .filter(|(score, _)| *score >= MIN_VENUE_SCORE)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, f)| name_of(f));

    // 2. Famous things nearby: bundled Wikidata landmarks, and named Overture structures we're
    //    outside of. Fame matters most; distance counts against.
    let mut nearby: Vec<(f64, u32, Name)> = Vec::new();
    for l in s.landmarks {
        if l.distance_m <= landmark_reach_m(l.sitelinks) {
            nearby.push((fame_score(l.sitelinks) - l.distance_m / 250.0, l.sitelinks, l.name.clone()));
        }
    }
    for f in s.features.iter().filter(|f| !f.contains && f.distance_m <= SEARCH_M) {
        let w = venue_weight(f);
        let sitelinks = fame_of(f);
        if w > 0.0 && sitelinks > 0 {
            nearby.push((w * 0.5 + fame_score(sitelinks) - f.distance_m / 100.0, sitelinks, name_of(f)));
        }
    }
    nearby.sort_by(|a, b| b.0.total_cmp(&a.0));
    let not_the_venue = |n: &Name| venue.as_ref().is_none_or(|v| !same(v, n));
    let landmark = nearby.iter().find(|(score, _, n)| *score >= MIN_LANDMARK_SCORE && not_the_venue(n));

    // 3. Otherwise a notable point of interest right here.
    let poi = || {
        s.places
            .iter()
            .filter(|p| p.confidence.unwrap_or(0.0) >= 0.8 && p.distance_m <= 80.0)
            .filter_map(|p| {
                let w = place_weight(p.category.as_deref().unwrap_or(""));
                (w > 0.0).then(|| (w - p.distance_m / 40.0, p))
            })
            .filter(|(score, _)| *score >= MIN_VENUE_SCORE)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, p)| p.name.clone())
    };

    let nearest_division = |subtypes: &[&str], max_m: f64| {
        s.divisions
            .iter()
            .filter(|d| d.distance_m <= max_m && d.subtype.as_deref().is_some_and(|t| subtypes.contains(&t)))
            .min_by(|a, b| a.distance_m.total_cmp(&b.distance_m))
            .map(|d| d.name.clone())
    };
    // Neighbourhoods: Overture's neighbourhood/macrohood tier where it has one (Paris: Passy,
    // Vaugirard); else a close microhood (Tokyo's only tier: Shiba, Roppongi — in Paris those are squares).
    let district = nearest_division(&["neighborhood", "macrohood"], NEIGHBOURHOOD_M);
    let microhood = || nearest_division(&["microhood"], MICROHOOD_M);
    // Town: Overture knows small places GeoNames' populated-places list lacks (e.g. Pebble Beach).
    // Inside a big city Overture's "localities" are its districts (Tokyo's wards, Paris'
    // arrondissements), so the city names the town and the ward stands in for the neighbourhood.
    // GeoNames is the offline fallback.
    let locality = nearest_division(&["locality"], TOWN_M);
    let (neighbourhood, town_name) = match s.big_city.clone() {
        Some(city) => (district.or(locality).or_else(microhood), Some(city)),
        None => (district.or_else(microhood), locality.or_else(|| s.town.as_ref().map(|t| t.town.clone()))),
    };

    let (primary, near) = match (venue, landmark) {
        (Some(v), Some((_, sitelinks, l))) if *sitelinks >= MIN_NEAR_SITELINKS => (Some(v), Some(l.clone())),
        (Some(v), _) => (Some(v), None),
        (None, Some((_, _, l))) => (Some(l.clone()), None),
        (None, None) => (poi(), None),
    };

    // Context: neighbourhood, town, region, country — skipping anything already said.
    let mut context: Vec<Name> = Vec::new();
    let town = s.town.as_ref();
    for n in
        [neighbourhood, town_name, town.and_then(|t| t.region.clone()), town.and_then(|t| t.country.clone())]
            .into_iter()
            .flatten()
    {
        let said = primary.iter().chain(&near).chain(&context).any(|m| same(m, &n));
        if !said {
            context.push(n);
        }
    }
    let (primary, context) = match primary {
        Some(p) => (p, context),
        // No venue or landmark: the most specific area name leads.
        None if !context.is_empty() => {
            let first = context.remove(0);
            (first, context)
        }
        None => return None,
    };
    Some(Place {
        primary: primary.display(),
        near: near.map(|n| n.display()),
        context: (!context.is_empty())
            .then(|| context.iter().map(Name::display).collect::<Vec<_>>().join(", ")),
        complete: true,
    })
}

fn same(a: &Name, b: &Name) -> bool {
    a.preferred.eq_ignore_ascii_case(&b.preferred)
}

/// The geocoder used by photo-stamp: bundled data plus Overture on demand.
pub struct Gazetteer {
    places: PlacesDb,
    overture: Option<Arc<Overture>>,
    cache: Mutex<HashMap<String, Place>>,
}

impl Gazetteer {
    /// `overture: None` means fully offline (bundled data only).
    pub fn new(places: PlacesDb, overture: Option<Overture>) -> Self {
        Self { places, overture: overture.map(Arc::new), cache: Mutex::new(HashMap::new()) }
    }

    fn lookup_overture(
        &self,
        ov: &Overture,
        at: GpsPoint,
    ) -> anyhow::Result<(Vec<Feature>, Vec<Feature>, Vec<Feature>)> {
        let (land, infra, divisions) = std::thread::scope(|s| {
            let land = s.spawn(|| ov.features_near(Dataset::LandUse, at, SEARCH_M));
            let infra = s.spawn(|| ov.features_near(Dataset::Infrastructure, at, SEARCH_M));
            let divisions = ov.features_near(Dataset::Division, at, DIVISION_SEARCH_M);
            let join = |h: std::thread::ScopedJoinHandle<'_, anyhow::Result<Vec<Feature>>>| {
                h.join().map_err(|_| anyhow::anyhow!("Overture lookup panicked"))?
            };
            (join(land), join(infra), divisions)
        });
        let mut features = land?;
        features.extend(infra?);
        let divisions = divisions?;
        // Points of interest only matter when there's no venue: skip the download otherwise.
        let inside_venue = features.iter().any(|f| f.contains && venue_weight(f) > 0.0);
        let places = if inside_venue { Vec::new() } else { ov.features_near(Dataset::Places, at, SEARCH_M)? };
        Ok((features, places, divisions))
    }
}

impl Geocoder for Gazetteer {
    fn warm_up(&self) {
        let Some(ov) = self.overture.clone() else { return };
        std::thread::spawn(move || {
            let before = ov.downloaded_bytes();
            match ov.warm_up() {
                Ok(()) => {
                    let mb = (ov.downloaded_bytes() - before) as f64 / 1_048_576.0;
                    if mb > 0.0 {
                        log::info!("Overture: indexes ready ({mb:.0} MB downloaded)");
                    }
                }
                Err(e) => log::warn!(
                    "Overture: couldn't prepare indexes now (will retry on the first photo): {e:#}"
                ),
            }
        });
    }

    fn place(&self, at: GpsPoint) -> anyhow::Result<Option<Place>> {
        // ~11 m: photos from the same spot share a result.
        let key = format!("{:.4},{:.4}", at.lat, at.lon);
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return Ok(Some(hit.clone()));
        }
        let town = self.places.town(at)?;
        let landmarks = self.places.landmarks_near(at, LANDMARK_MAX_REACH_M)?;
        let (features, places, divisions, complete) = match &self.overture {
            None => (Vec::new(), Vec::new(), Vec::new(), true),
            Some(ov) => {
                let before = ov.downloaded_bytes();
                let result = self.lookup_overture(ov, at);
                let fetched = ov.downloaded_bytes() - before;
                if fetched > 0 {
                    log::info!("Overture: downloaded {:.1} MB for this spot", fetched as f64 / 1_048_576.0);
                }
                match result {
                    Ok((f, p, d)) => (f, p, d, true),
                    Err(e) => {
                        log::warn!("Overture lookup failed, using bundled data only: {e:#}");
                        (Vec::new(), Vec::new(), Vec::new(), false)
                    }
                }
            }
        };
        let big_city = self.places.big_city(at)?;
        let surroundings = Surroundings {
            town,
            big_city,
            landmarks: &landmarks,
            features: &features,
            places: &places,
            divisions: &divisions,
        };
        let place = build_place(&surroundings, &self.places).map(|p| Place { complete, ..p });
        if let Some(p) = place.as_ref().filter(|p| p.complete) {
            self.cache.lock().unwrap().insert(key, p.clone());
        }
        Ok(place)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Real Overture rows captured around test locations (tests/fixtures/overture.json).
    fn fixture(spot: &str) -> (GpsPoint, Vec<Feature>, Vec<Feature>, Vec<Feature>) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/overture.json");
        let all: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let s = &all[spot];
        let at = GpsPoint { lat: s["lat"].as_f64().unwrap(), lon: s["lon"].as_f64().unwrap() };
        let feature = |ds: Dataset, v: &Value| -> Option<Feature> {
            let name = Name::new(v["name_en"].as_str(), v["name"].as_str())?;
            let bbox = ["xmin", "xmax", "ymin", "ymax"].map(|k| v[k].as_f64().unwrap_or(f64::NAN));
            let point = v["lat"].as_f64().zip(v["lon"].as_f64()).map(|(lat, lon)| GpsPoint { lat, lon });
            let contains = v["contains"].as_bool().unwrap_or(false);
            let distance_m = match point {
                Some(p) => crate::places::distance_m(at, p),
                None if contains => 0.0,
                None => crate::places::distance_m(
                    at,
                    GpsPoint { lat: at.lat.clamp(bbox[2], bbox[3]), lon: at.lon.clamp(bbox[0], bbox[1]) },
                ),
            };
            let bbox =
                if bbox[0].is_nan() { point.map_or(bbox, |p| [p.lon, p.lon, p.lat, p.lat]) } else { bbox };
            Some(Feature {
                dataset: ds,
                name,
                category: v["category"].as_str().or(v["class"].as_str()).map(String::from),
                subtype: v["subtype"].as_str().map(String::from),
                wikidata: v["wikidata"].as_str().map(String::from),
                confidence: v["confidence"].as_f64(),
                bbox,
                contains,
                distance_m,
            })
        };
        let list = |key: &str, ds: Dataset| -> Vec<Feature> {
            s[key].as_array().into_iter().flatten().filter_map(|v| feature(ds, v)).collect()
        };
        let mut features = list("land_use", Dataset::LandUse);
        features.extend(list("infrastructure", Dataset::Infrastructure));
        (at, features, list("places", Dataset::Places), list("divisions", Dataset::Division))
    }

    fn caption(spot: &str) -> Vec<String> {
        let db = crate::places::tests::fixture_db();
        let (at, features, places, divisions) = fixture(spot);
        let landmarks = db.landmarks_near(at, LANDMARK_MAX_REACH_M).unwrap();
        let big_city = db.big_city(at).unwrap();
        let s = Surroundings {
            town: db.town(at).unwrap(),
            big_city,
            landmarks: &landmarks,
            features: &features,
            places: &places,
            divisions: &divisions,
        };
        build_place(&s, &db).unwrap().lines()
    }

    /// `cargo test print_captions -- --nocapture` shows every fixture's caption for review.
    #[test]
    fn print_captions() {
        for spot in [
            "pebble_beach",
            "augusta_national",
            "champ_de_mars",
            "eiffel_tower",
            "tokyo_tower",
            "hallstatt",
            "rural_iowa",
        ] {
            println!("{spot:>18}: {}", caption(spot).join(" | "));
        }
    }

    #[test]
    fn golf_courses_are_named() {
        assert_eq!(caption("pebble_beach")[0], "Pebble Beach Golf Course");
        assert_eq!(caption("augusta_national")[0], "Augusta National Golf Club");
    }

    #[test]
    fn park_with_famous_landmark_nearby() {
        let lines = caption("champ_de_mars");
        assert!(lines[0].starts_with("Field of Mars") || lines[0].starts_with("Champ de Mars"), "{lines:?}");
        assert!(lines[1].starts_with("near Eiffel Tower"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Paris")), "{lines:?}");
    }

    #[test]
    fn inside_a_famous_structure() {
        let lines = caption("tokyo_tower");
        assert!(lines[0].starts_with("Tokyo Tower"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Japan")), "{lines:?}");
    }

    #[test]
    fn small_park_in_a_small_town() {
        let lines = caption("rural_iowa");
        assert_eq!(lines[0], "Brookside Park", "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Ames") && l.contains("Iowa")), "{lines:?}");
    }

    #[test]
    fn village_without_a_venue() {
        let lines = caption("hallstatt");
        assert!(lines.iter().any(|l| l.contains("Hallstatt")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Austria")), "{lines:?}");
    }

    #[test]
    fn offline_still_names_town_and_landmarks() {
        let db = crate::places::tests::fixture_db();
        let at = GpsPoint { lat: 48.8556, lon: 2.2986 };
        let landmarks = db.landmarks_near(at, LANDMARK_MAX_REACH_M).unwrap();
        let big_city = db.big_city(at).unwrap();
        let s = Surroundings {
            town: db.town(at).unwrap(),
            big_city,
            landmarks: &landmarks,
            features: &[],
            places: &[],
            divisions: &[],
        };
        let lines = build_place(&s, &db).unwrap().lines();
        assert!(lines[0].starts_with("Eiffel Tower"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Paris")), "{lines:?}");
    }
}
