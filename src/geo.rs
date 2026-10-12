//! Turning GPS coordinates into human place names using OpenStreetMap services.
//!
//! Three questions, combined into up to three caption lines:
//!
//! 1. **What am I inside?** Overpass `is_in` finds named areas containing the point; the best
//!    *venue* (golf course, park, zoo, stadium, campus, castle, airport, beach…) becomes the main
//!    line: "Pebble Beach Golf Course".
//! 2. **What am I near?** Notable features within 250 m (landmarks, museums, peaks, towers…).
//!    If we're not inside a venue, the best one is the main line ("Eiffel Tower"); if we are, a
//!    clearly famous one nearby is added as "near …".
//! 3. **Which town?** Nominatim reverse geocoding gives neighbourhood, town, region and country
//!    for the context line: "Pebble Beach, California, United States".
//!
//! Names are in the preferred language with the local name in parentheses when it differs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::metadata::GpsPoint;

/// Where a photo was taken, as caption lines.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Place {
    /// Most specific name: the venue we're in, a landmark, or a neighbourhood/town.
    pub primary: String,
    /// A famous landmark close by, when `primary` is a venue we're inside.
    pub near: Option<String>,
    /// Neighbourhood, town, region, country.
    pub context: Option<String>,
    /// False when part of the lookup failed (e.g. Overpass unreachable): usable, but worth retrying.
    #[serde(skip)]
    pub complete: bool,
}

impl Place {
    pub fn lines(&self) -> Vec<String> {
        let near = self.near.as_ref().map(|n| format!("near {n}"));
        std::iter::once(self.primary.clone()).chain(near).chain(self.context.clone()).collect()
    }
}

/// Anything that can name a location. A trait so tests and `--offline` can avoid the network.
pub trait Geocoder {
    fn place(&self, at: GpsPoint) -> anyhow::Result<Option<Place>>;

    /// Prepare in the background (e.g. download indexes) so the first lookup is quick.
    fn warm_up(&self) {}
}

/// Never looks anything up.
pub struct NoGeocoder;

impl Geocoder for NoGeocoder {
    fn place(&self, _: GpsPoint) -> anyhow::Result<Option<Place>> {
        Ok(None)
    }
}

/// "48.85840° N, 2.29450° E"
pub fn format_coords(at: GpsPoint) -> String {
    let ns = if at.lat < 0.0 { 'S' } else { 'N' };
    let ew = if at.lon < 0.0 { 'W' } else { 'E' };
    format!("{:.5}° {ns}, {:.5}° {ew}", at.lat.abs(), at.lon.abs())
}

const NOMINATIM_URL: &str = "https://nominatim.openstreetmap.org/reverse";
/// Public Overpass instances, tried in turn. Override with `PHOTO_STAMP_OVERPASS_URLS` (comma-separated).
const OVERPASS_MIRRORS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
];
/// How far from the camera a landmark may be and still be "where the photo was taken".
const SEARCH_RADIUS_M: f64 = 250.0;
/// Below this score a nearby feature isn't notable enough to name the photo after.
const MIN_POI_SCORE: f64 = 4.0;
/// A nearby landmark must be at least this notable to get a "near …" line.
const MIN_NEAR_SCORE: f64 = 7.0;
/// Below this score an enclosing area isn't venue-like enough to name the photo after.
const MIN_VENUE_SCORE: f64 = 4.0;
/// OSM usage policies: at most one request per second.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(1100);

pub struct OsmGeocoder {
    agent: ureq::Agent,
    lang: String,
    overpass_urls: Vec<String>,
    /// Index of the mirror that last answered, tried first next time.
    preferred_mirror: AtomicUsize,
    cache_path: Option<PathBuf>,
    cache: Mutex<HashMap<String, Place>>,
    last_request: Mutex<Option<Instant>>,
}

