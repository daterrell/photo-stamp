//! On-demand reads of Overture Maps data (https://overturemaps.org): named land use (golf courses,
//! parks…), infrastructure (towers, bridges…), places (POIs) and division points (neighbourhoods).
//!
//! Overture publishes GeoParquet files on public cloud storage (AWS S3, mirrored on Azure). Rows are
//! sorted spatially and every row group carries bounding-box statistics, so a lookup around one point
//! only needs a handful of row groups. We fetch exactly those byte ranges over HTTP — no API, no rate
//! limits — and keep every fetched range in a size-capped disk cache, so revisiting an area is
//! instant and works offline.
//!
//! The first lookup for a release builds a small per-dataset index (each file's row-group bounding
//! boxes) from the file footers, about 60 MiB of downloads once per release (roughly monthly).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type};
use arrow_array::{Array, ArrayRef, RecordBatch, StructArray};
use arrow_schema::DataType;
use bytes::{Buf, Bytes};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use parquet::file::reader::{ChunkReader, Length};
use parquet::file::statistics::Statistics;
use serde::{Deserialize, Serialize};

use crate::geo::Name;
use crate::metadata::GpsPoint;
use crate::places::distance_m;

const S3: &str = "https://overturemaps-us-west-2.s3.us-west-2.amazonaws.com";
const AZURE: &str = "https://overturemapswestus2.blob.core.windows.net";
/// Re-check the list of releases at most this often.
const RELEASE_CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
/// Overture deletes releases ~60 days after publishing; move on before that.
const MOVE_ON_AFTER_DAYS: i64 = 40;
const DEFAULT_CACHE_MB: u64 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dataset {
    Places,
    LandUse,
    Infrastructure,
    Division,
}

impl Dataset {
    pub const ALL: [Dataset; 4] =
        [Dataset::Places, Dataset::LandUse, Dataset::Infrastructure, Dataset::Division];

    fn path(self) -> &'static str {
        match self {
            Dataset::Places => "theme=places/type=place",
            Dataset::LandUse => "theme=base/type=land_use",
            Dataset::Infrastructure => "theme=base/type=infrastructure",
            Dataset::Division => "theme=divisions/type=division",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Dataset::Places => "places",
            Dataset::LandUse => "land_use",
            Dataset::Infrastructure => "infrastructure",
            Dataset::Division => "division",
        }
    }

    /// Point datasets: the bounding box *is* the location, so the geometry column isn't read.
    fn is_points(self) -> bool {
        matches!(self, Dataset::Places | Dataset::Division)
    }

    /// Top-level columns we read (besides names, bbox and geometry).
    fn extra_columns(self) -> &'static [&'static str] {
        match self {
            Dataset::Places => &["basic_category", "confidence"],
            Dataset::LandUse | Dataset::Infrastructure => &["class", "subtype", "wikidata"],
            Dataset::Division => &["subtype", "wikidata"],
        }
    }
}

/// One named feature near (or containing) the query point.
#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    pub dataset: Dataset,
    pub name: Name,
    /// `basic_category` for places, `class` otherwise.
    pub category: Option<String>,
    pub subtype: Option<String>,
    pub wikidata: Option<String>,
    pub confidence: Option<f64>,
    /// Bounding box: xmin, xmax, ymin, ymax.
    pub bbox: [f64; 4],
    /// The query point lies inside this feature's area.
    pub contains: bool,
    /// Metres from the query point (0 when it's inside).
    pub distance_m: f64,
}

impl Feature {
    /// Rough area of the bounding box in km².
    pub fn bbox_km2(&self) -> f64 {
        let [x0, x1, y0, y1] = self.bbox;
        let h = (y1 - y0) * 111.32;
        let w = (x1 - x0) * 111.32 * ((y0 + y1) / 2.0).to_radians().cos();
        (h * w).abs()
    }
}

