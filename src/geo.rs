//! How the client names places: it asks the place server ([`crate::remote::RemoteGeocoder`]),
//! remembering the answers, and keeps a small towns database for when the server can't be reached.

use std::time::Duration;

use serde::Serialize;

pub use photo_stamp_proto::{GpsPoint, Name, Place, format_coords};

/// Anything that can name a location.
pub trait Geocoder {
    /// The full answer: from the place server, or a cached one. An error means it isn't available
    /// now (e.g. the server can't be reached); callers that can wait try again later.
    fn place(&self, at: GpsPoint) -> anyhow::Result<Option<Place>>;

    /// What's known without the server: a cached answer, or the nearest town from the local towns
    /// database. Used when a photo shouldn't wait any longer.
    fn local_place(&self, at: GpsPoint) -> Option<Place> {
        self.place(at).ok().flatten()
    }

    /// The nearest town, for showing where a waiting photo is from ("near Kyoto").
    fn nearby_town(&self, _at: GpsPoint) -> Option<String> {
        None
    }

    /// How long photos may wait for the server before they're stamped with local data only.
    fn max_wait(&self) -> Option<Duration> {
        None
    }

    /// Prepare in the background (e.g. connect to the server) so the first lookup is quick.
    fn warm_up(&self) {}

    /// The state of the place lookups, for the dashboard.
    fn health(&self) -> GeoHealth {
        GeoHealth::default()
    }
}

/// The state of the place lookups, for the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GeoHealth {
    /// When the local towns database was built (YYYY-MM-DD).
    pub towns_built: Option<String>,
    /// `None` when no place server is set up.
    pub server: Option<ServerHealth>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ServerHealth {
    pub url: String,
    /// Whether the last request worked (`None`: nothing asked yet).
    pub connected: Option<bool>,
    pub last_error: Option<String>,
    /// Round trip of the last answered request.
    pub latency_ms: Option<u64>,
    /// Who the server says this device is, and its data.
    pub user: Option<String>,
    pub device: Option<String>,
    pub history: Option<bool>,
    pub max_wait_hours: Option<u32>,
    pub data_version: Option<String>,
    pub server_version: Option<String>,
    pub places_built: Option<String>,
    pub overture_release: Option<String>,
    /// Answers remembered on this PC, and how often they saved asking.
    pub cached_answers: u64,
    pub cache_hits: u64,
    pub server_answers: u64,
}

/// Never looks anything up.
pub struct NoGeocoder;

impl Geocoder for NoGeocoder {
    fn place(&self, _: GpsPoint) -> anyhow::Result<Option<Place>> {
        Ok(None)
    }
}