impl OsmGeocoder {
    pub fn new(lang: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(45)))
            .user_agent(concat!(
                "photo-stamp/",
                env!("CARGO_PKG_VERSION"),
                " (personal photo captioning tool)"
            ))
            .build()
            .into();
        let overpass_urls = std::env::var("PHOTO_STAMP_OVERPASS_URLS")
            .ok()
            .map(|s| {
                s.split(',').map(str::trim).filter(|u| !u.is_empty()).map(str::to_owned).collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| OVERPASS_MIRRORS.iter().map(|s| s.to_string()).collect());
        let cache_path = crate::paths::data_dir().map(|d| d.join("places-v3.json"));
        let cache = cache_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            agent,
            lang: lang.to_owned(),
            overpass_urls,
            preferred_mirror: AtomicUsize::new(0),
            cache_path,
            cache: Mutex::new(cache),
            last_request: Mutex::new(None),
        }
    }

    fn cache_key(&self, at: GpsPoint) -> String {
        // 4 decimal places ≈ 11 m: photos from the same spot share a lookup.
        format!("{:.4},{:.4},{}", at.lat, at.lon, self.lang)
    }

    fn throttle(&self) {
        let mut last = self.last_request.lock().unwrap();
        if let Some(since) = last.map(|t| t.elapsed())
            && since < MIN_REQUEST_GAP
        {
            std::thread::sleep(MIN_REQUEST_GAP - since);
        }
        *last = Some(Instant::now());
    }

    /// Reverse geocode; `lang: None` returns names in the local language.
    fn nominatim(&self, at: GpsPoint, lang: Option<&str>) -> anyhow::Result<Value> {
        self.throttle();
        let mut req = self
            .agent
            .get(NOMINATIM_URL)
            .query("format", "jsonv2")
            .query("lat", at.lat.to_string())
            .query("lon", at.lon.to_string())
            .query("zoom", "16")
            .query("addressdetails", "1");
        if let Some(lang) = lang {
            req = req.query("accept-language", lang);
        }
        let body = req.call().context("Nominatim reverse geocode")?.body_mut().read_to_string()?;
        Ok(serde_json::from_str(&body)?)
    }

    /// Run an Overpass query, failing over between mirrors.
    fn overpass(&self, query: &str) -> anyhow::Result<OverpassResponse> {
        let n = self.overpass_urls.len();
        let first = self.preferred_mirror.load(Ordering::Relaxed);
        let mut errors = Vec::new();
        for i in (0..n).map(|k| (first + k) % n) {
            let url = &self.overpass_urls[i];
            self.throttle();
            let attempt = || -> anyhow::Result<OverpassResponse> {
                let body = self.agent.post(url).send_form([("data", query)])?.body_mut().read_to_string()?;
                let resp: OverpassResponse = serde_json::from_str(&body).context("not JSON")?;
                // Overloaded servers can answer 200 with a partial result and an error remark.
                if let Some(remark) = resp.remark.as_deref().filter(|r| r.contains("error")) {
                    anyhow::bail!("{remark}");
                }
                Ok(resp)
            };
            match attempt() {
                Ok(resp) => {
                    self.preferred_mirror.store(i, Ordering::Relaxed);
                    return Ok(resp);
                }
                Err(e) => errors.push(format!("{url}: {e:#}")),
            }
        }
        Err(anyhow!("all Overpass mirrors failed: {}", errors.join("; ")))
    }

    fn save_cache(&self) {
        let Some(path) = &self.cache_path else { return };
        let cache = self.cache.lock().unwrap();
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, serde_json::to_vec_pretty(&*cache)?)
        };
        if let Err(e) = write() {
            log::warn!("couldn't save place cache {}: {e}", path.display());
        }
    }
}

impl Geocoder for OsmGeocoder {
    fn place(&self, at: GpsPoint) -> anyhow::Result<Option<Place>> {
        let key = self.cache_key(at);
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return Ok(Some(Place { complete: true, ..hit.clone() }));
        }

