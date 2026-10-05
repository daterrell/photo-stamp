//! Extracting the bits of EXIF we care about, and patching the raw TIFF block.

use chrono::NaiveDateTime;
use exif::{In, Reader, Tag, Value};

/// What the photo tells us about when and where it was taken.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoInfo {
    pub taken: Option<NaiveDateTime>,
    pub gps: Option<GpsPoint>,
    /// EXIF orientation (1..=8); 1 when absent.
    pub orientation: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpsPoint {
    pub lat: f64,
    pub lon: f64,
}

impl PhotoInfo {
    /// True when there is nothing worth burning in.
    pub fn is_empty(&self) -> bool {
        self.taken.is_none() && self.gps.is_none()
    }
}

/// Parse a raw TIFF/EXIF block (no `Exif\0\0` prefix).
pub fn parse_exif(tiff: &[u8]) -> anyhow::Result<PhotoInfo> {
    let exif = Reader::new().read_raw(tiff.to_vec())?;

    let taken = [Tag::DateTimeOriginal, Tag::DateTimeDigitized, Tag::DateTime]
        .iter()
        .find_map(|&tag| exif.get_field(tag, In::PRIMARY).and_then(|f| parse_datetime(&f.value)));

    let coord = |value_tag, ref_tag, negative: &[u8]| -> Option<f64> {
        let deg = dms_to_degrees(&exif.get_field(value_tag, In::PRIMARY)?.value)?;
        let is_negative = match &exif.get_field(ref_tag, In::PRIMARY)?.value {
            Value::Ascii(v) => v.first().and_then(|s| s.first()).is_some_and(|c| negative.contains(c)),
            _ => false,
        };
        Some(if is_negative { -deg } else { deg })
    };
    let gps = match (
        coord(Tag::GPSLatitude, Tag::GPSLatitudeRef, b"Ss"),
        coord(Tag::GPSLongitude, Tag::GPSLongitudeRef, b"Ww"),
    ) {
        // Exactly 0,0 is what many cameras write when they have no fix.
        (Some(lat), Some(lon)) if lat.abs() <= 90.0 && lon.abs() <= 180.0 && (lat, lon) != (0.0, 0.0) => {
            Some(GpsPoint { lat, lon })
        }
        _ => None,
    };

    let orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .filter(|o| (1..=8).contains(o))
        .map_or(1, |o| o as u8);

    Ok(PhotoInfo { taken, gps, orientation })
}

fn parse_datetime(value: &Value) -> Option<NaiveDateTime> {
    let Value::Ascii(parts) = value else { return None };
    let dt = exif::DateTime::from_ascii(parts.first()?).ok()?;
    chrono::NaiveDate::from_ymd_opt(dt.year.into(), dt.month.into(), dt.day.into())?.and_hms_opt(
        dt.hour.into(),
        dt.minute.into(),
        dt.second.into(),
    )
}

fn dms_to_degrees(value: &Value) -> Option<f64> {
    let Value::Rational(r) = value else { return None };
    let part = |i: usize| r.get(i).filter(|x| x.denom != 0).map(|x| x.to_f64());
    let deg = part(0)? + part(1).unwrap_or(0.0) / 60.0 + part(2).unwrap_or(0.0) / 3600.0;
    deg.is_finite().then_some(deg)
}

