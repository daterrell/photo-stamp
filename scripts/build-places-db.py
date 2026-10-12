#!/usr/bin/env python3
"""Build photo-stamp's bundled places database: one SQLite file with spatial (R*Tree) indexes.

  city / region / country   GeoNames cities500 + admin1 + countryInfo, with English and local names
                            from alternateNamesV2 (what Immich uses for city/state/country).
  fame                      Wikidata items that have coordinates and 5+ Wikipedia sitelinks:
                            qid -> sitelink count. Ranks Overture features that carry a Wikidata id.
  landmark                  The notable ones among those (not towns or admin areas), with English and
                            local names, so famous landmarks can be named fully offline.

Uses only the Python standard library. Downloads are reused for 30 days (default ~/.cache/photo-stamp-dev/places-src).

  scripts/build-places-db.py OUT.sqlite
  scripts/build-places-db.py --near 36.56,-121.94 --near 48.858,2.294 --radius-km 30 \
      --fame-from tests/fixtures/overture.json tests/fixtures/places.sqlite
"""

import argparse
import re
import csv
import io
import math
import os
import sqlite3
import sys
import time
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path

GEONAMES = "https://download.geonames.org/export/dump/"
QLEVER = "https://qlever.dev/api/wikidata"
USER_AGENT = "photo-stamp-build (https://github.com/daterrell/photo-stamp)"
MIN_FAME_SITELINKS = 5
MIN_LANDMARK_SITELINKS = 15

csv.field_size_limit(sys.maxsize)


def log(*a):
    print(*a, file=sys.stderr, flush=True)


def fetch(url: str, dest: Path, data: bytes | None = None, max_age_days: float = 30) -> Path:
    """Download `url` to `dest` unless a fresh enough copy is already there."""
    if dest.exists() and time.time() - dest.stat().st_mtime < max_age_days * 86400:
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    log(f"downloading {url if not data else 'QLever query'} -> {dest.name}")
    headers = {"User-Agent": USER_AGENT}
    if data:
        headers.update({"Accept": "text/tab-separated-values", "Content-Type": "application/x-www-form-urlencoded"})
    req = urllib.request.Request(url, data=data, headers=headers)
    tmp = dest.with_suffix(dest.suffix + ".part")
    for attempt in range(6):
        try:
            with urllib.request.urlopen(req, timeout=600) as r, open(tmp, "wb") as f:
                while chunk := r.read(1 << 20):
                    f.write(chunk)
            tmp.replace(dest)
            return dest
        except Exception as e:  # noqa: BLE001 - retry any network failure
            # Public services rate-limit; honor Retry-After, else back off exponentially.
            retry_after = getattr(e, "headers", None) and e.headers.get("Retry-After")
            wait = int(retry_after) if retry_after and retry_after.isdigit() else 30 * 2**attempt
            log(f"  attempt {attempt + 1} failed: {e}; retrying in {wait}s")
            time.sleep(wait)
    raise SystemExit(f"couldn't download {url}")


def zipped_lines(path: Path, member: str):
    with zipfile.ZipFile(path) as z, z.open(member) as f:
        yield from io.TextIOWrapper(f, encoding="utf-8")


def qlever(query: str, dest: Path) -> list[list[str]]:
    # QLever quirk: compare against a VALUES variable, not a literal (a literal matches nothing).
    fetch(QLEVER, dest, data=urllib.parse.urlencode({"query": query}).encode())
    with open(dest, encoding="utf-8") as f:
        rows = [line.rstrip("\n").split("\t") for line in f]
    return rows[1:]  # header


def literal(v: str) -> str:
    """'"Eiffel Tower"@en' -> 'Eiffel Tower'."""
    if v.startswith('"'):
        return v[1 : v.rindex('"')].replace('\\"', '"').replace("\\\\", "\\")
    return v


def qid(v: str) -> int:
    return int(v.rsplit("Q", 1)[1].rstrip(">"))


def number(v: str) -> int:
    return int(literal(v).split("^^")[0])


def point(v: str) -> tuple[float, float] | None:
    """'POINT(2.2945 48.8584)' or '"Point(2.2945 48.8584)"^^<…wktLiteral>' -> (lat, lon)."""
    s = literal(v).strip()
    if not s.upper().startswith("POINT(") or not s.endswith(")"):
        return None
    try:
        lon, lat = map(float, s[6:-1].split())
    except ValueError:
        return None
    return lat, lon


