photo-stamp for Windows
=======================

Burns the place and local time each photo was taken into the photo itself, using the photo's
own metadata (date and GPS position).

SETUP

1. Make an empty folder to drop photos into, e.g. Pictures\Stamp. Copy photo-stamp.exe AND the
   photo-stamp-files folder into it.
2. Double-click photo-stamp.exe. Nothing appears on screen: it runs in the background.
3. To have it start whenever you log in, open a terminal in that folder and run:
       photo-stamp.exe --install-startup

WHAT IT DOES

- Every photo you drop into the folder, or any subfolder, is stamped with the place and time in its
  bottom-left corner and MOVED to the "outbox" folder, in the same subfolders:
      trip\day 1\IMG_1.jpg  ->  outbox\trip\day 1\IMG_1.jpg
  It keeps its name and dates. The original is not kept, so keep a copy elsewhere if you want it.
- It doesn't matter how old a photo's file dates are. A hidden "photo-stamp" tag in the photo
  marks it as stamped, so nothing is ever stamped twice.
- Photos that are already stamped, or have no date or location in their metadata, are moved to
  the outbox unchanged.
- JPEG, PNG, WebP, TIFF and HEIC (iPhone) photos are stamped. Camera RAW files (DNG, CR3, NEF,
  ARW, ...) are never modified: they're moved with a stamped copy beside them as <name>.jpg.
- If the outbox already has a photo by that name, the new one becomes "name (2).jpg".
- Other files (videos, documents, ...) are left where they are, and so are the folders.
- The outbox and photo-stamp-files folders are ignored. To use a different outbox, start it with
  --outbox "D:\Stamped".

COMMANDS (from a terminal in the folder)

    photo-stamp.exe --install-startup     start at login (and start now)
    photo-stamp.exe --uninstall-startup   stop starting at login
    photo-stamp.exe --stop                stop the running copy
    photo-stamp.exe stamp <files>         stamp specific photos once
    photo-stamp.exe --help                everything else

LOG

What it did, and why anything was left alone, is written to
    %LOCALAPPDATA%\photo-stamp\photo-stamp.log

PLACE DATA

Towns, regions and countries come from the bundled photo-stamp-files\places.sqlite and work offline.
Venues (golf courses, parks, stadiums, towers...) and neighborhoods are downloaded on demand from
Overture Maps, only for the area around each photo, and cached in %LOCALAPPDATA%\photo-stamp\overture
(400 MB at most). The first photo after each monthly Overture release takes a minute longer while
its index downloads (about 70 MB, done in the background at startup). Each new spot then downloads
about 10-20 MB once; spots you've been to before need no download.

Data: GeoNames (CC BY 4.0, geonames.org); Wikidata (CC0); Overture Maps (overturemaps.org; includes
OpenStreetMap data (c) OpenStreetMap contributors, ODbL). Includes ExifTool by Phil Harvey.