/// Rewrite IFD0's Orientation tag to 1 ("upright") in place.
/// Returns false if the tag isn't present or the block is malformed.
pub fn reset_orientation(tiff: &mut [u8]) -> bool {
    let big_endian = match tiff.get(..2) {
        Some(b"MM") => true,
        Some(b"II") => false,
        _ => return false,
    };
    let u16_at = |b: &[u8], at: usize| -> Option<u16> {
        let bytes: [u8; 2] = b.get(at..at + 2)?.try_into().ok()?;
        Some(if big_endian { u16::from_be_bytes(bytes) } else { u16::from_le_bytes(bytes) })
    };
    let u32_at = |b: &[u8], at: usize| -> Option<u32> {
        let bytes: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        Some(if big_endian { u32::from_be_bytes(bytes) } else { u32::from_le_bytes(bytes) })
    };

    let Some(ifd0) = u32_at(tiff, 4).map(|o| o as usize) else { return false };
    let Some(count) = u16_at(tiff, ifd0) else { return false };
    for i in 0..count as usize {
        let entry = ifd0 + 2 + i * 12;
        // Tag 0x0112 (Orientation), type 3 (SHORT): value lives in the first 2 bytes of the value field.
        if u16_at(tiff, entry) == Some(0x0112) && u16_at(tiff, entry + 2) == Some(3) {
            let one = if big_endian { 1u16.to_be_bytes() } else { 1u16.to_le_bytes() };
            return match tiff.get_mut(entry + 8..entry + 10) {
                Some(slot) => {
                    slot.copy_from_slice(&one);
                    true
                }
                None => false,
            };
        }
    }
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use exif::experimental::Writer;
    use exif::{Field, Rational};

    pub fn field(tag: Tag, value: Value) -> Field {
        Field { tag, ifd_num: In::PRIMARY, value }
    }

    pub fn ascii(s: &str) -> Value {
        Value::Ascii(vec![s.as_bytes().to_vec()])
    }

    pub fn dms(deg: u32, min: u32, sec_x100: u32) -> Value {
        Value::Rational(vec![
            Rational { num: deg, denom: 1 },
            Rational { num: min, denom: 1 },
            Rational { num: sec_x100, denom: 100 },
        ])
    }

    /// Build a TIFF/EXIF block from fields.
    pub fn build_exif(fields: &[Field], big_endian: bool) -> Vec<u8> {
        let mut w = Writer::new();
        for f in fields {
            w.push_field(f);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        w.write(&mut out, !big_endian).unwrap();
        out.into_inner()
    }

    pub fn eiffel_fields() -> Vec<Field> {
        vec![
            field(Tag::Orientation, Value::Short(vec![6])),
            field(Tag::DateTimeOriginal, ascii("2024:06:12 14:32:05")),
            field(Tag::GPSLatitudeRef, ascii("N")),
            field(Tag::GPSLatitude, dms(48, 51, 3024)),
            field(Tag::GPSLongitudeRef, ascii("E")),
            field(Tag::GPSLongitude, dms(2, 17, 4020)),
        ]
    }

    #[test]
    fn parses_date_gps_orientation() {
        let info = parse_exif(&build_exif(&eiffel_fields(), false)).unwrap();
        assert_eq!(info.taken.unwrap().to_string(), "2024-06-12 14:32:05");
        let gps = info.gps.unwrap();
        assert!((gps.lat - 48.8584).abs() < 1e-4, "{gps:?}");
        assert!((gps.lon - 2.2945).abs() < 1e-4, "{gps:?}");
        assert_eq!(info.orientation, 6);
    }

    #[test]
    fn southern_western_hemispheres_are_negative() {
        let fields = [
            field(Tag::GPSLatitudeRef, ascii("S")),
            field(Tag::GPSLatitude, dms(33, 51, 0)),
            field(Tag::GPSLongitudeRef, ascii("W")),
            field(Tag::GPSLongitude, dms(70, 0, 0)),
        ];
        let gps = parse_exif(&build_exif(&fields, true)).unwrap().gps.unwrap();
        assert!(gps.lat < 0.0 && gps.lon < 0.0);
    }

    #[test]
    fn zero_island_and_blank_dates_are_ignored() {
        let fields = [
            field(Tag::DateTimeOriginal, ascii("0000:00:00 00:00:00")),
            field(Tag::GPSLatitudeRef, ascii("N")),
            field(Tag::GPSLatitude, dms(0, 0, 0)),
            field(Tag::GPSLongitudeRef, ascii("E")),
            field(Tag::GPSLongitude, dms(0, 0, 0)),
            field(Tag::Make, ascii("Acme")),
        ];
        let info = parse_exif(&build_exif(&fields, false)).unwrap();
        assert!(info.is_empty(), "{info:?}");
    }

    #[test]
    fn reset_orientation_both_byte_orders() {
        for big_endian in [false, true] {
            let mut tiff = build_exif(&eiffel_fields(), big_endian);
            assert!(reset_orientation(&mut tiff));
            let info = parse_exif(&tiff).unwrap();
            assert_eq!(info.orientation, 1);
            assert!(info.taken.is_some() && info.gps.is_some());
        }
    }

    #[test]
    fn reset_orientation_rejects_garbage() {
        assert!(!reset_orientation(&mut [1, 2, 3]));
        let mut no_orientation = build_exif(&[field(Tag::Make, ascii("Acme"))], false);
        assert!(!reset_orientation(&mut no_orientation));
    }
}
