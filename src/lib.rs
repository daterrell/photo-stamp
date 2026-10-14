//! Burn the date and place a photo was taken into the image, using its EXIF metadata.

pub mod answers;
pub mod api;
pub mod formats;
pub mod geo;
pub mod localtime;
pub mod logging;
pub mod metadata;
pub mod outbox;
pub mod paths;
pub mod remote;
pub mod render;
pub mod settings;
pub mod stamp;
pub mod status;
pub mod update;
pub mod watch;
#[cfg(windows)]
pub mod winsys;