        let preferred = self.nominatim(at, Some(&self.lang))?;
        let local = self.nominatim(at, None)?;
        let area = Area::from_nominatim(&preferred, &local);
        // A failed venue/landmark lookup shouldn't throw away a perfectly good town name,
        // but the result is marked incomplete so callers that can wait may retry.
        let (inside, around, complete) = match self.overpass(&place_query(at, SEARCH_RADIUS_M)) {
            Ok(resp) => {
                let (inside, around) = resp.split_inside_around();
                (Some(inside), Some(around), true)
            }
            Err(e) => {
                log::warn!("venue/landmark lookup failed: {e:#}");
                (None, None, false)
            }
        };

        let Some(place) = build_place(inside.as_ref(), around.as_ref(), &area, at, &self.lang) else {
            return Ok(None);
        };
        let place = Place { complete, ..place };
        if complete {
            self.cache.lock().unwrap().insert(key, place.clone());
            self.save_cache();
        }
        Ok(Some(place))
    }
}

/// A name in the preferred language plus the local-language name when it differs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Name {
    pub preferred: String,
    pub local: Option<String>,
}

impl Name {
    pub fn new(preferred: Option<&str>, local: Option<&str>) -> Option<Self> {
        let preferred = preferred.or(local)?.trim().to_owned();
        if preferred.is_empty() {
            return None;
        }
        // Skip a local name that only differs in case, spacing or punctuation ("Champ-de-Mars").
        let squash = |s: &str| {
            s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>()
        };
        let local = local
            .map(str::trim)
            .filter(|l| !l.is_empty() && !squash(&preferred).contains(&squash(l)))
            .map(str::to_owned);
        Some(Self { preferred, local })
    }

    fn from_tags(tags: &HashMap<String, String>, lang: &str) -> Option<Self> {
        Name::new(tags.get(&format!("name:{lang}")).map(String::as_str), tags.get("name").map(String::as_str))
    }

    /// "Munich (München)", or just "Paris".
    pub fn display(&self) -> String {
        match &self.local {
            Some(l) => format!("{} ({l})", self.preferred),
            None => self.preferred.clone(),
        }
    }

    fn same_as(&self, other: &Name) -> bool {
        self.preferred.eq_ignore_ascii_case(&other.preferred)
    }
}

/// Neighbourhood / settlement / region names pulled out of Nominatim reverse responses.
#[derive(Debug, Default, PartialEq)]
pub struct Area {
    pub district: Option<Name>,
    /// Hamlet or village: often the name people know a rural spot by ("Pebble Beach").
    pub settlement: Option<Name>,
    pub locality: Option<Name>,
    pub region: Option<Name>,
    pub country: Option<Name>,
}

impl Area {
    /// `preferred` and `local` are the same lookup made in the preferred and the local language.
    pub fn from_nominatim(preferred: &Value, local: &Value) -> Area {
        // Take the first key present in either response, so both names describe the same thing.
        let pick = |keys: &[&str]| {
            keys.iter().find_map(|k| {
                let p = preferred["address"][*k].as_str();
                let l = local["address"][*k].as_str();
                Name::new(p, l)
            })
        };
        // Region and country are recognisable enough in the preferred language alone.
        let preferred_only = |n: Option<Name>| n.map(|n| Name { local: None, ..n });
        Area {
            district: pick(&["neighbourhood", "quarter", "suburb", "city_district"]),
            settlement: pick(&["hamlet", "village"]),
            locality: pick(&["city", "town", "municipality"]),
            region: preferred_only(pick(&["state", "region", "province", "county"])),
            country: preferred_only(pick(&["country"])),
        }
    }

    /// Most specific name of the area itself, used as the main line when no venue/landmark is found.
    fn most_specific(&self) -> Option<&Name> {
        [&self.district, &self.settlement, &self.locality, &self.region, &self.country]
            .into_iter()
            .find_map(Option::as_ref)
    }

    /// "Gros-Caillou, Paris, Île-de-France, France", leaving out anything already said.
    fn context(&self, already: &[&Name]) -> Option<String> {
        let mut parts: Vec<&Name> = Vec::new();
        for n in [&self.district, &self.settlement, &self.locality, &self.region, &self.country]
            .into_iter()
            .flatten()
        {
            if !already.iter().chain(&parts).any(|seen| seen.same_as(n)) {
                parts.push(n);
            }
        }
        (!parts.is_empty()).then(|| parts.iter().map(|n| n.display()).collect::<Vec<_>>().join(", "))
    }
}

