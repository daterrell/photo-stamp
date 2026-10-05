# photo-stamp

Burns **where** and **when** a photo was taken into the bottom-left corner of the image, using its EXIF metadata.

```
Eiffel Tower, Paris
Wed 12 Jun 2024, 14:32
```

- **When**: EXIF `DateTimeOriginal`, falling back to `DateTimeDigitized`, then `DateTime`.
- **Where**: EXIF GPS coordinates, turned into a place name with OpenStreetMap:
  - [Overpass](https://overpass-api.de) finds notable named features within 250 m (attractions, museums,
    monuments, castles, peaks, parks, towers…), scored by category, whether they have a Wikipedia/Wikidata
    entry, and distance. The best one wins if it's notable enough: "Elizabeth Tower, City of Westminster".
  - Otherwise [Nominatim](https://nominatim.org) reverse geocoding gives a neighbourhood and town:
    "Brixton, Greater London" or "Hallstatt, Austria".
  - With no network (or `--offline`) the raw coordinates are used: "48.85840° N, 2.29450° E".
- Only one of date/place present → only that line is drawn. **No EXIF, or EXIF with neither → the file is
  not touched.**
- Text is white with a dark outline and a soft drop shadow, sized to ~4% of the image's short side (shrunk to
  fit if needed), so it reads on skies, snow, night shots and busy detail alike.

The MVP uses only EXIF — it does not look at the image content to work out the location.

## Usage

```sh
photo-stamp [OPTIONS] <PATHS>...

  -n, --dry-run        Show the caption each photo would get without modifying anything
      --offline        Don't contact OpenStreetMap; caption with coordinates instead
      --lang <LANG>    Preferred language for place names [default: en]
      --quality <Q>    JPEG quality for re-encoded photos [default: 92]
```

Directories are searched recursively for `.jpg`/`.jpeg`.

> **Files are modified in place.** Run `--dry-run` first and keep a backup of originals.

What's preserved on rewrite: EXIF (with Orientation reset to "normal", since the pixels are rotated upright
before drawing), XMP, IPTC, ICC colour profile, file permissions and modification time. A JPEG comment marker is
added so running the tool twice never stamps a photo twice. The embedded EXIF thumbnail is left as-is
(unstamped).

Place lookups are cached in `~/.cache/photo-stamp/places.json` (by coordinates rounded to ~11 m) and requests
are throttled to 1 per second per the OSM usage policies.

## Development

No local Rust toolchain needed — everything runs in Docker via `dev.sh`:

```sh
./dev.sh cargo test
./dev.sh cargo clippy --all-targets
./dev.sh cargo fmt
./dev.sh cargo build --release     # → target/release/photo-stamp, runs on the host
```

The first run builds a small image (`docker/Dockerfile`: `rust:1-slim` + clippy/rustfmt). The cargo registry is
cached in `~/.cache/photo-stamp-dev`.

| Module            | Role                                                             |
|-------------------|------------------------------------------------------------------|
| `src/metadata.rs` | EXIF parsing (date, GPS, orientation) and in-place orientation reset |
| `src/geo.rs`      | Overpass/Nominatim lookups, POI scoring, label building, cache  |
| `src/render.rs`   | Outlined + shadowed text rendering                               |
| `src/stamp.rs`    | Per-file pipeline: decode, rotate, draw, re-encode, carry metadata |
| `src/main.rs`     | CLI                                                              |

## Limitations / next steps

- JPEG only (HEIC, PNG, TIFF and RAW are skipped).
- Times are shown as recorded by the camera (local camera time); `OffsetTimeOriginal` isn't used yet.
- Re-encoding a JPEG is slightly lossy (quality 92 by default).
- Location ranking is heuristic; tune `category_weight` / `MIN_POI_SCORE` in `src/geo.rs`.

## Licence

Code: MIT. Embedded font: DejaVu Sans Bold — see `assets/DejaVuSans-LICENSE.txt`.
