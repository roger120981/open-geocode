"""Write a `geocode bench` answer key of street crossings from an OpenStreetMap extract.

    python scripts/fixtures/crossings.py --pbf <extract.osm.pbf> --relation <id> --work <dir>

Needs the `osmium` command line tool (osmium-tool 1.16 or later).

A crossing is a node that two differently named street ways share, inside the
boundary relation. The truth is that node's own position, read from the extract
and never from og. Ways tagged bridge, tunnel or a layer other than 0 are
dropped: a street that passes over another does not meet it. Two names that
share more than one node are dropped too, as one query would have two answers.
Every eligible node is ranked by sha256('<seed>:<node id>') and the lowest
SAMPLE are kept: a hash-based draw, so it is uniform over the city and does not
depend on file order. Each is written three ways, see FORMS.
"""

import argparse
import hashlib
import json
import re
import subprocess
import sys
from collections import Counter, defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
SEED = 2026
SAMPLE = 80
# Highway values the builder does not turn into streets (src/builder/osm/street.rs).
NOT_STREETS = {"abandoned", "bus_stop", "corridor", "platform", "proposed", "rest_area", "services"}
CONNECTOR = re.compile(r"(?:^|\s)(?:&|and|at|@)(?:\s|$)", re.IGNORECASE)
ABBREVIATIONS = {
    "avenue": "Ave",
    "boulevard": "Blvd",
    "circle": "Cir",
    "court": "Ct",
    "drive": "Dr",
    "highway": "Hwy",
    "lane": "Ln",
    "parkway": "Pkwy",
    "place": "Pl",
    "road": "Rd",
    "street": "St",
    "terrace": "Ter",
    "trail": "Trl",
    "north": "N",
    "south": "S",
    "east": "E",
    "west": "W",
}
SOURCE = {
    "register": "Street crossings of OpenStreetMap",
    "country": "US",
    "publisher": "OpenStreetMap contributors",
    "licence": "Open Database License 1.0 (ODbL), https://opendatacommons.org/licenses/odbl/1-0/",
    "attribution": "(c) OpenStreetMap contributors",
    "url": "https://www.openstreetmap.org",
    "notes": (
        "A node shared by two street ways of different names, inside the city boundary relation; "
        "ways tagged bridge, tunnel or a layer other than 0 are dropped, so are names that hold a connector "
        "word (&, and, at, @) and name pairs that share more than one node; the truth is the node's position "
        "in the extract; highway values the builder does not turn into streets are not streets"
    ),
}
FORMS = {
    "full": "both names as OpenStreetMap writes them, joined by 'and', then the city and the state",
    "messy": "road type and direction words abbreviated, joined by '&', then the city alone",
    "reversed": "as full, the two streets in the other order",
}


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def write(path, meta, cases):
    # Same layout as registers.py; that module needs duckdb and pyproj, this
    # script needs neither.
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write('{\n "meta": ')
        handle.write(json.dumps(meta, ensure_ascii=False, indent=1).replace("\n", "\n "))
        handle.write(',\n "cases": [\n')
        handle.write(",\n".join("  " + json.dumps(case, ensure_ascii=False) for case in cases))
        handle.write("\n ]\n}\n")


def osmium(*args):
    result = subprocess.run(["osmium", *map(str, args)], capture_output=True, text=True)
    if result.returncode:
        sys.exit(f"osmium {' '.join(map(str, args))} failed:\n{result.stderr}")
    return result.stdout


def unescape(text):
    return re.sub(r"%([0-9a-f]+)%", lambda found: chr(int(found.group(1), 16)), text)


def tags_of(field):
    tags = {}
    for pair in field[1:].split(","):
        key, _, value = pair.partition("=")
        if key:
            tags[unescape(key)] = unescape(value)
    return tags


def boundary(pbf, relation, work):
    """The ring list of the relation's outline, and its bounding box."""
    only = work / "boundary.osm.pbf"
    osmium("getid", "-r", pbf, f"r{relation}", "-o", only, "--overwrite")
    out = work / "boundary.geojson"
    osmium("export", only, "-f", "geojson", "-o", out, "--overwrite")
    for feature in json.loads(out.read_text())["features"]:
        geometry = feature["geometry"]
        if geometry["type"] == "MultiPolygon":
            polygons = geometry["coordinates"]
            points = [p for polygon in polygons for ring in polygon for p in ring]
            box = (min(p[0] for p in points), min(p[1] for p in points), max(p[0] for p in points), max(p[1] for p in points))
            return polygons, box
    sys.exit(f"relation {relation} has no polygon outline")


def inside(lon, lat, polygons):
    """Even-odd ray cast: a ring is a shell or a hole by how many contain the point."""
    hits = 0
    for polygon in polygons:
        for ring in polygon:
            crossed = False
            for (x1, y1), (x2, y2) in zip(ring, ring[1:]):
                if (y1 > lat) != (y2 > lat) and lon < (x2 - x1) * (lat - y1) / (y2 - y1) + x1:
                    crossed = not crossed
            hits += crossed
    return hits % 2 == 1