/// Combine what we're inside, what's nearby and the address into caption lines.
pub fn build_place(
    inside: Option<&OverpassResponse>,
    around: Option<&OverpassResponse>,
    area: &Area,
    at: GpsPoint,
    lang: &str,
) -> Option<Place> {
    let venue = inside.and_then(|r| best_venue(r, lang));
    let landmark = around.and_then(|r| best_landmark(r, at, lang, venue.is_some(), inside));

    let (primary, near) = match (venue, landmark) {
        (Some(v), landmark) => (v, landmark),
        (None, Some(l)) => (l, None),
        (None, None) => (area.most_specific()?.clone(), None),
    };
    let context = area.context(&[&primary]);
    Some(Place { primary: primary.display(), near: near.map(|n| n.display()), context, complete: true })
}

/// One Overpass request answering both questions (halving load on the public servers):
/// first the named areas containing the point, output with bounding boxes (`out tags bb`), then
/// notable features nearby, output with centres (`out tags center`). The output modes are what
/// tell the two result sets apart; see [`OverpassResponse::split_inside_around`].
fn place_query(at: GpsPoint, radius: f64) -> String {
    let around = format!("(around:{radius},{},{})", at.lat, at.lon);
    let filters = [
        r#"["tourism"~"^(attraction|museum|viewpoint|theme_park|zoo|aquarium|gallery|artwork)$"]"#,
        r#"["historic"]"#,
        r#"["leisure"~"^(park|garden|stadium|nature_reserve|marina|golf_course)$"]"#,
        r#"["natural"~"^(peak|volcano|beach|bay|waterfall|glacier|cape|arch)$"]"#,
        r#"["amenity"~"^(place_of_worship|university|theatre|townhall|arts_centre)$"]"#,
        r#"["man_made"~"^(tower|lighthouse|bridge|pier)$"]"#,
        r#"["building"~"^(cathedral|castle|palace)$"]"#,
    ];
    let mut q = format!(
        "[out:json][timeout:25];\n\
         is_in({},{})->.a;\n\
         (way(pivot.a);relation(pivot.a);)->.c;\n\
         nwr.c[\"name\"];\n\
         out tags bb;\n(\n",
        at.lat, at.lon
    );
    for f in filters {
        q.push_str(&format!("  nwr{around}[\"name\"]{f};\n"));
    }
    q.push_str(");\nout tags center;");
    q
}

#[derive(Debug, Default, Deserialize)]
pub struct OverpassResponse {
    #[serde(default)]
    pub elements: Vec<OverpassElement>,
    pub remark: Option<String>,
}

