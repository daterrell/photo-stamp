# photo-stamp

(If you're reading this on GitHub, you're reading a mirror).

Burns **where** and **when** a photo was taken into its bottom-left corner, using the photo's own metadata.

**Downloads**
- **Windows:** `photo-stamp-windows-<version>.zip` on the [Releases](https://github.com/daterrell/photo-stamp/releases)
  page. Every CI run on `main` (on [Forgejo](https://forgejo.terrell.ltd/dave/photo-stamp/actions)) also has a
  `photo-stamp-windows` artifact with the latest build.
- **Docker:** `docker pull forgejo.terrell.ltd/dave/photo-stamp:latest` (or `:main` for the newest build). It's a
  private package, so log in first with `docker login forgejo.terrell.ltd`.

**Versions come from tags**, not `Cargo.toml`: a build of tag `v0.7.0` is `0.7.0`, and anything in between is
like `0.7.1-dev.3+g1b37dbe` (`scripts/version.sh`; baked into every program, shown by `--version` and the
dashboard). To release, tag a commit on `main` and push the tag: `git tag v0.7.0 && git push origin v0.7.0`.
CI builds it like any other commit, then publishes it: a GitHub Release with the Windows zip, and the `:0.7.0`
and `:latest` images.

**Builds** leave only usable artifacts, published: every build of `main` (beta) and every tag (stable) puts the
Windows zip in Forgejo's package registry (`photo-stamp-windows`; the newest 10 betas are kept) and the images in
its container registry (version tags, `:main`, `:latest`, and the newest 10 `sha-…` tags). Pull request builds
keep their zip as an Actions artifact for 3 days. Each CI job removes what it created on the runner; a final job
removes leftovers of cancelled runs and keeps the shared caches bounded (Docker build cache 20 GB, `main`'s
cargo target 25 GB).

**Updates** reach Windows installs through the place server: it relays the builds from the package registry, so
clients need nothing but their device key. A client checks every 6 hours (or with **Check now** in the
dashboard), downloads a newer build on its channel (Settings > Updates: releases, or every build), checks its
SHA-256, and offers it: **Install and restart** finishes the photo in hand, swaps the program files (the old ones
are kept as `*.old`), and starts the new version, restarting the dashboard server too. If the new version doesn't
come up within 90 s, the old files are put back and the old version starts again. Two minutes after a successful
update the `.old` files and the download are removed.

```
Pebble Beach Golf Course
Pebble Beach, California, United States
Tue 3 Jun 2025, 15:10 PDT
```

- **Where**: place names in English with local names in parentheses, on up to three lines:
  1. The **venue you're inside** (golf course, park, stadium, zoo, campus, tower, castle…) from
     [Overture Maps](https://overturemaps.org) — "Pebble Beach Golf Course". Venue type and fame (how many
     Wikipedia languages describe it) are weighed against size, so a stadium beats the park around it and a
     course beats a garden within it. If you're not inside a venue, a famous **landmark nearby** is used instead —
     "Eiffel Tower (Tour Eiffel)" (the more famous, the further away it can be).
  2. "near …" a clearly famous landmark close by, when line 1 is a venue.
  3. **Neighborhood** (Overture), then **town, region, country** ([GeoNames](https://www.geonames.org), as
     [Immich](https://immich.app) uses) — "Pebble Beach, California, United States".
- **Places come from your own place server** (`photo-stamp-server`, see below), which holds the full data: an
  Overture Maps mirror, GeoNames and [Wikidata](https://www.wikidata.org). A client sends just the point (about
  30 bytes) and gets the caption back (about 200 bytes), typically in well under a second.
- **On the client** (where the photos are): answers are remembered (`answers.sqlite`), so repeat spots are
  instant and work offline. A small towns database (`towns.sqlite`, 7 MB, kept up to date from the server)
  shows where waiting photos are from. While the server can't be reached, photos **wait**; after the person's
  wait time (set on the server, 72 hours unless changed), or when you choose **Stamp now** in the dashboard,
  they're stamped with what the PC knows: a remembered answer, else the nearest town. With no server set up,
  photos are named by the nearest town.
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
dropped (the original is kept in `originals\`), and the folders stay.

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
- **Originals are kept** in `originals\`, in the same subfolders, so photos can be stamped again later
  (`--originals` or the dashboard puts them elsewhere). RAW files need no copy: the RAW itself is in the outbox.
- **Filesystem events** give instant pickup, with a scan every 30 s as backup. Files open in another program
  (or mid-sync) are retried. Photos wait while the place server can't be reached (see **Where** above).
- **HEIC** (iPhone) support is compiled in, so no codec packs are needed. exiftool and the towns database
  (`towns.sqlite`) live in `photo-stamp-files\` beside the exe, which the watcher ignores.
- **Place server:** set its address and this device's key in the dashboard (Settings > Place server). They're
  kept in `%LOCALAPPDATA%\photo-stamp\client.json`, readable only by you.

**Setup:** run `photo-stamp-setup.exe` from the release. It's a per-user installer (no admin rights) made with
NSIS (`installer/photo-stamp.nsi`):
- It asks for the drop folder (default `Pictures\Photo Stamp`) and installs to
  `%LOCALAPPDATA%\Programs\Photo Stamp`.
- It runs `photo-stamp.exe --configure <folder>`. That records the folder in `%LOCALAPPDATA%\photo-stamp\app.json`,
  adds the start-at-login entry (`HKCU\…\Run`, `photo-stamp.exe --background`) and registers for notifications.
- It starts photo-stamp and opens the dashboard.

Settings > Apps uninstalls it. That keeps the drop folder, photos and `%LOCALAPPDATA%\photo-stamp`. Running setup
again changes the drop folder. The setup isn't code-signed, so SmartScreen asks once ("More info", then "Run anyway").

**Running:** there's one program, `photo-stamp.exe`, and one copy runs per user.
- It watches the drop folder, serves the dashboard, and puts an icon by the clock. Click the icon for the
  dashboard; right-click it for Open drop folder, Pause/Resume and Quit. The icon turns gray while paused.
- Starting it again (the Start menu shortcut) opens the running copy's dashboard.
- Which folder it watches: `--root`, then `app.json`, then the exe's own folder (handy for a portable copy).

From a terminal:

```
photo-stamp.exe --quit                # quit the running copy (waits for the photos in hand)
photo-stamp.exe --background          # start without opening the dashboard (what login does)
photo-stamp.exe stamp <files>         # one-off
```

### Dashboard

photo-stamp serves its dashboard at `http://127.0.0.1:8765/` (`--port` or `PHOTO_STAMP_UI_PORT` to change it). It
opens when photo-stamp is started by hand, from the tray icon, or by clicking a notification.

- **Activity:**
  - the photo being stamped, as a small preview that changes from the original to the stamped result, with
    the steps (finding the place, drawing the caption, saving), and how many more are in progress;
  - the queue, with why each photo is waiting (still copying, map data unreachable, open in another program,
    paused);
  - a progress bar with photos done and time left, estimated separately for new spots (map data to download)
    and places you've been before;
  - a running table of everything processed, each with its caption, how long it took and a before/after view.
- **Not stamped:** photos moved unchanged, grouped by reason, and photos that couldn't be processed, with
  **Try again**.
- **Health:** map data release, indexes, cache and connection; the bundled place data; all-time totals; and the
  live log.
- **Settings:** outbox, quality, photos at a time, timing, offline mode, a notification when a batch is done,
  and start at login. Several photos are stamped at once, each all the way through (place lookup, drawing,
  saving). **Photos at a time** set to 0 (automatic) uses half the PC's processor cores, 2 to 6.
- **Pause / Resume** (new photos are still noticed and queued); quitting is in the tray icon's menu.

How it fits together: the page and its API (`src/api.rs`) come from photo-stamp itself, on 127.0.0.1. It only
answers requests addressed to `127.0.0.1`/`localhost`, and changes must come from its own page (`Origin`). A
restart or update hands the port over to the new copy, and the open page reconnects by itself.

Settings changed in the dashboard are kept in `%LOCALAPPDATA%\photo-stamp\settings-<folder>.json`; options on the
command line override them. History and previews (the most recent 2,000) are kept there too.

Build it (Docker only; produces `dist/photo-stamp-windows/` and `dist/photo-stamp-windows.zip`):

```sh
scripts/build-windows.sh
```

`docker/Dockerfile.windows` cross-compiles libde265, x265 and libheif as static libraries with mingw-w64,
then builds the exe against them. The script adds the portable exiftool release, verified by checksum. It also adds the
towns database, the newest one [dave/photo-stamp-places](https://forgejo.terrell.ltd/dave/photo-stamp-places)
builds monthly, downloaded with `scripts/fetch-places.sh`. That needs a Forgejo token that can read packages, in
`$TOKEN` or `~/.config/forgejo/token`.

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
so large copies are safe. If the place server can't be reached, a photo waits in the inbox and is retried
(1, 2, 4 min) before settling for what's known locally (a remembered answer, else the nearest town). Name
clashes become `photo (2).jpg`. Watch progress with `docker compose logs -f`.

Settings (in `compose.yaml` / environment):

| Variable              | Default          | Meaning                                                        |
|-----------------------|------------------|----------------------------------------------------------------|
| `PHOTOS_DIR`          | `$PWD/photos`    | Host folder backing the `photos` volume (absolute path)        |
| `PUID` / `PGID`       | `1000`           | Host user/group that output files belong to                    |
| `PHOTO_STAMP_POLL_SECS` | `5`            | Backup polling interval (filesystem events are also used)      |
| `PHOTO_STAMP_QUALITY` | `92`             | Quality for lossy re-encoding                                   |
| `PHOTO_STAMP_SERVER_URL` | –             | Your place server, e.g. `https://places.example.com`           |
| `PHOTO_STAMP_SERVER_KEY` | –             | This device's key (`photo-stamp-server device add …`)           |
| `PHOTO_STAMP_OFFLINE` | `false`          | Don't ask the place server: nearest town only                   |

Remembered answers live in the `cache` volume.

## Place server

`photo-stamp-server` holds the full place data and names points for the family's photo-stamp clients. It lives in
its own repo, [dave/photo-stamp-server](https://forgejo.terrell.ltd/dave/photo-stamp-server), with how to run it.
The `/v1` protocol (`proto/`) and the HTTP server (`http/`) are copied there: a change to the protocol has to be
made in both repos.

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
| `src/geo.rs`      | The client's place lookup interface                                   |
| `src/remote.rs`   | Asking the place server; towns database; client.json                  |
| `src/answers.rs`  | Remembered answers (`answers.sqlite`)                                 |
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
| `web/`            | The dashboard page, served by photo-stamp (`src/dashboard.rs`)      |
| `installer/`      | The Windows setup program (NSIS)                                     |
| `assets/`         | Windows icons and manifest (`scripts/make-icons.sh`), caption font   |
| `proto/`          | Place types, the `/v1` protocol, the places/towns database reader     |

## Limitations / next steps

- 16-bit PNG/TIFF are written back as 8-bit; WebP is re-encoded losslessly (bigger files); multi-page TIFFs
  keep only the first page.
- For HEIF, rotation comes from the container (as the spec requires); EXIF Orientation is ignored there.
- RAW output quality is that of the camera's embedded preview (usually full-size JPEG).
- The embedded EXIF thumbnail is removed rather than regenerated.
- Location ranking is heuristic; its weights are in photo-stamp-server's `server/src/gazetteer.rs`.

## License

Code: MIT. Embedded font: DejaVu Sans Bold — see `assets/DejaVuSans-LICENSE.txt`.
