//! The `/v1` API between photo-stamp and photo-stamp-server. JSON over HTTPS; every request carries
//! `Authorization: Bearer <device key>`.
//!
//! - `POST /v1/place` [`PlaceRequest`] → [`PlaceReply`]
//! - `POST /v1/places` [`BatchRequest`] → [`BatchReply`] (up to [`MAX_BATCH`] points)
//! - `GET  /v1/me` → [`Me`]; `PUT /v1/me` [`MeUpdate`] → [`Me`]
//! - `GET  /v1/towns.sqlite`: the small towns database clients keep for previews and fallbacks
//! - `GET  /v1/client/latest?channel=stable|beta` → [`ClientUpdate`] (204: none published)
//! - `GET  /v1/client/download/<version>`: that build's Windows zip
//! - `GET  /healthz` (no key): `ok`

use serde::{Deserialize, Serialize};

use crate::{GpsPoint, Place};

pub const MAX_BATCH: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceRequest {
    pub lat: f64,
    pub lon: f64,
    /// GPS accuracy in meters, when the camera recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_m: Option<f64>,
}

impl PlaceRequest {
    pub fn at(&self) -> GpsPoint {
        GpsPoint { lat: self.lat, lon: self.lon }
    }
}

impl From<GpsPoint> for PlaceRequest {
    fn from(p: GpsPoint) -> Self {
        Self { lat: p.lat, lon: p.lon, accuracy_m: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceReply {
    /// `None`: nothing known there (open sea); the client falls back to coordinates.
    pub place: Option<Place>,
    /// False when part of the lookup failed on the server: usable, but worth asking again later.
    pub complete: bool,
    /// The server's data release: cached answers from another release are asked again.
    pub data_version: String,
    /// Changes when this person's personal places change (cached answers are then asked again).
    pub rev: u64,
}

impl PlaceReply {
    /// The place, marked complete or not as the server said.
    pub fn into_place(self) -> Option<Place> {
        let complete = self.complete;
        self.place.map(|p| Place { complete, ..p })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchRequest {
    pub points: Vec<PlaceRequest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchReply {
    /// In the same order as the request.
    pub replies: Vec<PlaceReply>,
}

/// Who's asking, and their settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Me {
    pub user: String,
    pub device: String,
    /// Whether this device's points are kept (for trips and the map).
    pub history: bool,
    /// How long photos wait for the server before being stamped with local data only.
    pub max_wait_hours: u32,
    pub rev: u64,
    pub data_version: String,
    pub server_version: String,
    /// When the server's place database was built (YYYY-MM-DD).
    #[serde(default)]
    pub places_built: Option<String>,
    /// The Overture Maps release the server reads.
    #[serde(default)]
    pub overture_release: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wait_hours: Option<u32>,
}

/// The newest Windows build on a channel ("stable": releases; "beta": every build of main).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientUpdate {
    pub version: String,
    /// Of the zip, hex.
    pub sha256: String,
    /// Where to download it, on this server (`/v1/client/download/<version>`).
    pub path: String,
}

/// `{"error": "..."}` on failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorReply {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_small() {
        let reply = PlaceReply {
            place: Some(Place {
                primary: "Champ de Mars".into(),
                near: Some("Eiffel Tower (tour Eiffel)".into()),
                context: Some("Le Quartier Dupleix, Paris, Île-de-France Region, France".into()),
                complete: true,
            }),
            complete: true,
            data_version: "2026-09-23.1/2026-10-11".into(),
            rev: 3,
        };
        let json = serde_json::to_string(&reply).unwrap();
        assert!(json.len() < 300, "{} bytes: {json}", json.len());
        assert_eq!(serde_json::from_str::<PlaceReply>(&json).unwrap().into_place(), reply.into_place());
        let req = serde_json::to_string(&PlaceRequest::from(GpsPoint { lat: 48.8556, lon: 2.2986 })).unwrap();
        assert_eq!(req, r#"{"lat":48.8556,"lon":2.2986}"#);
    }
}
