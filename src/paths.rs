//! Where photo-stamp keeps its own files (place cache, settings, history, log). Shared with the
//! dashboard, which needs to find the same folder.

pub use photo_stamp_http::registry::{data_dir, folder_key};
