//! Burn the date and place a photo was taken into the image, using its EXIF metadata.

pub mod formats;
pub mod geo;
pub mod inplace;
pub mod localtime;
pub mod logging;
pub mod metadata;
pub mod paths;
pub mod render;
pub mod stamp;
pub mod watch;
#[cfg(windows)]
pub mod winsys;
