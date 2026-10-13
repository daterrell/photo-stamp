# photo-stamp

(If you're reading this on GitHub, you're reading a mirror).

Burns **where** and **when** a photo was taken into its bottom-left corner, using the photo's own metadata.

**Downloads**
- **Windows:** `photo-stamp-windows-<version>.zip` on the [Releases](https://github.com/daterrell/photo-stamp/releases)
  page. Every CI run on `main` (on [Forgejo](https://forgejo.terrell.ltd/dave/photo-stamp/actions)) also has a
  `photo-stamp-windows` artifact with the latest build.
- **Docker:** `docker pull forgejo.terrell.ltd/dave/photo-stamp:latest` (or `:main` for the newest build). It's a
  private package, so log in first with `docker login forgejo.terrell.ltd`.

To release, bump `version` in `Cargo.toml`, then tag and push the tag (`git tag v0.4.0 && git push origin v0.4.0`).
CI builds and attaches the Windows zip, and publishes the Docker image.

```
Pebble Beach Golf Course
Pebble Beach, California, United States
Tue 3 Jun 2025, 15:10 PDT
```

- **Where**: place names in English with local names in parentheses, on up to three lines, worked out
  **offline-first** (no online API, no rate limits):
  1. The **venue you're inside** (golf course, park, stadium, zoo, campus, tower, castle…) from
     [Overture Maps](https://overturemaps.org) — "Pebble Beach Golf Course". Venue type and fame (how many
     Wikipedia languages describe it) are weighed against size, so a stadium beats the park around it and a
     course beats a garden within it. If you're not inside a venue, a famous **landmark nearby** is used instead —
     "Eiffel Tower (Tour Eiffel)" (the more famous, the further away it can be).
  2. "near …" a clearly famous landmark close by, when line 1 is a venue.
  3. **Neighborhood** (Overture), then **town, region, country** (GeoNames) — "Pebble Beach, California,
     United States".
- **Bundled data** (`places.sqlite`, built by `scripts/build-places-db.py`): towns, regions and countries from
  [GeoNames](https://www.geonames.org) (as [Immich](https://immich.app) uses), plus famous landmarks and a fame
  score for ~1M places from [Wikidata](https://www.wikidata.org). Always available, so the town line never waits.
- **Overture Maps** is read on demand: just the few row groups around each photo, fetched as plain byte ranges
  from Overture's public files on AWS (Azure as fallback), and kept in a disk cache (400 MB cap,
  `PHOTO_STAMP_OVERTURE_CACHE_MB`). Each Overture release (roughly monthly) needs about 70 MB of file indexes,
  downloaded in the background when the watcher starts; after that a new spot costs about 10–20 MB and a few
  seconds, and repeat spots are instant and offline. If
  Overture can't be reached, photos still get town/region/country and nearby famous landmarks (watch mode first
  retries for a few minutes). `--offline` uses the bundled data only.
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
are recognized and never stamped twice.

## Windows: drop folder with an outbox

A standalone `photo-stamp.exe` that watches **the folder it's in, and all its subfolders**. Each photo dropped
there is stamped and **moved to `outbox\`**, in the same subfolders: `trip\day 1\a.jpg` becomes
`outbox\trip\day 1\a.jpg`, with the same name and file dates. The photo is removed from where it was
dropped, and the folders stay. There's no backup copy.

- **Every photo is handled**, including ones already in the folder when it starts. File dates don't matter:
  only the `photo-stamp` marker in a photo's metadata says it's already stamped.
- **Already-stamped photos, and photos with no date or location**, are moved to the outbox unchanged, so the
  drop folder always empties. The log says which ones weren't stamped and why.
- **Camera RAW files** are never modified; they move to the outbox with a stamped `<name>.jpg` beside them.
- **Name clashes** in the outbox get `a (2).jpg`, `a (3).jpg`… Nothing in the outbox is ever overwritten.
- Other files (videos, sidecars, documents) are left where they are. Photos that can't be read stay put and
  are logged.
- `--outbox <folder>` puts the outbox somewhere else, e.g. on another drive.
- **Runs in the background:** no window, one instance per folder. It logs to
  `%LOCALAPPDATA%\photo-stamp\photo-stamp.log`.
- **Filesystem events** give instant pickup, with a scan every 30 s as backup. Files open in another program
  (or mid-sync) are retried, as are photos taken while Overture Maps couldn't be reached (for a few minutes,
  then they're stamped with the bundled town/landmark data).
- **HEIC** (iPhone) support is compiled in, so no codec packs are needed. exiftool and the bundled place data
  (`places.sqlite`) live in `photo-stamp-files\` beside the exe, which the watcher ignores. The Overture cache
  is in `%LOCALAPPDATA%\photo-stamp\overture`.

Setup: copy `photo-stamp.exe`, `photo-stamp-ui.exe` and `photo-stamp-files\` into an empty drop folder, then double-click
photo-stamp.exe (or open photo-stamp-ui.exe and start it from the dashboard). From a
terminal in that folder:

```
photo-stamp.exe --install-startup     # start at login (HKCU Run entry) and start now
photo-stamp.exe --uninstall-startup
photo-stamp.exe --stop
photo-stamp.exe stamp <files>         # one-off
```

### Dashboard

`photo-stamp-ui.exe` shows and controls photo-stamp in your browser. It's a separate program: double-click it
and it opens `http://127.0.0.1:8765/`, with a button to start photo-stamp if it isn't running.

- **Activity:**
  - the photo being stamped, as a small preview that changes from the original to the stamped result, with
    the steps (finding the place, drawing the caption, saving);
  - the queue, with why each photo is waiting (still copying, map data unreachable, open in another program,
    paused);
  - a progress bar with photos done and time left, estimated separately for new spots (map data to download)
    and places you've been before;
  - a running table of everything processed, each with its caption, how long it took and a before/after view.
- **Not stamped:** photos moved unchanged, grouped by reason, and photos that couldn't be processed, with
  **Try again**.
- **Health:** map data release, indexes, cache and connection; the bundled place data; all-time totals; and the
  live log.
- **Settings:** outbox, quality, timing, offline mode, a notification when a batch is done, and start at login.
- **Pause / Resume** (new photos are still noticed and queued), **Stop**, and **Start**.

How it fits together: photo-stamp serves a small API on 127.0.0.1 (a random port), protected by a random token.
It writes the port and token to `%LOCALAPPDATA%\photo-stamp\services\<folder>.json`, which only you can
read. photo-stamp-ui finds services there, passes the page's requests on with the token, and starts them when
asked. It only answers requests addressed to `127.0.0.1`/`localhost`, and changes must come from its own page.
It exits after 30 minutes with no dashboard open; `photo-stamp-ui.exe --stop` stops it sooner.

Settings changed in the dashboard are kept in `%LOCALAPPDATA%\photo-stamp\settings-<folder>.json`; options on the
command line override them. History and previews (the most recent 2,000) are kept there too.

Build it (Docker only; produces `dist/photo-stamp-windows/` and `dist/photo-stamp-windows.zip`):

```sh
scripts/build-windows.sh
```

`docker/Dockerfile.windows` cross-compiles libde265, x265 and libheif as static libraries with mingw-w64,
then builds the exe against them. The script adds the portable exiftool release, verified by checksum.

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
so large copies are safe. If Overture Maps can't be reached, a photo waits in the inbox and is retried
(1, 2, 4 min) before settling for the bundled data (town, region, country and famous landmarks). Name
clashes become `photo (2).jpg`. Watch progress with `docker compose logs -f`.

Settings (in `compose.yaml` / environment):

| Variable              | Default          | Meaning                                                        |
|-----------------------|------------------|----------------------------------------------------------------|
| `PHOTOS_DIR`          | `$PWD/photos`    | Host folder backing the `photos` volume (absolute path)        |
| `PUID` / `PGID`       | `1000`           | Host user/group that output files belong to                    |
| `PHOTO_STAMP_POLL_SECS` | `5`            | Backup polling interval (filesystem events are also used)      |
| `PHOTO_STAMP_QUALITY` | `92`             | Quality for lossy re-encoding                                   |
| `PHOTO_STAMP_OFFLINE` | `false`          | Bundled place data only (no Overture downloads)                 |
| `PHOTO_STAMP_OVERTURE_CACHE_MB` | `400`  | Size cap of the Overture download cache                         |

The Overture cache lives in the `cache` volume.

## Command line

```sh
photo-stamp watch <ROOT> [--poll-secs 5] [--settle-secs 3] [--no-events]
photo-stamp stamp [-n|--dry-run] <PATHS>...     # IN PLACE; RAW → "<name>.jpg" alongside
  global: --offline  --quality <Q>
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
| `src/gazetteer.rs`| Naming a spot: venue / landmark / neighborhood / town scoring         |
| `src/places.rs`   | The bundled GeoNames + Wikidata database (`places.sqlite`)            |
| `src/overture.rs` | On-demand Overture Maps reads: range requests, row-group pruning, cache |
| `src/wkb.rs`      | Point-in-polygon on WKB geometry                                      |
| `src/geo.rs`      | Place/Name types; the previous OpenStreetMap geocoder (not used by default) |
| `src/render.rs`   | Shaped, outlined + shadowed text (cosmic-text)                        |
| `src/formats.rs`  | Format detection, decode/encode (image crate, libheif, RAW previews)  |
| `src/stamp.rs`    | Per-photo pipeline; one-off `stamp` in place                          |
| `src/outbox.rs`   | Windows drop folder: stamp and move to the outbox                     |
| `src/watch.rs`    | Inbox watcher and folder sorting                                      |
| `src/status.rs`   | What the service is doing: queue, progress, results, history, previews |
| `src/api.rs`      | The service's local API for the dashboard                             |
| `src/settings.rs` | Settings the dashboard can change                                     |
| `src/main.rs`     | CLI                                                                   |
| `http/`           | Minimal HTTP server and the service registry, shared with the dashboard |
| `ui/`             | `photo-stamp-ui`: the dashboard server and page (`ui/web/`)           |

## Limitations / next steps

- 16-bit PNG/TIFF are written back as 8-bit; WebP is re-encoded losslessly (bigger files); multi-page TIFFs
  keep only the first page.
- For HEIF, rotation comes from the container (as the spec requires); EXIF Orientation is ignored there.
- RAW output quality is that of the camera's embedded preview (usually full-size JPEG).
- The embedded EXIF thumbnail is removed rather than regenerated.
- Location ranking is heuristic; tune `category_weight` / `MIN_POI_SCORE` in `src/geo.rs`.

## License

Code: MIT. Embedded font: DejaVu Sans Bold — see `assets/DejaVuSans-LICENSE.txt`.
