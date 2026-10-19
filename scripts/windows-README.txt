Photo Stamp for Windows
=======================

Burns the place and local time each photo was taken into the photo itself, using the photo's
own metadata (date and GPS position).

SETUP

Run photo-stamp-setup.exe (from the release page). It asks which folder you'll drop photos into
(Pictures\Photo Stamp unless you choose another), installs Photo Stamp for you (no admin rights
needed), starts it, and opens the dashboard. It starts by itself whenever you log in.

If Windows says "Windows protected your PC", click "More info", then "Run anyway": Photo Stamp
isn't signed with a paid certificate, so Windows doesn't recognize it yet.

To change the drop folder, run the setup program again. To remove Photo Stamp: Settings > Apps >
Photo Stamp > Uninstall. Your photos, the drop folder and Photo Stamp's settings are kept.

THE ICON BY THE CLOCK

While Photo Stamp is running its icon sits by the clock (it may be under the ^ arrow); it turns
gray while paused. Click it to open the dashboard. Right-click it to open the drop folder, pause
or resume, or quit (it starts again when you next log in, or from Photo Stamp in the Start menu).

WHAT IT DOES

- Every photo you drop into the folder, or any subfolder, is stamped with the place and time in its
  bottom-left corner and MOVED to the "outbox" folder, in the same subfolders:
      trip\day 1\IMG_1.jpg  ->  outbox\trip\day 1\IMG_1.jpg
  It keeps its name and dates. The original is kept in the "originals" folder (same subfolders).
- It doesn't matter how old a photo's file dates are. A hidden "photo-stamp" tag in the photo
  marks it as stamped, so nothing is ever stamped twice.
- Photos that are already stamped, or have no date or location in their metadata, are moved to
  the outbox unchanged. The dashboard says what each was missing, and you can stamp it by hand
  there with your own words.
- JPEG, PNG, WebP, TIFF and HEIC (iPhone) photos are stamped. Camera RAW files (DNG, CR3, NEF,
  ARW, ...) are never modified: they're moved with a stamped copy beside them as <name>.jpg.
- If the outbox already has a photo by that name, the new one becomes "name (2).jpg".
- Other files (videos, documents, ...) are left where they are, and so are the folders.
- The outbox and originals folders are ignored. Another outbox or originals folder can be chosen in
  the dashboard: Settings.

DASHBOARD

Click the icon by the clock (or Photo Stamp in the Start menu) to see what Photo Stamp is doing, in
your browser (http://127.0.0.1:8765/): the photos being stamped, what's waiting, progress and time
left, everything processed, photos that weren't stamped, and the log. You can pause and resume
there, and change its settings. Only this PC can open it.

You can also drag photos, or whole folders of them, onto the dashboard: they're copied into the
drop folder and stamped. Click a processed photo to see it larger; the arrow keys step through
them, and "Edit stamp" changes what's written on it.

UPDATES

New versions come through your place server. photo-stamp checks every few hours, downloads a
newer one in the background, and the dashboard offers "Install and restart". If the new version
doesn't start, the old one is put back automatically. Settings > Updates chooses releases only
(the default) or every build.

COMMANDS (from a terminal in Photo Stamp's program folder, %LOCALAPPDATA%\Programs\Photo Stamp)

    photo-stamp.exe --quit                quit the running copy
    photo-stamp.exe stamp <files>         stamp specific photos once
    photo-stamp.exe --help                everything else

LOG

What it did, and why anything was left alone, is written to
    %LOCALAPPDATA%\photo-stamp\photo-stamp.log

PLACE DATA

Place names come from your family's place server (photo-stamp-server), which has the full map
data: the venue you're in (golf course, park, stadium, tower...), famous landmarks nearby, the
neighborhood, town, region and country. Set it up in the dashboard: Settings > Place server, with
the server's address and this device's key. Each photo sends just its position; answers are
remembered on this PC, so places you've been before work offline.

When the place server can't be reached, photos wait (the dashboard shows where each is from and
when it'll be stamped anyway). After your wait time (72 hours unless you change it), or when you
choose "Stamp now", they're stamped with what this PC knows: a remembered answer, otherwise the
nearest town from photo-stamp-files\towns.sqlite. Without a place server, photos are named by the
nearest town.

The original of every stamped photo is kept in the "originals" folder, in the same subfolders.

Data: GeoNames (CC BY 4.0, geonames.org); Wikidata (CC0); Overture Maps (overturemaps.org; includes
OpenStreetMap data (c) OpenStreetMap contributors, ODbL). Includes ExifTool by Phil Harvey.
