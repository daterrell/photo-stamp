//! Showing the time of day as it was *where the photo was taken*.
//!
//! Camera clocks are often left on home time when traveling, so when we can establish the true
//! instant (UTC) we convert it into the timezone at the photo's GPS position.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::metadata::{GpsPoint, PhotoInfo};

const FORMAT: &str = "%a %-d %b %Y, %H:%M";

/// IANA timezone at a location, from an embedded offline boundary map.
pub fn timezone_at(at: GpsPoint) -> Option<Tz> {
    static FINDER: OnceLock<tzf_rs::DefaultFinder> = OnceLock::new();
    let name = FINDER.get_or_init(tzf_rs::DefaultFinder::new).get_tz_name(at.lon, at.lat);
    name.parse().ok()
}

/// The true moment the photo was taken, when the metadata pins it down.
/// Prefers the camera's recorded UTC offset; falls back to the GPS fix time.
pub fn instant(info: &PhotoInfo) -> Option<DateTime<Utc>> {
    match (info.taken, info.offset) {
        (Some(t), Some(off)) => t.and_local_timezone(off).single().map(|t| t.with_timezone(&Utc)),
        _ => info.gps_time,
    }
}

/// "Wed 12 Jun 2024, 14:32 CEST" when we know both the instant and the place's timezone,
/// otherwise the camera's own clock reading without a zone.
pub fn caption(info: &PhotoInfo, tz: Option<Tz>) -> Option<String> {
    if let (Some(tz), Some(utc)) = (tz, instant(info)) {
        let local = utc.with_timezone(&tz);
        return Some(format!("{} {}", local.format(FORMAT), zone_label(&local.format("%Z").to_string())));
    }
    info.taken.map(|t| t.format(FORMAT).to_string())
}

/// Zones without a common abbreviation come out as "+07"; make those "UTC+07".
fn zone_label(abbrev: &str) -> String {
    if abbrev.starts_with(['+', '-']) { format!("UTC{abbrev}") } else { abbrev.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, 0).unwrap()
    }

    #[test]
    fn finds_timezones() {
        assert_eq!(timezone_at(GpsPoint { lat: 48.8584, lon: 2.2945 }), Some(chrono_tz::Europe::Paris));
        assert_eq!(timezone_at(GpsPoint { lat: 35.6586, lon: 139.7454 }), Some(chrono_tz::Asia::Tokyo));
        assert_eq!(timezone_at(GpsPoint { lat: 40.6892, lon: -74.0445 }), Some(chrono_tz::America::New_York));
    }

    #[test]
    fn camera_left_on_home_time_is_corrected() {
        // A London-set camera (+01:00 in summer) shoots in Tokyo at 22:32 local.
        let info = PhotoInfo {
            taken: Some(at(2024, 6, 12, 14, 32)),
            offset: FixedOffset::east_opt(3600),
            ..Default::default()
        };
        assert_eq!(caption(&info, Some(chrono_tz::Asia::Tokyo)).unwrap(), "Wed 12 Jun 2024, 22:32 JST");
    }

    #[test]
    fn gps_time_used_without_offset() {
        let info = PhotoInfo {
            taken: Some(at(2024, 1, 5, 9, 0)),
            gps_time: Some(at(2024, 1, 5, 14, 0).and_utc()),
            ..Default::default()
        };
        assert_eq!(caption(&info, Some(chrono_tz::America::New_York)).unwrap(), "Fri 5 Jan 2024, 09:00 EST");
        // Even with no camera date at all.
        let gps_only = PhotoInfo { taken: None, ..info };
        assert_eq!(
            caption(&gps_only, Some(chrono_tz::America::New_York)).unwrap(),
            "Fri 5 Jan 2024, 09:00 EST"
        );
    }

    #[test]
    fn numeric_zone_abbreviations_get_utc_prefix() {
        let info = PhotoInfo { gps_time: Some(at(2024, 3, 1, 3, 0).and_utc()), ..Default::default() };
        let tz: Tz = "Asia/Ho_Chi_Minh".parse().unwrap();
        assert_eq!(caption(&info, Some(tz)).unwrap(), "Fri 1 Mar 2024, 10:00 UTC+07");
    }

    #[test]
    fn falls_back_to_camera_clock() {
        let info = PhotoInfo { taken: Some(at(2021, 8, 15, 21, 10)), ..Default::default() };
        assert_eq!(caption(&info, Some(chrono_tz::Europe::Paris)).unwrap(), "Sun 15 Aug 2021, 21:10");
        assert_eq!(caption(&PhotoInfo::default(), None), None);
    }
}
