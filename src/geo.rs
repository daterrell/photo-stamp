//! Turning GPS coordinates into a human place name using OpenStreetMap services.
//!
//! Strategy: ask Overpass for notable named features (landmarks, museums, parks, peaks…)
//! close to the point and pick the best by category, notability and distance. Separately
//! reverse-geocode with Nominatim for the neighbourhood/town. Combine the two into a label
//! like "Eiffel Tower (Tour Eiffel), Paris", or "Minato (港区), Tokyo (東京都)" when no
//! landmark is near: names in the preferred language, local names in parentheses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;

use crate::metadata::GpsPoint;

/// Anything that can name a location. A trait so tests and `--offline` can avoid the network.
pub trait Geocoder {
    fn place_name(&self, at: GpsPoint) -> anyhow::Result<Option<String>>;
}

/// Never looks anything up.
pub struct NoGeocoder;

impl Geocoder for NoGeocoder {
    fn place_name(&self, _: GpsPoint) -> anyhow::Result<Option<String>> {
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
const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
/// How far from the camera a landmark may be and still be "where the photo was taken".
const SEARCH_RADIUS_M: f64 = 250.0;
/// Below this score a nearby feature isn't notable enough to name the photo after.
const MIN_POI_SCORE: f64 = 4.0;
/// Nominatim's usage policy: at most one request per second.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(1100);

pub struct OsmGeocoder {
    agent: ureq::Agent,
    lang: String,
    cache_path: Option<PathBuf>,
    cache: Mutex<HashMap<String, String>>,
    last_request: Mutex<Option<Instant>>,
}

impl OsmGeocoder {
    pub fn new(lang: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(concat!(
                "photo-stamp/",
                env!("CARGO_PKG_VERSION"),
                " (personal photo captioning tool)"
            ))
            .build()
            .into();
        let cache_path = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .map(|d| d.join("photo-stamp").join("places-v2.json"));
        let cache = cache_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            agent,
            lang: lang.to_owned(),
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

    fn overpass(&self, at: GpsPoint) -> anyhow::Result<OverpassResponse> {
        self.throttle();
        let body = self
            .agent
            .post(OVERPASS_URL)
            .send_form([("data", overpass_query(at, SEARCH_RADIUS_M).as_str())])
            .context("Overpass POI query")?
            .body_mut()
            .read_to_string()?;
        Ok(serde_json::from_str(&body)?)
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
            eprintln!("warning: couldn't save place cache {}: {e}", path.display());
        }
    }
}

impl Geocoder for OsmGeocoder {
    fn place_name(&self, at: GpsPoint) -> anyhow::Result<Option<String>> {
        let key = self.cache_key(at);
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return Ok(Some(hit.clone()));
        }

        let preferred = self.nominatim(at, Some(&self.lang))?;
        let local = self.nominatim(at, None)?;
        let area = Area::from_nominatim(&preferred, &local);
        // A failed POI lookup shouldn't throw away a perfectly good town name.
        let (poi, complete) = match self.overpass(at) {
            Ok(resp) => (best_poi(&resp, at, &self.lang), true),
            Err(e) => {
                eprintln!("warning: landmark lookup failed, using area name only: {e:#}");
                (None, false)
            }
        };
        let Some(label) = combine_label(poi.as_ref(), &area) else { return Ok(None) };

        // Only remember complete answers: after an Overpass outage the next photo here should
        // get another chance at the landmark name.
        if complete {
            self.cache.lock().unwrap().insert(key, label.clone());
            self.save_cache();
        }
        Ok(Some(label))
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
        let local =
            local.map(str::trim).filter(|l| !l.is_empty() && !preferred.contains(l)).map(str::to_owned);
        Some(Self { preferred, local })
    }

    /// "Munich (München)", or just "Paris".
    pub fn display(&self) -> String {
        match &self.local {
            Some(l) => format!("{} ({l})", self.preferred),
            None => self.preferred.clone(),
        }
    }
}

/// Neighbourhood / town / region names pulled out of Nominatim reverse responses.
#[derive(Debug, Default, PartialEq)]
pub struct Area {
    pub district: Option<Name>,
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
        Area {
            district: pick(&["neighbourhood", "quarter", "suburb", "city_district"]),
            locality: pick(&["city", "town", "village", "hamlet", "municipality"]),
            region: pick(&["county", "state", "region"]),
            // Country names are recognisable enough in the preferred language alone.
            country: pick(&["country"]).map(|c| Name { local: None, ..c }),
        }
    }
}