# ---------------------------------------------------------------- GeoNames

def geonames(cache: Path):
    countries, regions, cities = {}, {}, {}
    country_lang = {}
    info = fetch(GEONAMES + "countryInfo.txt", cache / "countryInfo.txt")
    for line in open(info, encoding="utf-8"):
        if line.startswith("#"):
            continue
        f = line.rstrip("\n").split("\t")
        countries[f[0]] = {"name": f[4], "geonameid": int(f[16]) if f[16] else None}
        langs = [l.split("-")[0] for l in f[15].split(",") if l]
        country_lang[f[0]] = langs[0] if langs else None

    admin1 = fetch(GEONAMES + "admin1CodesASCII.txt", cache / "admin1CodesASCII.txt")
    for line in open(admin1, encoding="utf-8"):
        code, name, _ascii, gid = line.rstrip("\n").split("\t")
        cc, ac = code.split(".", 1)
        regions[(cc, ac)] = {"name": name, "geonameid": int(gid)}

    zpath = fetch(GEONAMES + "cities500.zip", cache / "cities500.zip")
    for line in zipped_lines(zpath, "cities500.txt"):
        f = line.rstrip("\n").split("\t")
        cities[int(f[0])] = {
            "name": f[1], "lat": float(f[4]), "lon": float(f[5]), "country": f[8],
            "region": f[10], "population": int(f[14] or 0),
        }
    log(f"geonames: {len(cities)} places, {len(regions)} regions, {len(countries)} countries")

    # English and local-language names, from the (large) alternate names file.
    wanted = {gid: c["country"] for gid, c in cities.items()}
    for (cc, _), r in regions.items():
        wanted[r["geonameid"]] = cc
    for cc, c in countries.items():
        if c["geonameid"]:
            wanted[c["geonameid"]] = cc
    best: dict[tuple[int, str], tuple[int, str]] = {}  # (gid, "en"|"local") -> (rank, name)
    zpath = fetch(GEONAMES + "alternateNamesV2.zip", cache / "alternateNamesV2.zip")
    for line in zipped_lines(zpath, "alternateNamesV2.txt"):
        f = line.rstrip("\n").split("\t")
        gid = int(f[1])
        cc = wanted.get(gid)
        if cc is None or f[6] == "1" or f[7] == "1":  # colloquial / historic
            continue
        lang = f[2]
        which = "en" if lang == "en" else "local" if lang and lang == country_lang.get(cc) else None
        if not which:
            continue
        rank = (2 if f[4] == "1" else 0) + (1 if f[5] == "1" else 0)  # preferred, short
        key = (gid, which)
        if key not in best or rank > best[key][0]:
            best[key] = (rank, f[3])
    names = {k: v[1] for k, v in best.items()}
    log(f"geonames: {sum(1 for k in names if k[1] == 'en')} English, {sum(1 for k in names if k[1] == 'local')} local names")
    return countries, regions, cities, names, country_lang


# ---------------------------------------------------------------- Wikidata

FAME_QUERY = f"""
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX wikibase: <http://wikiba.se/ontology#>
SELECT ?item ?links WHERE {{
  VALUES ?min {{ {MIN_FAME_SITELINKS} }}
  ?item wdt:P625 ?coord ; wikibase:sitelinks ?links .
  FILTER(?links >= ?min)
}}"""

# Notable places that aren't towns, administrative areas or countries, with their English name and the
# name in their country's official language. One combined query is too heavy for the public endpoint
# (it answers 429), so it's split into light ones joined below.
_PREFIXES = """
PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX wikibase: <http://wikiba.se/ontology#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
"""
_NOTABLE = (f"VALUES ?min {{ {MIN_LANDMARK_SITELINKS} }} "
            "?item wdt:P625 ?coord ; wikibase:sitelinks ?links . FILTER(?links >= ?min)")
