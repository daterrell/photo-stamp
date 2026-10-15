#!/usr/bin/env python3
"""Derive the clients' small towns database from a full places database.

    python3 scripts/make-towns-db.py data/places.sqlite dist/towns.sqlite

Same schema as places.sqlite, keeping towns of 5,000+ people and every region and country (no
landmarks or fame). photo-stamp uses it to show where waiting photos are from, and to stamp them
when the place server can't be reached. photo-stamp-server builds the same file during `import`
(in its own repo, dave/photo-stamp-server) and serves it to clients.
"""
import sqlite3
import sys
from pathlib import Path

TOWN_MIN_POPULATION = 5000

SCHEMA = """
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE country(code TEXT PRIMARY KEY, name TEXT NOT NULL, local TEXT);
CREATE TABLE region(country TEXT NOT NULL, code TEXT NOT NULL, name TEXT NOT NULL, local TEXT,
                    PRIMARY KEY(country, code)) WITHOUT ROWID;
CREATE TABLE city(id INTEGER PRIMARY KEY, name TEXT NOT NULL, local TEXT, country TEXT NOT NULL,
                  region TEXT, population INTEGER NOT NULL, lat REAL NOT NULL, lon REAL NOT NULL);
CREATE VIRTUAL TABLE city_idx USING rtree(id, min_lat, max_lat, min_lon, max_lon);
CREATE TABLE fame(qid INTEGER PRIMARY KEY, sitelinks INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE landmark(qid INTEGER PRIMARY KEY, name TEXT, local TEXT, country TEXT,
                      sitelinks INTEGER NOT NULL, lat REAL NOT NULL, lon REAL NOT NULL);
CREATE VIRTUAL TABLE landmark_idx USING rtree(id, min_lat, max_lat, min_lon, max_lon);
"""


def main():
    full, out = Path(sys.argv[1]), Path(sys.argv[2])
    tmp = out.with_suffix(".tmp")
    tmp.unlink(missing_ok=True)
    db = sqlite3.connect(tmp)
    db.executescript(SCHEMA)
    db.execute("ATTACH DATABASE ? AS f", (str(full),))
    db.executescript(f"""
        INSERT INTO meta SELECT * FROM f.meta;
        INSERT OR REPLACE INTO meta VALUES('kind', 'towns');
        INSERT INTO country SELECT * FROM f.country;
        INSERT INTO region SELECT * FROM f.region;
        INSERT INTO city SELECT * FROM f.city WHERE population >= {TOWN_MIN_POPULATION};
        INSERT INTO city_idx SELECT id, lat, lat, lon, lon FROM city;
    """)
    db.commit()
    db.execute("DETACH DATABASE f")
    n = db.execute("SELECT count(*) FROM city").fetchone()[0]
    db.execute("VACUUM")
    db.close()
    tmp.replace(out)
    print(f"wrote {out} ({out.stat().st_size / 2**20:.1f} MiB): {n} towns", file=sys.stderr)


if __name__ == "__main__":
    main()