impl OverpassResponse {
    /// Separate a [`place_query`] answer: enclosing areas carry `bounds`, nearby features don't.
    pub fn split_inside_around(self) -> (OverpassResponse, OverpassResponse) {
        let (inside, around) = self.elements.into_iter().partition(|e| e.bounds.is_some());
        (
            OverpassResponse { elements: inside, remark: None },
            OverpassResponse { elements: around, remark: None },
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct OverpassElement {
    lat: Option<f64>,
    lon: Option<f64>,
    center: Option<LatLon>,
    bounds: Option<Bounds>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct LatLon {
    lat: f64,
    lon: f64,
}

#[derive(Debug, Deserialize)]
struct Bounds {
    minlat: f64,
    minlon: f64,
    maxlat: f64,
    maxlon: f64,
}

impl Bounds {
    fn area_km2(&self) -> f64 {
        let h = (self.maxlat - self.minlat) * 111.0;
        let w = (self.maxlon - self.minlon) * 111.0 * ((self.minlat + self.maxlat) / 2.0).to_radians().cos();
        (h * w).abs()
    }
}

/// How good an enclosing area is as "the place this photo was taken": venues you visit.
/// Administrative, census, timezone and similar boundaries score 0.
fn venue_weight(tags: &HashMap<String, String>) -> f64 {
    let t = |k: &str| tags.get(k).map(String::as_str);
    let weights = [
        match t("leisure") {
            Some("golf_course" | "stadium") => 7.0,
            Some("water_park" | "resort" | "beach_resort") => 6.0,
            Some("park" | "marina") => 5.0,
            Some("garden" | "nature_reserve" | "sports_centre" | "ice_rink") => 4.0,
            _ => 0.0,
        },
        match t("tourism") {
            Some("theme_park" | "zoo" | "aquarium" | "museum") => 7.0,
            Some("attraction" | "resort" | "gallery") => 6.0,
            Some("camp_site") => 4.0,
            _ => 0.0,
        },
        match t("historic") {
            Some("castle" | "palace") => 7.0,
            Some("fort" | "archaeological_site" | "monument") => 6.0,
            Some("ruins" | "manor") => 5.0,
            _ => 0.0,
        },
        match t("amenity") {
            Some("university" | "theatre") => 6.0,
            Some("college" | "place_of_worship" | "arts_centre") => 5.0,
            Some("marketplace") => 4.0,
            Some("hospital" | "school") => 3.0,
            _ => 0.0,
        },
        match t("building") {
            Some("cathedral" | "castle" | "palace") => 7.0,
            Some("church" | "temple" | "mosque" | "synagogue") => 5.0,
            _ => 0.0,
        },
        match t("boundary") {
            Some("national_park") => 6.0,
            Some("protected_area") => 4.0,
            _ => 0.0,
        },
        match t("aeroway") {
            Some("aerodrome") => 6.0,
            _ => 0.0,
        },
        match t("natural") {
            Some("beach") => 6.0,
            _ => 0.0,
        },
        match t("landuse") {
            Some("winter_sports") => 6.0,
            Some("cemetery") => 4.0,
            _ => 0.0,
        },
    ];
    weights.into_iter().fold(0.0, f64::max)
}

fn notability(tags: &HashMap<String, String>) -> f64 {
    let mut s = 0.0;
    if tags.contains_key("wikidata") {
        s += 3.0;
    }
    if tags.contains_key("wikipedia") {
        s += 2.0;
    }
    s
}

/// The enclosing area that best names where we are. Notable venue types win; very large areas
/// are penalised so the stadium beats the national park it sits in, and the course beats a
/// small unremarkable garden inside it because the garden scores lower on type and notability.
fn best_venue(resp: &OverpassResponse, lang: &str) -> Option<Name> {
    resp.elements
        .iter()
        .filter_map(|el| {
            let weight = venue_weight(&el.tags);
            if weight == 0.0 {
                return None;
            }
            let km2 = el.bounds.as_ref().map_or(1.0, Bounds::area_km2);
            let size_penalty = km2.max(1.0).log10() * 1.5;
            Some((weight + notability(&el.tags) - size_penalty, km2, el))
        })
        .filter(|(score, _, _)| *score >= MIN_VENUE_SCORE)
        // Highest score; on a tie the smaller area.
        .max_by(|a, b| a.0.total_cmp(&b.0).then(b.1.total_cmp(&a.1)))
        .and_then(|(_, _, el)| Name::from_tags(&el.tags, lang))
}

/// How inherently "photo-worthy" a nearby feature's category is.
fn landmark_weight(tags: &HashMap<String, String>) -> f64 {
    let t = |k: &str| tags.get(k).map(String::as_str);
    let weights = [
        match t("tourism") {
            Some("attraction" | "theme_park" | "zoo") => 6.0,
            Some("museum" | "aquarium") => 5.0,
            Some("viewpoint" | "gallery") => 4.0,
            Some("artwork") => 2.0,
            _ => 0.0,
        },
        match t("historic") {
            Some("castle" | "monument" | "palace" | "fort" | "archaeological_site" | "ruins") => 6.0,
            Some("memorial" | "building" | "church" | "city_gate") => 4.0,
            Some(_) => 2.5,
            None => 0.0,
        },
        match t("natural") {
            Some("peak" | "volcano" | "waterfall" | "glacier" | "arch") => 6.0,
            Some("beach" | "bay" | "cape") => 4.5,
            _ => 0.0,
        },
        match t("leisure") {
            Some("golf_course" | "stadium") => 5.0,
            Some("park" | "garden" | "nature_reserve") => 4.0,
            Some("marina") => 2.5,
            _ => 0.0,
        },
        match t("building") {
            Some("cathedral" | "castle" | "palace") => 5.0,
            _ => 0.0,
        },
        match t("man_made") {
            Some("tower" | "lighthouse" | "bridge") => 4.0,
            Some("pier") => 3.0,
            _ => 0.0,
        },
        match t("amenity") {
            Some("place_of_worship") => 2.5,
            Some("university" | "theatre" | "townhall" | "arts_centre") => 3.0,
            _ => 0.0,
        },
    ];
    weights.into_iter().fold(0.0, f64::max)
}

fn distance_m(a: GpsPoint, b: GpsPoint) -> f64 {
    let (la1, la2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = la2 - la1;
    let dlon = (b.lon - a.lon).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

/// Score a nearby feature: notable categories and features with Wikipedia/Wikidata entries win,
/// with a distance penalty so a famous landmark 200 m away beats an obscure one at 20 m.
fn landmark_score(el: &OverpassElement, at: GpsPoint) -> f64 {
    let pos = el
        .lat
        .zip(el.lon)
        .or(el.center.as_ref().map(|c| (c.lat, c.lon)))
        .map(|(lat, lon)| GpsPoint { lat, lon });
    let dist = pos.map_or(SEARCH_RADIUS_M / 2.0, |p| distance_m(at, p));
    landmark_weight(&el.tags) + notability(&el.tags) - dist / 75.0
}

/// Best nearby landmark. With `as_extra` (we're already inside a venue) it must be famous enough
/// for a "near …" line, and must not be one of the areas we're inside.
fn best_landmark(
    resp: &OverpassResponse,
    at: GpsPoint,
    lang: &str,
    as_extra: bool,
    inside: Option<&OverpassResponse>,
) -> Option<Name> {
    let enclosing: Vec<&str> = inside
        .map(|r| r.elements.iter().filter_map(|e| e.tags.get("name").map(String::as_str)).collect())
        .unwrap_or_default();
    let min = if as_extra { MIN_NEAR_SCORE } else { MIN_POI_SCORE };
    resp.elements
        .iter()
        .filter(|el| !el.tags.get("name").is_some_and(|n| enclosing.contains(&n.as_str())))
        .map(|el| (landmark_score(el, at), el))
        .filter(|(s, _)| *s >= min)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .and_then(|(_, el)| Name::from_tags(&el.tags, lang))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EIFFEL: GpsPoint = GpsPoint { lat: 48.8584, lon: 2.2945 };

    fn overpass(v: Value) -> OverpassResponse {
        serde_json::from_value(v).unwrap()
    }

    fn name(p: &str, l: &str) -> Name {
        Name::new(Some(p), Some(l)).unwrap()
    }

    /// Real OSM responses captured for a point, see tests/fixtures.
    fn fixture(file: &str) -> (OverpassResponse, OverpassResponse, Area) {
        let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        (
            serde_json::from_value(v["inside"].clone()).unwrap(),
            serde_json::from_value(v["around"].clone()).unwrap(),
            Area::from_nominatim(&v["nominatim_en"], &v["nominatim_local"]),
        )
    }

    #[test]
    fn golf_course_photo_names_the_course() {
        // A photo taken on the course at Pebble Beach (real OSM data).
        let at = GpsPoint { lat: 36.565462, lon: -121.945278 };
        let (inside, around, area) = fixture("pebble_beach.json");
        let place = build_place(Some(&inside), Some(&around), &area, at, "en").unwrap();
        assert_eq!(
            place.lines(),
            ["Pebble Beach Golf Course", "Pebble Beach, California, United States"],
            "{place:?}"
        );
    }

    #[test]
    fn venue_beats_small_garden_inside_it_and_huge_park_around_it() {
        let inside = overpass(json!({"elements": [
            {"type": "way", "bounds": {"minlat": 33.4990, "minlon": -82.0300, "maxlat": 33.5090, "maxlon": -82.0150},
             "tags": {"name": "Augusta National Golf Club", "leisure": "golf_course", "wikidata": "Q756346", "wikipedia": "en:Augusta National Golf Club"}},
            {"type": "way", "bounds": {"minlat": 33.5026, "minlon": -82.0220, "maxlat": 33.5028, "maxlon": -82.0218},
             "tags": {"name": "The Terrace", "leisure": "garden"}},
            {"type": "relation", "bounds": {"minlat": 33.0, "minlon": -83.0, "maxlat": 34.0, "maxlon": -81.0},
             "tags": {"name": "Big Regional Protected Area", "boundary": "protected_area", "wikidata": "Q1"}},
            {"type": "relation", "tags": {"name": "Richmond County", "boundary": "administrative", "admin_level": "6", "wikidata": "Q2"}},
        ]}));
        assert_eq!(best_venue(&inside, "en").unwrap().preferred, "Augusta National Golf Club");
    }

    #[test]
    fn stadium_beats_the_park_it_is_in() {
        let inside = overpass(json!({"elements": [
            {"type": "way", "bounds": {"minlat": 0.0, "minlon": 0.0, "maxlat": 0.03, "maxlon": 0.03},
             "tags": {"name": "Big Park", "leisure": "park", "wikidata": "Q3"}},
            {"type": "way", "bounds": {"minlat": 0.01, "minlon": 0.01, "maxlat": 0.012, "maxlon": 0.012},
             "tags": {"name": "Arena", "leisure": "stadium", "wikidata": "Q4"}},
        ]}));
        assert_eq!(best_venue(&inside, "en").unwrap().preferred, "Arena");
    }

    #[test]
    fn landmark_when_not_inside_a_venue() {
        let around = overpass(json!({"elements": [
            {"type": "node", "lat": 48.8585, "lon": 2.2946,
             "tags": {"name": "Plaque", "historic": "memorial"}},
            {"type": "way", "center": {"lat": 48.8583, "lon": 2.2944},
             "tags": {"name": "Tour Eiffel", "name:en": "Eiffel Tower", "tourism": "attraction",
                      "man_made": "tower", "wikidata": "Q243", "wikipedia": "fr:Tour Eiffel"}},
        ]}));
        let paris = json!({"address": {"suburb": "Gros-Caillou", "city": "Paris", "state": "Île-de-France", "country": "France"}});
        let area = Area::from_nominatim(&paris, &paris);
        let place =
            build_place(Some(&OverpassResponse::default()), Some(&around), &area, EIFFEL, "en").unwrap();
        assert_eq!(
            place.lines(),
            ["Eiffel Tower (Tour Eiffel)", "Gros-Caillou, Paris, Île-de-France, France"]
        );
    }

    #[test]
    fn famous_landmark_nearby_is_added_to_a_venue() {
        let inside = overpass(json!({"elements": [
            {"type": "way", "bounds": {"minlat": 48.853, "minlon": 2.290, "maxlat": 48.860, "maxlon": 2.302},
             "tags": {"name": "Champ de Mars", "leisure": "park", "wikidata": "Q1"}},
        ]}));
        let around = overpass(json!({"elements": [
            {"type": "way", "center": {"lat": 48.8566, "lon": 2.2950},
             "tags": {"name": "Champ de Mars", "leisure": "park", "wikidata": "Q1"}},
            {"type": "way", "center": {"lat": 48.8578, "lon": 2.2950},
             "tags": {"name": "Tour Eiffel", "name:en": "Eiffel Tower", "tourism": "attraction",
                      "wikidata": "Q243", "wikipedia": "fr:Tour Eiffel"}},
            {"type": "node", "lat": 48.8566, "lon": 2.2951, "tags": {"name": "Bench Plaque", "historic": "memorial"}},
        ]}));
        let area =
            Area::from_nominatim(&json!({"address": {"city": "Paris", "country": "France"}}), &json!({}));
        let at = GpsPoint { lat: 48.8566, lon: 2.2950 };
        let place = build_place(Some(&inside), Some(&around), &area, at, "en").unwrap();
        assert_eq!(place.lines(), ["Champ de Mars", "near Eiffel Tower (Tour Eiffel)", "Paris, France"]);
    }

    #[test]
    fn unremarkable_features_are_not_used() {
        let around = overpass(json!({"elements": [
            {"type": "node", "lat": 48.8600, "lon": 2.2970,
             "tags": {"name": "Some Sculpture", "tourism": "artwork"}},
        ]}));
        assert_eq!(best_landmark(&around, EIFFEL, "en", false, None), None);
        assert_eq!(best_venue(&OverpassResponse::default(), "en"), None);
    }

    #[test]
    fn bilingual_names() {
        assert_eq!(name("Munich", "München").display(), "Munich (München)");
        assert_eq!(name("Paris", "Paris").display(), "Paris");
        assert_eq!(name("Tokyo Tower", "東京タワー").display(), "Tokyo Tower (東京タワー)");
        // No English form: the local name is used once, not repeated in parentheses.
        assert_eq!(Name::new(None, Some("Gros-Caillou")).unwrap().display(), "Gros-Caillou");
        assert_eq!(Name::new(None, None), None);
    }

    #[test]
    fn area_only_places() {
        let en =
            json!({"address": {"suburb": "Minato", "city": "Tokyo", "state": "Tokyo", "country": "Japan"}});
        let local =
            json!({"address": {"suburb": "港区", "city": "東京都", "state": "東京都", "country": "日本"}});
        let area = Area::from_nominatim(&en, &local);
        let place = build_place(None, None, &area, EIFFEL, "en").unwrap();
        assert_eq!(place.lines(), ["Minato (港区)", "Tokyo (東京都), Japan"]);

        let village =
            json!({"address": {"village": "Hallstatt", "state": "Upper Austria", "country": "Austria"}});
        let place = build_place(None, None, &Area::from_nominatim(&village, &village), EIFFEL, "en").unwrap();
        assert_eq!(place.lines(), ["Hallstatt", "Upper Austria, Austria"]);

        let sea = json!({"error": "Unable to geocode"});
        assert_eq!(build_place(None, None, &Area::from_nominatim(&sea, &sea), EIFFEL, "en"), None);
    }

    #[test]
    fn coords_format() {
        assert_eq!(format_coords(GpsPoint { lat: -33.85, lon: -70.0 }), "33.85000° S, 70.00000° W");
    }

    #[test]
    fn queries_mention_location() {
        let q = place_query(EIFFEL, 250.0);
        assert!(q.contains("is_in(48.8584,2.2945)"));
        assert!(q.contains("around:250,48.8584,2.2945"));
        assert!(q.contains("out tags bb;") && q.ends_with("out tags center;"));
    }

    #[test]
    fn combined_response_splits_by_output_mode() {
        let resp = overpass(json!({"elements": [
            {"type": "way", "bounds": {"minlat": 0.0, "minlon": 0.0, "maxlat": 1.0, "maxlon": 1.0},
             "tags": {"name": "Course", "leisure": "golf_course"}},
            {"type": "node", "lat": 0.5, "lon": 0.5, "tags": {"name": "Statue", "historic": "monument"}},
            {"type": "way", "center": {"lat": 0.5, "lon": 0.5}, "tags": {"name": "Course", "leisure": "golf_course"}},
        ]}));
        let (inside, around) = resp.split_inside_around();
        assert_eq!(inside.elements.len(), 1);
        assert_eq!(around.elements.len(), 2);
    }

    #[test]
    fn no_russian_mirrors() {
        assert!(OVERPASS_MIRRORS.iter().all(|m| !m.contains(".ru/")), "{OVERPASS_MIRRORS:?}");
    }

    #[test]
    fn error_remarks_are_detected() {
        let r: OverpassResponse =
            serde_json::from_str(r#"{"elements":[],"remark":"runtime error: Query timed out"}"#).unwrap();
        assert!(r.remark.unwrap().contains("error"));
    }
}