/// Build the one-line place label from the best landmark (if any) and the surrounding area.
pub fn combine_label(poi: Option<&Name>, area: &Area) -> Option<String> {
    let mut parts: Vec<&Name> = Vec::new();
    if let Some(poi) = poi {
        parts.push(poi);
        if let Some(loc) = area.locality.as_ref().or(area.region.as_ref()) {
            parts.push(loc);
        }
    } else {
        if let Some(d) = &area.district {
            parts.push(d);
        }
        if let Some(place) = area.locality.as_ref().or(area.region.as_ref()).or(area.country.as_ref()) {
            parts.push(place);
        }
        // A lone town reads better with its country: "Hallstatt, Austria".
        if parts.len() == 1
            && let Some(c) = area.country.as_ref().filter(|c| c.preferred != parts[0].preferred)
        {
            parts.push(c);
        }
    }
    // "Paris, Paris" or "Musée de Paris, Paris" → drop the redundant town.
    if parts.len() == 2 && parts[0].preferred.contains(&parts[1].preferred) {
        parts.pop();
    }
    (!parts.is_empty()).then(|| parts.iter().map(|n| n.display()).collect::<Vec<_>>().join(", "))
}

fn overpass_query(at: GpsPoint, radius: f64) -> String {
    let around = format!("(around:{radius},{},{})", at.lat, at.lon);
    let filters = [
        r#"["tourism"~"^(attraction|museum|viewpoint|theme_park|zoo|aquarium|gallery|artwork)$"]"#,
        r#"["historic"]"#,
        r#"["leisure"~"^(park|garden|stadium|nature_reserve|marina)$"]"#,
        r#"["natural"~"^(peak|volcano|beach|bay|waterfall|glacier|cape|arch)$"]"#,
        r#"["amenity"~"^(place_of_worship|university|theatre|townhall|arts_centre)$"]"#,
        r#"["man_made"~"^(tower|lighthouse|bridge|pier)$"]"#,
        r#"["building"~"^(cathedral|castle|palace)$"]"#,
    ];
    let mut q = String::from("[out:json][timeout:25];(\n");
    for f in filters {
        q.push_str(&format!("  nwr{around}[\"name\"]{f};\n"));
    }
    q.push_str(");\nout tags center;");
    q
}

#[derive(Debug, Deserialize)]
pub struct OverpassResponse {
    #[serde(default)]
    pub elements: Vec<OverpassElement>,
}

