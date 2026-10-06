# photo-stamp

Burns **where** and **when** a photo was taken into its bottom-left corner, using the photo's own metadata.

```
Pebble Beach Golf Course
Pebble Beach, California, United States
Tue 3 Jun 2025, 15:10 PDT
```

- **Where**: GPS coordinates turned into place names with OpenStreetMap, in English with local names in
  parentheses, on up to three lines:
  1. The **venue you're inside** (golf course, park, zoo, stadium, campus, castle, museum, airport, beach…), found
     with Overpass `is_in` — "Pebble Beach Golf Course". Venue types and notability (Wikipedia/Wikidata) are
     weighed against size, so a stadium beats the park around it and a course beats a garden within it. If you're
     not inside a venue, the most notable **landmark within 250 m** is used instead — "Eiffel Tower (Tour Eiffel)".
  2. "near …" a clearly famous landmark close by, when line 1 is a venue.
  3. **Neighbourhood, town, region, country** from [Nominatim](https://nominatim.org) — "Pebble Beach,
     California, United States".
  - Overpass requests fail over between public mirrors (override with `PHOTO_STAMP_OVERPASS_URLS`).
  - With no network (or `--offline`) the raw coordinates are used: "48.85840° N, 2.29450° E".
- **When**: the *local time at the place the photo was taken*, with its zone. The true instant comes from
  `OffsetTimeOriginal` (phones record it) or the GPS timestamp, and is converted to the timezone at the GPS
  position (offline lookup) — so a camera left on home time still shows the right local time. Without that
  information the camera's clock reading is shown as-is, without a zone.
- Only one of date/place known → only that line is drawn. **Neither → the photo is not touched.**
- White text with a dark outline and soft shadow, ~4% of the short side, wrapping long place names at commas,
  so it reads on skies, snow, night shots and busy detail alike. Any script renders (Latin, CJK, Cyrillic,
  Arabic, Thai, Devanagari…) given the Noto fonts in the Docker image.

The tool uses only metadata — it doesn't look at the image content to work out the location.

## Formats

| Format                         | Result                                                                  |
|--------------------------------|-------------------------------------------------------------------------|
| JPEG, PNG, WebP, TIFF          | Stamped in the same format                                              |
| HEIC / HEIF / AVIF             | Stamped in the same format (libheif)                                    |
| Camera RAW (DNG, CR2, CR3, NEF, ARW, ORF, RAF, RW2, PEF…) | RAW is never modified; a stamped JPEG `IMG_1.CR3.jpg` is made from the camera's embedded full-size preview |

All metadata (EXIF, XMP, IPTC, ICC profile) is copied to the stamped file, with Orientation reset (pixels are
rotated upright before drawing) and the XMP keyword `photo-stamp` added, which is how already-stamped photos
are recognised and never stamped twice.

## Watch folder (Docker)

```sh
mkdir -p photos
docker compose up -d --build
```

Then drop photos (or folders of photos) into `photos/inbox/`:

```
photos/
  inbox/      ← drop photos here
  stamped/    stamped copies (subfolders preserved)
  originals/  untouched originals, moved here once stamped
  skipped/    not stamped: no date/location, unsupported, already stamped, or unreadable
```

New files are noticed instantly via filesystem events, with the inbox also polled every 5 s as a backup (for
network shares, `/mnt/c` under WSL, or missed events). They're processed once they've stopped changing for 3 s,
so large copies are safe. If OpenStreetMap is
unreachable or only partly answers, a photo waits in the inbox and is retried (1, 2, 4 min) before settling for
what it has (town name, or coordinates). Name
clashes become `photo (2).jpg`. Watch progress with `docker compose logs -f`.

Settings (in `compose.yaml` / environment):

| Variable              | Default          | Meaning                                                        |
|-----------------------|------------------|----------------------------------------------------------------|
| `PHOTOS_DIR`          | `$PWD/photos`    | Host folder backing the `photos` volume (absolute path)        |
| `PUID` / `PGID`       | `1000`           | Host user/group that output files belong to                    |
| `PHOTO_STAMP_POLL_SECS` | `5`            | Backup polling interval (filesystem events are also used)      |
| `PHOTO_STAMP_LANG`    | `en`             | Main language for place names                                   |
| `PHOTO_STAMP_QUALITY` | `92`             | Quality for lossy re-encoding                                   |
| `PHOTO_STAMP_OFFLINE` | `false`          | Never contact OpenStreetMap                                     |

Place lookups are cached in the `cache` volume and throttled to 1 request/second per the OSM usage policies.

## Command line

```sh
photo-stamp watch <ROOT> [--poll-secs 5] [--settle-secs 3] [--no-events]
photo-stamp stamp [-n|--dry-run] <PATHS>...     # IN PLACE; RAW → "<name>.jpg" alongside
  global: --offline  --lang <LANG>  --quality <Q>
```

Running outside Docker needs `exiftool` on the PATH (and libheif for HEIC; build with
`--no-default-features` to leave HEIC out).

## Development

No local Rust toolchain needed — everything runs in a dev container via `dev.sh`:

```sh
./dev.sh cargo test
./dev.sh cargo clippy --all-targets
./dev.sh cargo fmt
docker build -t photo-stamp .        # production image
```

`docker/Dockerfile` is the dev image (Rust + clippy/rustfmt + libheif, exiftool, fonts, ImageMagick); it is
rebuilt automatically when it changes. The cargo registry is cached in `~/.cache/photo-stamp-dev`.

| Module            | Role                                                                  |
|-------------------|-----------------------------------------------------------------------|
| `src/metadata.rs` | Reading/copying metadata and RAW previews via exiftool                |
| `src/localtime.rs`| Timezone at a GPS position; local-time caption                        |
| `src/geo.rs`      | Overpass/Nominatim lookups, POI scoring, bilingual labels, cache      |
| `src/render.rs`   | Shaped, outlined + shadowed text (cosmic-text)                        |
| `src/formats.rs`  | Format detection, decode/encode (image crate, libheif, RAW previews)  |
| `src/stamp.rs`    | Per-photo pipeline and in-place mode                                  |
| `src/watch.rs`    | Inbox watcher and folder sorting                                      |
| `src/main.rs`     | CLI                                                                   |

## Limitations / next steps

- 16-bit PNG/TIFF are written back as 8-bit; WebP is re-encoded losslessly (bigger files); multi-page TIFFs
  keep only the first page.
- For HEIF, rotation comes from the container (as the spec requires); EXIF Orientation is ignored there.
- RAW output quality is that of the camera's embedded preview (usually full-size JPEG).
- The embedded EXIF thumbnail is removed rather than regenerated.
- Location ranking is heuristic; tune `category_weight` / `MIN_POI_SCORE` in `src/geo.rs`.

## Licence

Code: MIT. Embedded font: DejaVu Sans Bold — see `assets/DejaVuSans-LICENSE.txt`.