LANDMARK_QUERIES = {
    "coords": f"{_PREFIXES} SELECT ?item ?coord ?links WHERE {{ {_NOTABLE} }}",
    # Each item's direct types, and which of those types are settlements / administrative areas /
    # countries. Wikidata's class tree has odd links (e.g. "observation tower" sits under
    # "administrative territorial entity"), so an item only counts as a town or area if at least half
    # its types do: Paris has fifteen such types, the Eiffel Tower one of three.
    "types": f"{_PREFIXES} SELECT ?item ?type WHERE {{ {_NOTABLE} ?item wdt:P31 ?type }}",
    # Roots: human settlement, administrative entity, country, plus things that have coordinates but
    # aren't somewhere you stand: continent, ocean, language, event, organization, historical country.
    "area_types": f"""{_PREFIXES} SELECT DISTINCT ?type WHERE {{ {_NOTABLE} ?item wdt:P31 ?type .
        VALUES ?root {{ wd:Q486972 wd:Q56061 wd:Q6256 wd:Q5107 wd:Q9430 wd:Q34770 wd:Q1190554 wd:Q43229
                       wd:Q3024240 }}
        ?type wdt:P279* ?root }}""",
    # Craters and the like on the Moon or Mars have "coordinates" too.
    "offworld": f"""{_PREFIXES} SELECT DISTINCT ?item WHERE {{ {_NOTABLE} ?item wdt:P376 ?body
        FILTER(?body != wd:Q2) }}""",
    "en": f"""{_PREFIXES} SELECT ?item ?en WHERE {{ {_NOTABLE} ?item rdfs:label ?en FILTER(LANG(?en) = "en") }}""",
    "country": f"{_PREFIXES} SELECT ?item ?iso WHERE {{ {_NOTABLE} ?item wdt:P17/wdt:P297 ?iso }}",
}


def label_query(lang: str) -> str:
    """Labels in one language (local names are fetched one language at a time: a single query joining
    each item to its country's language is too heavy for the public endpoint)."""
    return f"""{_PREFIXES} SELECT ?item ?l WHERE {{ {_NOTABLE} ?item rdfs:label ?l FILTER(LANG(?l) = "{lang}") }}"""


def wikidata(cache: Path, country_lang: dict):
    fame = {}
    for row in qlever(FAME_QUERY, cache / "wikidata-fame.tsv"):
        fame[qid(row[0])] = number(row[1])
    log(f"wikidata: {len(fame)} items with coordinates and {MIN_FAME_SITELINKS}+ sitelinks")

    parts = {}
    for name, query in LANDMARK_QUERIES.items():
        dest = cache / f"wikidata-landmarks-{name}.tsv"
        cached = dest.exists()
        parts[name] = qlever(query, dest)
        if not cached:
            time.sleep(5)  # be gentle with the public endpoint
    area_types = {qid(r[0]) for r in parts["area_types"]}
    type_counts: dict[int, list[int]] = {}  # item -> [area types, all types]
    for r in parts["types"]:
        if len(r) > 1 and "/entity/Q" in r[1]:
            counts = type_counts.setdefault(qid(r[0]), [0, 0])
            counts[0] += qid(r[1]) in area_types
            counts[1] += 1
    excluded = {q for q, (area, total) in type_counts.items() if area * 2 >= total}
    excluded |= {qid(r[0]) for r in parts["offworld"]}

    def first_value(rows):
        out = {}
        for r in rows:
            if len(r) > 1 and r[1]:
                out.setdefault(qid(r[0]), literal(r[1]))
        return out

    en, country = first_value(parts["en"]), first_value(parts["country"])

    # Local names: the label in the item's country's main language (from GeoNames' country table,
    # translated to the codes Wikidata labels use).
    aliases = {"cmn": "zh", "no": "nb"}
    wanted: dict[str, set[int]] = {}
    for row in parts["coords"]:
        q = qid(row[0])
        lang = country_lang.get(country.get(q))
        lang = aliases.get(lang, lang)
        if q not in excluded and lang and lang != "en":
            wanted.setdefault(lang, set()).add(q)
    local: dict[int, str] = {}
    for lang, items in sorted(wanted.items()):
        if len(items) < 20:
            continue
        dest = cache / f"wikidata-landmarks-label-{lang}.tsv"
        cached = dest.exists()
        for q, label in first_value(qlever(label_query(lang), dest)).items():
            if q in items:
                local[q] = label
        if not cached:
            time.sleep(3)
    log(f"wikidata: local names in {sum(1 for i in wanted.values() if len(i) >= 20)} languages for {len(local)} items")

    landmarks = {}
    for row in parts["coords"]:
        q = qid(row[0])
        pt = point(row[1])
        if q in excluded or q in landmarks or pt is None or not (en.get(q) or local.get(q)):
            continue
        local_name = local.get(q)
        landmarks[q] = {"lat": pt[0], "lon": pt[1], "sitelinks": number(row[2]), "country": country.get(q),
                        "en": en.get(q), "local": local_name if local_name != en.get(q) else None}
    log(f"wikidata: {len(landmarks)} landmarks with {MIN_LANDMARK_SITELINKS}+ sitelinks")
    return fame, landmarks


