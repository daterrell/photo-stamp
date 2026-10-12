photo-stamp for Windows
=======================

Burns the place and local time each photo was taken into the photo itself, using the photo's
own metadata (date and GPS position), with place names from OpenStreetMap.

SETUP

1. Copy photo-stamp.exe AND the photo-stamp-files folder into the folder you want stamped,
   e.g. your Pictures folder. Subfolders are included.
2. Double-click photo-stamp.exe. Nothing appears on screen: it runs in the background.
3. To have it start whenever you log in, open a terminal in that folder and run:
       photo-stamp.exe --install-startup

WHAT IT DOES

- Every NEW photo that appears in the folder or its subfolders is replaced with the same photo
  with the place and time stamped in its bottom-left corner. The file keeps its name and dates.
  There is no backup copy, so keep originals elsewhere if you want them.
- Photos that were already there when you started it are left alone. Photos added while it
  wasn't running are caught when it next starts.
- JPEG, PNG, WebP, TIFF and HEIC (iPhone) photos are stamped in place. Camera RAW files (DNG, CR3,
  NEF, ARW, ...) are never modified: a stamped copy is saved beside them as <name>.jpg.
- Photos with no date or location in their metadata are left untouched.
- The photo-stamp-files folder is ignored.

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