// ------------------------------------------------------------------ release & index state

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileIndex {
    key: String,
    size: u64,
    /// Bounding box of each row group: xmin, xmax, ymin, ymax.
    row_groups: Vec<[f64; 4]>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ReleaseState {
    release: String,
    checked: u64,
}

/// Every file of one dataset in one release, with its row-group bounding boxes.
type DatasetIndex = Arc<Vec<FileIndex>>;

pub struct Overture {
    agent: ureq::Agent,
    dir: PathBuf,
    cache: RangeCache,
    state: Mutex<Option<ReleaseState>>,
    indexes: Mutex<HashMap<(String, Dataset), DatasetIndex>>,
    /// Bytes fetched over the network (for logging what a lookup cost).
    downloaded: AtomicU64,
    /// Serialises building each dataset's index (indexed by `Dataset as usize`).
    building: [Mutex<()>; 4],
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// "2026-09-23.1" -> days since the epoch of its date.
fn release_day(release: &str) -> Option<i64> {
    let date = chrono::NaiveDate::parse_from_str(release.get(..10)?, "%Y-%m-%d").ok()?;
    Some(date.signed_duration_since(chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?).num_days())
}

/// Which release to use: stay on `current` while it's listed and not near its deletion date,
/// otherwise the newest listed one.
fn choose_release(current: Option<&str>, listed: &[String], today: i64) -> Option<String> {
    let newest = listed.iter().max()?.clone();
    match current {
        Some(cur) if listed.iter().any(|r| r == cur) => match release_day(cur) {
            Some(day) if today - day > MOVE_ON_AFTER_DAYS && newest.as_str() > cur => Some(newest),
            _ => Some(cur.to_owned()),
        },
        _ => Some(newest),
    }
}

/// `<Key>`/`<Size>` pairs and `<Prefix>` entries from an S3 ListObjectsV2 response.
fn parse_listing(xml: &str) -> (Vec<(String, u64)>, Vec<String>, Option<String>) {
    let tag = |s: &str, name: &str| -> Option<String> {
        let open = format!("<{name}>");
        let start = s.find(&open)? + open.len();
        let end = s[start..].find(&format!("</{name}>"))? + start;
        Some(s[start..end].replace("&amp;", "&"))
    };
    let objects = xml
        .split("<Contents>")
        .skip(1)
        .filter_map(|c| Some((tag(c, "Key")?, tag(c, "Size")?.parse().ok()?)))
        .collect();
    let prefixes = xml.split("<CommonPrefixes>").skip(1).filter_map(|c| tag(c, "Prefix")).collect();
    let next = tag(xml, "NextContinuationToken");
    (objects, prefixes, next)
}

impl Overture {
    /// Keeps its index and range cache under `dir` (e.g. `%LOCALAPPDATA%\photo-stamp\overture`).
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let cap_mb = std::env::var("PHOTO_STAMP_OVERTURE_CACHE_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CACHE_MB);
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent(concat!("photo-stamp/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let state = std::fs::read_to_string(dir.join("release.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        Ok(Self {
            agent,
            dir: dir.to_owned(),
            cache: RangeCache::new(&dir.join("cache"), cap_mb * 1024 * 1024)?,
            state: Mutex::new(state),
            indexes: Mutex::new(HashMap::new()),
            downloaded: AtomicU64::new(0),
            building: Default::default(),
        })
    }

    fn get_text(&self, url: &str) -> anyhow::Result<String> {
        Ok(self.agent.get(url).call()?.body_mut().read_to_string()?)
    }

    /// One byte range of an object, from the cache or the network (S3, then Azure).
    fn range(&self, key: &str, start: u64, len: usize) -> anyhow::Result<Bytes> {
        if let Some(b) = self.cache.get(key, start, len) {
            return Ok(b);
        }
        let header = format!("bytes={}-{}", start, start + len as u64 - 1);
        let azure_key = key.strip_prefix("release/").map(|k| format!("{AZURE}/release/{k}"));
        let mut last_err = None;
        for url in std::iter::once(format!("{S3}/{key}")).chain(azure_key) {
            let attempt = || -> anyhow::Result<Bytes> {
                let mut resp = self.agent.get(&url).header("Range", &header).call()?;
                let mut buf = Vec::with_capacity(len);
                resp.body_mut().as_reader().take(len as u64).read_to_end(&mut buf)?;
                anyhow::ensure!(buf.len() == len, "short read: {} of {len} bytes", buf.len());
                Ok(Bytes::from(buf))
            };
            match attempt() {
                Ok(b) => {
                    self.cache.put(key, start, len, &b);
                    self.downloaded.fetch_add(len as u64, Ordering::Relaxed);
                    return Ok(b);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no source")).context(format!("reading {key}")))
    }

    /// The release to use, re-checking the published list at most daily.
    fn release(&self) -> anyhow::Result<String> {
        let mut state = self.state.lock().unwrap();
        if let Some(s) = state.as_ref()
            && now_secs().saturating_sub(s.checked) < RELEASE_CHECK_EVERY.as_secs()
        {
            return Ok(s.release.clone());
        }
        let listed = match self.get_text(&format!("{S3}/?list-type=2&prefix=release/&delimiter=/")) {
            Ok(xml) => parse_listing(&xml)
                .1
                .into_iter()
                .filter_map(|p| p.strip_prefix("release/").map(|r| r.trim_end_matches('/').to_owned()))
                .collect::<Vec<_>>(),
            // Offline: keep using what we have.
            Err(e) => match state.as_ref() {
                Some(s) => return Ok(s.release.clone()),
                None => return Err(e.context("listing Overture releases")),
            },
        };
        let today = (now_secs() / 86400) as i64;
        let chosen = choose_release(state.as_ref().map(|s| s.release.as_str()), &listed, today)
            .context("no Overture releases listed")?;
        if state.as_ref().is_some_and(|s| s.release != chosen) {
            log::info!("Overture: moving to release {chosen}");
            self.indexes.lock().unwrap().clear();
        }
        let new_state = ReleaseState { release: chosen.clone(), checked: now_secs() };
        let _ = std::fs::write(self.dir.join("release.json"), serde_json::to_vec(&new_state)?);
        *state = Some(new_state);
        Ok(chosen)
    }

    /// Per-file row-group bounding boxes for a dataset, built once per release.
    fn index(&self, release: &str, ds: Dataset) -> anyhow::Result<Arc<Vec<FileIndex>>> {
        if let Some(ix) = self.indexes.lock().unwrap().get(&(release.to_owned(), ds)) {
            return Ok(ix.clone());
        }
        // One builder per dataset: the startup warm-up and the first photo mustn't both download it.
        let _building = self.building[ds as usize].lock().unwrap();
        if let Some(ix) = self.indexes.lock().unwrap().get(&(release.to_owned(), ds)) {
            return Ok(ix.clone());
        }
        let path = self.dir.join(release).join(format!("{}.json", ds.slug()));
        let index: Vec<FileIndex> =
            match std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
                Some(ix) => ix,
                None => {
                    log::info!("Overture: indexing {} for release {release} (one-off download)", ds.slug());
                    let ix = self.build_index(release, ds)?;
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    std::fs::write(&path, serde_json::to_vec(&ix)?)?;
                    // Indexes of older releases are no longer needed.
                    for e in std::fs::read_dir(&self.dir)?.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if e.path().is_dir() && name != release && name != "cache" {
                            let _ = std::fs::remove_dir_all(e.path());
                        }
                    }
                    ix
                }
            };
        let index = Arc::new(index);
        self.indexes.lock().unwrap().insert((release.to_owned(), ds), index.clone());
        Ok(index)
    }

    fn build_index(&self, release: &str, ds: Dataset) -> anyhow::Result<Vec<FileIndex>> {
        let prefix = format!("release/{release}/{}/", ds.path());
        let mut files = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut url = format!("{S3}/?list-type=2&prefix={prefix}");
            if let Some(t) = &token {
                url.push_str(&format!(
                    "&continuation-token={}",
                    t.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
                ));
            }
            let (objects, _, next) = parse_listing(&self.get_text(&url)?);
            files.extend(objects.into_iter().filter(|(k, _)| k.ends_with(".parquet")));
            match next {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        anyhow::ensure!(!files.is_empty(), "no files under {prefix}");
        // Fetch footers several at a time: dozens of files, each a round trip or two.
        const PARALLEL: usize = 8;
        let chunk = files.len().div_ceil(PARALLEL);
        std::thread::scope(|s| {
            let workers: Vec<_> = files
                .chunks(chunk)
                .map(|part| {
                    s.spawn(move || {
                        part.iter()
                            .map(|(key, size)| {
                                let meta = self.metadata(key, *size)?;
                                let row_groups =
                                    (0..meta.num_row_groups()).map(|i| row_group_bbox(&meta, i)).collect();
                                Ok(FileIndex { key: key.clone(), size: *size, row_groups })
                            })
                            .collect::<anyhow::Result<Vec<_>>>()
                    })
                })
                .collect();
            let mut out = Vec::with_capacity(files.len());
            for w in workers {
                out.extend(w.join().map_err(|_| anyhow!("index worker panicked"))??);
            }
            Ok(out)
        })
    }

    /// A file's parquet metadata, from its footer (cached like any other range).
    fn metadata(&self, key: &str, size: u64) -> anyhow::Result<ParquetMetaData> {
        Ok(self.metadata_with_footer(key, size)?.0)
    }

    /// The metadata plus the footer byte ranges it came from (the parquet reader wants them again).
    fn metadata_with_footer(
        &self,
        key: &str,
        size: u64,
    ) -> anyhow::Result<(ParquetMetaData, Vec<(u64, Bytes)>)> {
        let tail = self.range(key, size - 8, 8)?;
        anyhow::ensure!(&tail[4..] == b"PAR1", "{key} isn't a parquet file");
        let meta_len = u32::from_le_bytes(tail[..4].try_into().unwrap()) as u64;
        let footer = self.range(key, size - 8 - meta_len, meta_len as usize)?;
        let ranges = vec![(size - 8 - meta_len, footer), (size - 8, tail)];
        let meta = ParquetMetaDataReader::new().parse_and_finish(&SparseFile::new(size, ranges.clone()))?;
        Ok((meta, ranges))
    }

    /// Build the release's indexes now (e.g. in the background at startup), so the first photo
    /// doesn't wait for them.
    pub fn warm_up(&self) -> anyhow::Result<()> {
        let release = self.release()?;
        std::thread::scope(|s| {
            let jobs: Vec<_> = Dataset::ALL
                .iter()
                .map(|&ds| {
                    s.spawn({
                        let release = &release;
                        move || self.index(release, ds).map(drop)
                    })
                })
                .collect();
            jobs.into_iter().try_for_each(|j| j.join().map_err(|_| anyhow!("index worker panicked"))?)
        })
    }

    /// Named features of `ds` whose bounding box comes within `radius_m` of `at`.
    pub fn features_near(&self, ds: Dataset, at: GpsPoint, radius_m: f64) -> anyhow::Result<Vec<Feature>> {
        let release = self.release()?;
        let index = self.index(&release, ds)?;
        let q = query_box(at, radius_m);
        let hits: Vec<(&FileIndex, Vec<usize>)> = index
            .iter()
            .map(|f| {
                (f, (0..f.row_groups.len()).filter(|&i| overlaps(f.row_groups[i], q)).collect::<Vec<_>>())
            })
            .filter(|(_, groups)| !groups.is_empty())
            .collect();
        // Usually one to three files; read them side by side.
        let out = std::thread::scope(|s| {
            let jobs: Vec<_> = hits
                .iter()
                .map(|(file, groups)| s.spawn(move || self.read_file(file, groups, ds, at, q)))
                .collect();
            let mut out = Vec::new();
            for j in jobs {
                out.extend(j.join().map_err(|_| anyhow!("reader panicked"))??);
            }
            anyhow::Ok(out)
        })?;
        Ok(out)
    }

    /// Total bytes fetched over the network so far.
    pub fn downloaded_bytes(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    /// Decode the matching row groups of one file, fetching just the columns we need.
    fn read_file(
        &self,
        file: &FileIndex,
        groups: &[usize],
        ds: Dataset,
        at: GpsPoint,
        q: [f64; 4],
    ) -> anyhow::Result<Vec<Feature>> {
        let (meta, footer) = self.metadata_with_footer(&file.key, file.size)?;
        let mask = projection(&meta, ds);
        let mut wanted: Vec<(u64, u64)> = groups
            .iter()
            .flat_map(|&g| meta.row_group(g).columns().iter().enumerate())
            .filter(|(i, _)| mask.leaf_included(*i))
            .map(|(_, c)| c.byte_range())
            .collect();
        // Column chunks of a row group sit next to each other: merge them (and small gaps) into a
        // few larger requests instead of one round trip per column.
        wanted.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, len) in wanted {
            match merged.last_mut() {
                Some((s, l)) if start <= *s + *l + MERGE_GAP => *l = (*l).max(start + len - *s),
                _ => merged.push((start, len)),
            }
        }
        let mut ranges = footer;
        for (start, len) in merged {
            ranges.push((start, self.range(&file.key, start, len as usize)?));
        }
        let sparse = SparseFile::new(file.size, ranges);
        let arrow_meta = ArrowReaderMetadata::try_new(Arc::new(meta), ArrowReaderOptions::new())?;
        let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(sparse, arrow_meta)
            .with_row_groups(groups.to_vec())
            .with_projection(mask)
            .build()?;
        let mut out = Vec::new();
        for batch in reader {
            collect_features(&batch?, ds, at, q, &mut out)?;
        }
        Ok(out)
    }
}

/// Column chunks closer together than this are fetched in one request.
const MERGE_GAP: u64 = 256 * 1024;

/// xmin, xmax, ymin, ymax around a point.
fn query_box(at: GpsPoint, radius_m: f64) -> [f64; 4] {
    let dlat = radius_m / 111_320.0;
    let dlon = dlat / at.lat.to_radians().cos().max(0.01);
    [at.lon - dlon, at.lon + dlon, at.lat - dlat, at.lat + dlat]
}

fn overlaps(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] <= b[1] && a[1] >= b[0] && a[2] <= b[3] && a[3] >= b[2]
}

/// Bounding box of a row group from its column statistics (everything, if stats are missing).
fn row_group_bbox(meta: &ParquetMetaData, g: usize) -> [f64; 4] {
    let rg = meta.row_group(g);
    let stat = |path: &str, want_min: bool| -> Option<f64> {
        let col = rg.columns().iter().find(|c| c.column_path().string() == path)?;
        match col.statistics()? {
            Statistics::Double(s) => if want_min { s.min_opt() } else { s.max_opt() }.copied(),
            Statistics::Float(s) => if want_min { s.min_opt() } else { s.max_opt() }.map(|v| f64::from(*v)),
            _ => None,
        }
    };
    [
        stat("bbox.xmin", true).unwrap_or(-180.0),
        stat("bbox.xmax", false).unwrap_or(180.0),
        stat("bbox.ymin", true).unwrap_or(-90.0),
        stat("bbox.ymax", false).unwrap_or(90.0),
    ]
}

/// The leaf columns we decode: names.primary, names.common, bbox.*, geometry and the dataset's extras.
fn projection(meta: &ParquetMetaData, ds: Dataset) -> ProjectionMask {
    let schema = meta.file_metadata().schema_descr();
    let leaves = (0..schema.num_columns()).filter(|&i| {
        let path = schema.column(i).path().string();
        path == "names.primary"
            || path.starts_with("names.common.")
            || path.starts_with("bbox.")
            || (path == "geometry" && !ds.is_points())
            || ds.extra_columns().contains(&path.as_str())
    });
    ProjectionMask::leaves(schema, leaves)
}

// ------------------------------------------------------------------ decoding rows

fn child<'a>(s: &'a StructArray, name: &str) -> Option<&'a ArrayRef> {
    s.column_by_name(name)
}

fn str_at(a: &ArrayRef, i: usize) -> Option<String> {
    if a.is_null(i) {
        return None;
    }
    match a.data_type() {
        DataType::Utf8 => Some(a.as_string::<i32>().value(i).to_owned()),
        DataType::LargeUtf8 => Some(a.as_string::<i64>().value(i).to_owned()),
        DataType::Utf8View => Some(a.as_string_view().value(i).to_owned()),
        _ => None,
    }
}

fn f64_at(a: &ArrayRef, i: usize) -> Option<f64> {
    if a.is_null(i) {
        return None;
    }
    match a.data_type() {
        DataType::Float64 => Some(a.as_primitive::<Float64Type>().value(i)),
        DataType::Float32 => Some(f64::from(a.as_primitive::<Float32Type>().value(i))),
        _ => None,
    }
}

fn bytes_at(a: &ArrayRef, i: usize) -> Option<&[u8]> {
    if a.is_null(i) {
        return None;
    }
    match a.data_type() {
        DataType::Binary => Some(a.as_binary::<i32>().value(i)),
        DataType::LargeBinary => Some(a.as_binary::<i64>().value(i)),
        DataType::BinaryView => Some(a.as_binary_view().value(i)),
        _ => None,
    }
}

/// `names.common[lang]` for row `i`.
fn common_name(common: &ArrayRef, i: usize, lang: &str) -> Option<String> {
    if common.is_null(i) {
        return None;
    }
    let map = common.as_map();
    let entries = map.value(i);
    let (keys, values) = (entries.column(0), entries.column(1));
    (0..entries.len()).find(|&j| str_at(keys, j).as_deref() == Some(lang)).and_then(|j| str_at(values, j))
}

fn collect_features(
    b: &RecordBatch,
    ds: Dataset,
    at: GpsPoint,
    q: [f64; 4],
    out: &mut Vec<Feature>,
) -> anyhow::Result<()> {
    let col = |name: &str| b.column_by_name(name);
    let names = col("names").context("no names column")?.as_struct();
    let primary = child(names, "primary").context("no names.primary")?;
    let common = child(names, "common");
    let bbox = col("bbox").context("no bbox column")?.as_struct();
    let (xmin, xmax, ymin, ymax) = (
        child(bbox, "xmin").context("bbox.xmin")?,
        child(bbox, "xmax").context("bbox.xmax")?,
        child(bbox, "ymin").context("bbox.ymin")?,
        child(bbox, "ymax").context("bbox.ymax")?,
    );
    let geometry = col("geometry");
    let category = col(if ds == Dataset::Places { "basic_category" } else { "class" });
    let (subtype, wikidata, confidence) = (col("subtype"), col("wikidata"), col("confidence"));

    for i in 0..b.num_rows() {
        let Some(local) = str_at(primary, i) else { continue };
        let row_box = [
            f64_at(xmin, i).unwrap_or(f64::NAN),
            f64_at(xmax, i).unwrap_or(f64::NAN),
            f64_at(ymin, i).unwrap_or(f64::NAN),
            f64_at(ymax, i).unwrap_or(f64::NAN),
        ];
        if !overlaps(row_box, q) {
            continue;
        }
        let english = common.and_then(|c| common_name(c, i, "en"));
        let Some(name) = Name::new(english.as_deref(), Some(&local)) else { continue };
        let geom = geometry.and_then(|g| bytes_at(g, i)).and_then(|w| crate::wkb::Geometry::parse(w).ok());
        let contains = geom.as_ref().is_some_and(|g| g.contains(at.lon, at.lat));
        let distance_m = if contains {
            0.0
        } else {
            match geom.as_ref().and_then(|g| g.as_point()) {
                Some((lon, lat)) => distance_m(at, GpsPoint { lat, lon }),
                // Lines and areas we're outside of: distance to the nearest edge of the bounding box.
                None => distance_m(
                    at,
                    GpsPoint {
                        lat: at.lat.clamp(row_box[2], row_box[3]),
                        lon: at.lon.clamp(row_box[0], row_box[1]),
                    },
                ),
            }
        };
        out.push(Feature {
            dataset: ds,
            name,
            category: category.and_then(|c| str_at(c, i)),
            subtype: subtype.and_then(|c| str_at(c, i)),
            wikidata: wikidata.and_then(|c| str_at(c, i)),
            confidence: confidence.and_then(|c| f64_at(c, i)),
            bbox: row_box,
            contains,
            distance_m,
        });
    }
    Ok(())
}

// ------------------------------------------------------------------ in-memory sparse file

/// A parquet "file" made of just the byte ranges we fetched. Reads outside them are errors, so the
/// parquet reader can never trigger an unbounded download.
struct SparseFile {
    len: u64,
    ranges: Vec<(u64, Bytes)>,
}

impl SparseFile {
    fn new(len: u64, mut ranges: Vec<(u64, Bytes)>) -> Self {
        ranges.sort_by_key(|r| r.0);
        Self { len, ranges }
    }

    fn slice_from(&self, start: u64) -> Option<Bytes> {
        self.ranges
            .iter()
            .find(|(s, b)| start >= *s && start < s + b.len() as u64)
            .map(|(s, b)| b.slice((start - s) as usize..))
    }
}

impl Length for SparseFile {
    fn len(&self) -> u64 {
        self.len
    }
}

impl ChunkReader for SparseFile {
    type T = bytes::buf::Reader<Bytes>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        self.slice_from(start)
            .map(Buf::reader)
            .ok_or_else(|| parquet::errors::ParquetError::General(format!("offset {start} wasn't fetched")))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        match self.slice_from(start) {
            Some(b) if b.len() >= length => Ok(b.slice(..length)),
            _ => {
                Err(parquet::errors::ParquetError::General(format!("range {start}+{length} wasn't fetched")))
            }
        }
    }
}

// ------------------------------------------------------------------ disk cache

/// Fetched byte ranges on disk, least recently used dropped first beyond a size cap.
struct RangeCache {
    dir: PathBuf,
    cap: u64,
    size: Mutex<Option<u64>>,
}

impl RangeCache {
    fn new(dir: &Path, cap: u64) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self { dir: dir.to_owned(), cap, size: Mutex::new(None) })
    }

    fn path(&self, key: &str, start: u64, len: usize) -> PathBuf {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in format!("{key}:{start}:{len}").bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        self.dir.join(format!("{h:016x}.bin"))
    }

    fn get(&self, key: &str, start: u64, len: usize) -> Option<Bytes> {
        let path = self.path(key, start, len);
        let data = std::fs::read(&path).ok().filter(|d| d.len() == len)?;
        // Mark as recently used.
        let _ =
            std::fs::File::options().write(true).open(&path).and_then(|f| f.set_modified(SystemTime::now()));
        Some(Bytes::from(data))
    }

    fn put(&self, key: &str, start: u64, len: usize, data: &[u8]) {
        let path = self.path(key, start, len);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, data).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        let mut size = self.size.lock().unwrap();
        let total = size.get_or_insert_with(|| self.scan().iter().map(|e| e.1).sum());
        *total += data.len() as u64;
        if *total > self.cap {
            *total = self.evict(self.cap * 8 / 10);
        }
    }

    fn scan(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let m = e.metadata().ok()?;
                        Some((e.path(), m.len(), m.modified().unwrap_or(UNIX_EPOCH)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Delete least recently used entries until at most `target` bytes remain; returns the new total.
    fn evict(&self, target: u64) -> u64 {
        let mut entries = self.scan();
        entries.sort_by_key(|e| e.2);
        let mut total: u64 = entries.iter().map(|e| e.1).sum();
        for (path, len, _) in entries {
            if total <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
        total
    }
}

pub fn default_dir() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("overture"))
}

/// For tests: is `e` a "not fetched" error from [`SparseFile`]?
#[cfg(test)]
fn is_unfetched(e: &parquet::errors::ParquetError) -> bool {
    e.to_string().contains("wasn't fetched")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_parses_keys_prefixes_and_tokens() {
        let xml = "<ListBucketResult><Contents><Key>release/r/a.parquet</Key><Size>10</Size></Contents>\
                   <Contents><Key>release/r/b.parquet</Key><Size>20</Size></Contents>\
                   <CommonPrefixes><Prefix>release/2026-09-23.1/</Prefix></CommonPrefixes>\
                   <NextContinuationToken>abc+/=</NextContinuationToken></ListBucketResult>";
        let (objects, prefixes, next) = parse_listing(xml);
        assert_eq!(objects, [("release/r/a.parquet".into(), 10), ("release/r/b.parquet".into(), 20)]);
        assert_eq!(prefixes, ["release/2026-09-23.1/"]);
        assert_eq!(next.as_deref(), Some("abc+/="));
    }

    #[test]
    fn release_choice_sticks_until_near_deletion() {
        let listed: Vec<String> = ["2026-08-19.0", "2026-09-23.1"].map(String::from).to_vec();
        let day = |s: &str| release_day(s).unwrap();
        // First run: newest.
        assert_eq!(choose_release(None, &listed, day("2026-09-30")).unwrap(), "2026-09-23.1");
        // Current release still fresh: stay (its cache is warm).
        assert_eq!(choose_release(Some("2026-08-19.0"), &listed, day("2026-09-10")).unwrap(), "2026-08-19.0");
        // Old enough to be deleted soon: move on.
        assert_eq!(choose_release(Some("2026-08-19.0"), &listed, day("2026-10-05")).unwrap(), "2026-09-23.1");
        // Current release gone: newest.
        assert_eq!(choose_release(Some("2026-06-01.0"), &listed, day("2026-07-01")).unwrap(), "2026-09-23.1");
    }

    #[test]
    fn sparse_file_refuses_unfetched_ranges() {
        let f = SparseFile::new(100, vec![(10, Bytes::from_static(b"abcdef"))]);
        assert_eq!(&f.get_bytes(12, 3).unwrap()[..], b"cde");
        let mut s = String::new();
        f.get_read(13).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "def");
        assert!(is_unfetched(&f.get_bytes(14, 10).unwrap_err()), "runs past the fetched range");
        assert!(f.get_read(50).is_err());
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let dir = std::env::temp_dir().join(format!("photo-stamp-rangecache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = RangeCache::new(&dir, 250).unwrap();
        cache.put("k", 0, 100, &[1; 100]);
        std::thread::sleep(Duration::from_millis(20));
        cache.put("k", 100, 100, &[2; 100]);
        std::thread::sleep(Duration::from_millis(20));
        assert!(cache.get("k", 0, 100).is_some(), "touching the first makes it recent");
        std::thread::sleep(Duration::from_millis(20));
        cache.put("k", 200, 100, &[3; 100]); // over the cap: evicts the least recently used (the second)
        assert!(cache.get("k", 0, 100).is_some());
        assert!(cache.get("k", 100, 100).is_none());
        assert!(cache.get("k", 200, 100).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Hits the real Overture data; run with `cargo test -- --ignored overture_live`.
    #[test]
    #[ignore]
    fn overture_live() {
        // Under target/ so a second run shows the warm-cache speed.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/overture-live");
        let ov = Overture::new(&dir).unwrap();
        let pebble = GpsPoint { lat: 36.565462, lon: -121.945278 };
        let land = ov.features_near(Dataset::LandUse, pebble, 300.0).unwrap();
        assert!(
            land.iter().any(|f| f.contains && f.name.preferred == "Pebble Beach Golf Course"),
            "{land:#?}"
        );
        let tokyo = GpsPoint { lat: 35.6586, lon: 139.7454 };
        let infra = ov.features_near(Dataset::Infrastructure, tokyo, 300.0).unwrap();
        assert!(infra.iter().any(|f| f.name.preferred == "Tokyo Tower" && f.contains), "{infra:#?}");
    }
}
