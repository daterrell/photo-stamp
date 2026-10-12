//! Burn the date and place a photo was taken into the image, using its EXIF metadata.

pub mod api;
pub mod formats;
pub mod gazetteer;
pub mod geo;
pub mod localtime;
pub mod logging;
pub mod metadata;
pub mod outbox;
pub mod overture;
pub mod paths;
pub mod places;
pub mod render;
pub mod settings;
pub mod stamp;
pub mod status;
pub mod watch;
#[cfg(windows)]
pub mod winsys;
pub mod wkb;