#[derive(Debug, Deserialize)]
pub struct OverpassElement {
    lat: Option<f64>,
    lon: Option<f64>,
    center: Option<LatLon>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct LatLon {
    lat: f64,
    lon: f64,
}

/// How inherently "photo-worthy" a feature's category is.
fn category_weight(tags: &HashMap<String, String>) -> f64 {
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
            Some(_) => 4.5,
            None => 0.0,
        },
        match t("leisure") {
            Some("park" | "garden" | "nature_reserve" | "stadium") => 4.0,
            Some(_) => 2.5,
            None => 0.0,
        },
        match t("building") {
            Some("cathedral" | "castle" | "palace") => 5.0,
            _ => 0.0,
        },
        match t("man_made") {
            Some("tower" | "lighthouse" | "bridge") => 4.0,
            Some(_) => 3.0,
            None => 0.0,
        },
        match t("amenity") {
            Some("place_of_worship") => 2.5,
            Some(_) => 3.0,
            None => 0.0,
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

/// Score a feature: notable categories and features with Wikipedia/Wikidata entries win,
/// with a gentle distance penalty so a famous landmark 200 m away beats an obscure one at 20 m.
fn score(el: &OverpassElement, at: GpsPoint) -> f64 {
    let mut s = category_weight(&el.tags);
    if el.tags.contains_key("wikidata") {
        s += 3.0;
    }
    if el.tags.contains_key("wikipedia") {
        s += 2.0;
    }
    let pos = el
        .lat
        .zip(el.lon)
        .or(el.center.as_ref().map(|c| (c.lat, c.lon)))
        .map(|(lat, lon)| GpsPoint { lat, lon });
    let dist = pos.map_or(SEARCH_RADIUS_M / 2.0, |p| distance_m(at, p));
    s - dist / 75.0
}

pub fn best_poi(resp: &OverpassResponse, at: GpsPoint, lang: &str) -> Option<Name> {
    let lang_key = format!("name:{lang}");
    resp.elements
        .iter()
        .map(|el| (score(el, at), el))
        .filter(|(s, _)| *s >= MIN_POI_SCORE)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .and_then(|(_, el)| {
            Name::new(el.tags.get(&lang_key).map(String::as_str), el.tags.get("name").map(String::as_str))
        })
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

    #[test]
    fn famous_landmark_beats_nearer_obscure_feature() {
        let resp = overpass(json!({"elements": [
            {"type": "node", "lat": 48.8585, "lon": 2.2946,
             "tags": {"name": "Plaque", "historic": "memorial"}},
            {"type": "way", "center": {"lat": 48.8583, "lon": 2.2944},
             "tags": {"name": "Tour Eiffel", "name:en": "Eiffel Tower", "tourism": "attraction",
                      "man_made": "tower", "wikidata": "Q243", "wikipedia": "fr:Tour Eiffel"}},
        ]}));
        assert_eq!(best_poi(&resp, EIFFEL, "en"), Some(name("Eiffel Tower", "Tour Eiffel")));
        assert_eq!(best_poi(&resp, EIFFEL, "de").unwrap().display(), "Tour Eiffel");
    }

    #[test]
    fn unremarkable_features_are_not_used() {
        let resp = overpass(json!({"elements": [
            {"type": "node", "lat": 48.8600, "lon": 2.2970,
             "tags": {"name": "Some Sculpture", "tourism": "artwork"}},
        ]}));
        assert_eq!(best_poi(&resp, EIFFEL, "en"), None);
        assert_eq!(best_poi(&overpass(json!({})), EIFFEL, "en"), None);
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
    fn labels() {
        let en = json!({"address": {"suburb": "Minato", "city": "Tokyo", "country": "Japan"}});
        let local = json!({"address": {"suburb": "港区", "city": "東京都", "country": "日本"}});
        let area = Area::from_nominatim(&en, &local);
        assert_eq!(combine_label(None, &area).as_deref(), Some("Minato (港区), Tokyo (東京都)"));
        let tower = name("Tokyo Tower", "東京タワー");
        // The landmark's name already says where it is.
        assert_eq!(combine_label(Some(&tower), &area).as_deref(), Some("Tokyo Tower (東京タワー)"));
        let temple = name("Senso-ji", "浅草寺");
        assert_eq!(combine_label(Some(&temple), &area).as_deref(), Some("Senso-ji (浅草寺), Tokyo (東京都)"));

        let paris = json!({"address": {"suburb": "Gros-Caillou", "city": "Paris", "country": "France"}});
        let area = Area::from_nominatim(&paris, &paris);
        let eiffel = name("Eiffel Tower", "Tour Eiffel");
        assert_eq!(combine_label(Some(&eiffel), &area).as_deref(), Some("Eiffel Tower (Tour Eiffel), Paris"));
        assert_eq!(combine_label(None, &area).as_deref(), Some("Gros-Caillou, Paris"));

        let village = json!({"address": {"village": "Hallstatt", "country": "Austria"}});
        let village_local = json!({"address": {"village": "Hallstatt", "country": "Österreich"}});
        let area = Area::from_nominatim(&village, &village_local);
        assert_eq!(combine_label(None, &area).as_deref(), Some("Hallstatt, Austria"));

        let sea = json!({"error": "Unable to geocode"});
        assert_eq!(combine_label(None, &Area::from_nominatim(&sea, &sea)), None);
    }

    #[test]
    fn coords_format() {
        assert_eq!(format_coords(GpsPoint { lat: -33.85, lon: -70.0 }), "33.85000° S, 70.00000° W");
    }

    #[test]
    fn query_mentions_location() {
        let q = overpass_query(EIFFEL, 250.0);
        assert!(q.contains("around:250,48.8584,2.2945"));
        assert!(q.ends_with("out tags center;"));
    }
}
