//! What photo-stamp (the client, where the photos are) and photo-stamp-server (the place server)
//! share: place names and the `/v1` request and reply types.
//!
//! The client sends a point (a few dozen bytes) and gets the caption lines back (well under a
//! kilobyte); the server holds the full place data.

use serde::{Deserialize, Serialize};

pub mod api;
#[cfg(feature = "places")]
pub mod places;
pub mod version;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GpsPoint {
    pub lat: f64,
    pub lon: f64,
}

impl GpsPoint {
    /// Whether this is a real position on Earth.
    pub fn is_valid(&self) -> bool {
        self.lat.is_finite() && self.lon.is_finite() && self.lat.abs() <= 90.0 && self.lon.abs() <= 180.0
    }

    /// ~11 m: photos from the same spot share an answer (server and client caches use this key).
    pub fn cache_key(&self) -> String {
        format!("{:.4},{:.4}", self.lat, self.lon)
    }
}

/// Where a photo was taken, as caption lines.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Place {
    /// Most specific name: the venue we're in, a landmark, or a neighborhood/town.
    pub primary: String,
    /// A famous landmark close by, when `primary` is a venue we're inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<String>,
    /// Neighborhood, town, region, country.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// False when part of the lookup failed (e.g. map data unreachable): usable, but worth retrying.
    #[serde(skip)]
    pub complete: bool,
}

impl Place {
    pub fn lines(&self) -> Vec<String> {
        let near = self.near.as_ref().map(|n| format!("near {n}"));
        std::iter::once(self.primary.clone()).chain(near).chain(self.context.clone()).collect()
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

    /// "Munich (München)", or just "Paris".
    pub fn display(&self) -> String {
        match &self.local {
            Some(l) => format!("{} ({l})", self.preferred),
            None => self.preferred.clone(),
        }
    }
}

/// "48.85840° N, 2.29450° E"
pub fn format_coords(at: GpsPoint) -> String {
    let ns = if at.lat < 0.0 { 'S' } else { 'N' };
    let ew = if at.lon < 0.0 { 'W' } else { 'E' };
    format!("{:.5}° {ns}, {:.5}° {ew}", at.lat.abs(), at.lon.abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_skip_redundant_local_forms() {
        let n = Name::new(Some("Champ de Mars"), Some("Champ-de-Mars")).unwrap();
        assert_eq!(n.display(), "Champ de Mars");
        let n = Name::new(Some("Munich"), Some("München")).unwrap();
        assert_eq!(n.display(), "Munich (München)");
        assert_eq!(Name::new(None, Some(" Kyoto ")).unwrap().display(), "Kyoto");
        assert!(Name::new(Some("  "), None).is_none());
    }

    #[test]
    fn place_lines_and_coordinates() {
        let p = Place {
            primary: "Pebble Beach Golf Course".into(),
            near: Some("Lone Cypress".into()),
            context: Some("Pebble Beach, California, United States".into()),
            complete: true,
        };
        assert_eq!(
            p.lines(),
            ["Pebble Beach Golf Course", "near Lone Cypress", "Pebble Beach, California, United States"]
        );
        assert_eq!(format_coords(GpsPoint { lat: -33.8568, lon: 151.2153 }), "33.85680° S, 151.21530° E");
        assert_eq!(GpsPoint { lat: 36.565462, lon: -121.945278 }.cache_key(), "36.5655,-121.9453");
        assert!(!GpsPoint { lat: 91.0, lon: 0.0 }.is_valid());
    }
}