def streets(pbf, box, work):
    """Nodes by id and named street ways, from the extract's highways in the box."""
    west, south, east, north = box
    pad = 0.02
    cut = work / "box.osm.pbf"
    osmium("extract", "-b", f"{west - pad},{south - pad},{east + pad},{north + pad}", "-s", "complete_ways", pbf, "-o", cut, "--overwrite")
    highways = work / "highways.osm.pbf"
    osmium("tags-filter", cut, "w/highway", "-o", highways, "--overwrite")
    nodes, ways = {}, {}
    for line in osmium("cat", highways, "-f", "opl").splitlines():
        fields = line.split(" ")
        if line[0] == "n":
            x = next(f for f in fields if f[0] == "x")
            y = next(f for f in fields if f[0] == "y")
            nodes[int(fields[0][1:])] = (float(x[1:]), float(y[1:]))
        elif line[0] == "w":
            tags = tags_of(next(f for f in fields if f[0] == "T"))
            refs = next(f for f in fields if f[0] == "N")[1:]
            ways[int(fields[0][1:])] = (tags, [int(ref[1:]) for ref in refs.split(",")])
    return nodes, ways


def grade_separated(tags):
    return (
        tags.get("bridge", "no") != "no"
        or tags.get("tunnel", "no") != "no"
        or tags.get("layer", "0") != "0"
    )


def fold(name):
    return " ".join(re.sub(r"[^\w]+", " ", name.lower()).split())


def messy(name):
    return " ".join(ABBREVIATIONS.get(word.lower(), word) for word in name.split())


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--pbf", required=True, type=Path)
    parser.add_argument("--relation", required=True, type=int, help="OpenStreetMap relation of the city boundary")
    parser.add_argument("--city", default="Naples")
    parser.add_argument("--state", default="FL")
    parser.add_argument("--work", required=True, type=Path, help="where the cut extracts are kept")
    parser.add_argument("--out", type=Path, default=REPO / "fixtures" / "queries" / "naples-crossings.json")
    args = parser.parse_args()
    args.work.mkdir(parents=True, exist_ok=True)

    polygons, box = boundary(args.pbf, args.relation, args.work)
    nodes, ways = streets(args.pbf, box, args.work)

    dropped = Counter()
    # node -> street ways through it, by name
    through = defaultdict(dict)
    for way_id, (tags, refs) in sorted(ways.items()):
        name = tags.get("name", "").strip()
        if tags["highway"] in NOT_STREETS or not name:
            continue
        if grade_separated(tags):
            dropped["ways with bridge, tunnel or layer"] += 1
            continue
        for ref in refs:
            through[ref][fold(name)] = name

    meetings = defaultdict(set)
    for ref, names in through.items():
        if len(names) < 2 or ref not in nodes:
            continue
        ordered = sorted(names)
        for index, one in enumerate(ordered):
            for other in ordered[index + 1 :]:
                meetings[(one, other)].add(ref)

    eligible = []
    for (one, other), refs in sorted(meetings.items()):
        if len(refs) > 1:
            dropped["name pairs sharing several nodes"] += 1
            continue
        (ref,) = refs
        lon, lat = nodes[ref]
        if not inside(lon, lat, polygons):
            dropped["name pairs outside the boundary"] += 1
            continue
        first, second = through[ref][one], through[ref][other]
        if CONNECTOR.search(first) or CONNECTOR.search(second):
            dropped["name pairs with a connector word"] += 1
            continue
        eligible.append((ref, first, second, lat, lon))

    ranked = sorted(eligible, key=lambda row: hashlib.sha256(f"{SEED}:{row[0]}".encode()).hexdigest())
    kept = sorted(ranked[:SAMPLE])
    if len(kept) < SAMPLE:
        sys.exit(f"only {len(eligible)} eligible crossings, {SAMPLE} wanted")

    area = f"{args.city}, {args.state}"
    cases = []
    for ref, first, second, lat, lon in kept:
        queries = {
            "full": f"{first} and {second}, {area}",
            "messy": f"{messy(first)} & {messy(second)}, {args.city}",
            "reversed": f"{second} and {first}, {area}",
        }
        for form, query in queries.items():
            cases.append(
                {
                    "name": f"n{ref}:{form}",
                    "q": query,
                    "form": form,
                    "cohort": "crossing",
                    "expect_lat": round(lat, 7),
                    "expect_lon": round(lon, 7),
                }
            )

    stamp = osmium("fileinfo", "-g", "header.option.osmosis_replication_timestamp", args.pbf).strip()
    meta = {
        **SOURCE,
        "release": f"{args.pbf.name}, replication timestamp {stamp or 'not stated'}",
        "files": {args.pbf.name: sha256(args.pbf)},
        "boundary": f"OpenStreetMap relation {args.relation}",
        "rows": len(meetings),
        "dropped": dict(sorted(dropped.items())),
        "seed": SEED,
        "sample": SAMPLE,
        "draw": f"lowest sha256('{SEED}:' || node id) among eligible nodes",
        "forms": FORMS,
        "truth": "the shared node's position in the extract, never og's",
        "rebuild": (
            f"python scripts/fixtures/crossings.py --pbf <extract.osm.pbf> --relation {args.relation} --work <dir>"
        ),
    }
    write(args.out, meta, cases)
    print(f"{args.out}: {len(cases)} cases from {len(meetings)} name pairs, dropped {dict(dropped)}", file=sys.stderr)


if __name__ == "__main__":
    main()