# ---------------------------------------------------------------- output

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


def km(a, b):
    la1, la2 = math.radians(a[0]), math.radians(b[0])
    h = math.sin((la2 - la1) / 2) ** 2 + math.cos(la1) * math.cos(la2) * math.sin(math.radians(b[1] - a[1]) / 2) ** 2
    return 2 * 6371 * math.asin(math.sqrt(h))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("out", type=Path)
    ap.add_argument("--cache", type=Path, default=Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
                    / "photo-stamp-dev" / "places-src")
    ap.add_argument("--near", action="append", default=[], help="lat,lon: only keep places near here (fixtures)")
    ap.add_argument("--radius-km", type=float, default=30)
    ap.add_argument("--fame-from", action="append", type=Path, default=[],
                    help="(with --near) also keep fame for Wikidata ids mentioned in this file")
    args = ap.parse_args()
    near = [tuple(map(float, n.split(","))) for n in args.near]
    keep = (lambda lat, lon: any(km((lat, lon), p) <= args.radius_km for p in near)) if near else (lambda *_: True)

    countries, regions, cities, names, country_lang = geonames(args.cache)
    fame, landmarks = wikidata(args.cache, country_lang)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    tmp = args.out.with_suffix(".tmp")
    tmp.unlink(missing_ok=True)
    db = sqlite3.connect(tmp)
    db.executescript(SCHEMA)
    db.executemany("INSERT INTO meta VALUES(?, ?)", [
        ("built", time.strftime("%Y-%m-%d")),
        ("sources", "GeoNames (CC BY 4.0, geonames.org); Wikidata (CC0) via QLever"),
    ])

    kept_cities = {gid: c for gid, c in cities.items() if keep(c["lat"], c["lon"])}
    db.executemany("INSERT INTO city VALUES(?,?,?,?,?,?,?,?)", (
        (gid, names.get((gid, "en"), c["name"]), names.get((gid, "local")), c["country"], c["region"],
         c["population"], c["lat"], c["lon"])
        for gid, c in kept_cities.items()))
    db.executemany("INSERT INTO city_idx VALUES(?,?,?,?,?)",
                   ((gid, c["lat"], c["lat"], c["lon"], c["lon"]) for gid, c in kept_cities.items()))
    used_regions = {(c["country"], c["region"]) for c in kept_cities.values()}
    db.executemany("INSERT INTO region VALUES(?,?,?,?)", (
        (cc, ac, names.get((r["geonameid"], "en"), r["name"]), names.get((r["geonameid"], "local")))
        for (cc, ac), r in regions.items() if not near or (cc, ac) in used_regions))
    db.executemany("INSERT INTO country VALUES(?,?,?)", (
        (cc, c["name"], names.get((c["geonameid"], "local")) if c["geonameid"] else None)
        for cc, c in countries.items()))

    kept_landmarks = {q: l for q, l in landmarks.items() if keep(l["lat"], l["lon"])}
    db.executemany("INSERT INTO landmark VALUES(?,?,?,?,?,?,?)", (
        (q, l["en"], l["local"], l["country"], l["sitelinks"], l["lat"], l["lon"]) for q, l in kept_landmarks.items()))
    db.executemany("INSERT INTO landmark_idx VALUES(?,?,?,?,?)",
                   ((q, l["lat"], l["lat"], l["lon"], l["lon"]) for q, l in kept_landmarks.items()))
    # Fixtures keep the fame table small: nearby landmarks plus any Wikidata ids found in --fame-from files.
    extra = set()
    for path in args.fame_from:
        extra |= {int(m) for m in re.findall(r"\bQ(\d+)\b", path.read_text(encoding="utf-8"))}
    fame_rows = fame.items() if not near else ((q, n) for q, n in fame.items() if q in kept_landmarks or q in extra)
    db.executemany("INSERT INTO fame VALUES(?,?)", fame_rows)
    db.commit()
    db.execute("VACUUM")
    db.close()
    tmp.replace(args.out)
    log(f"wrote {args.out} ({args.out.stat().st_size / 2**20:.1f} MiB): {len(kept_cities)} places, "
        f"{len(kept_landmarks)} landmarks")


if __name__ == "__main__":
    main()
