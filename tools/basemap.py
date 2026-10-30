"""Builds dash-nodes' self-hosted basemap from Natural Earth (public domain).

Outputs (into argv[2]):
  world-low.json   land + country borders at 1:110m, for small zooms
  world-high.json  countries, borders, state lines (large countries), lakes at 1:50m
  places.json      country and city label points
Coordinates are rounded to 2 decimals (~1 km), plenty for a city-level map.
"""
import json, os, sys

src, out = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)

def load(name):
    with open(os.path.join(src, name + ".geojson")) as f:
        return json.load(f)["features"]

def rnd_line(coords, closed):
    pts = []
    for x, y in coords:
        p = [round(x, 2), round(y, 2)]
        if not pts or pts[-1] != p:
            pts.append(p)
    if closed:
        if pts and pts[0] != pts[-1]:
            pts.append(pts[0])
        return pts if len(pts) >= 4 else None
    return pts if len(pts) >= 2 else None

def rnd_geom(g):
    t, c = g["type"], g["coordinates"]
    if t == "Polygon":
        rings = [r for r in (rnd_line(r, True) for r in c) if r]
        return {"type": t, "coordinates": rings} if rings else None
    if t == "MultiPolygon":
        polys = []
        for p in c:
            rings = [r for r in (rnd_line(r, True) for r in p) if r]
            if rings:
                polys.append(rings)
        return {"type": t, "coordinates": polys} if polys else None
    if t == "LineString":
        l = rnd_line(c, False)
        return {"type": t, "coordinates": l} if l else None
    if t == "MultiLineString":
        ls = [l for l in (rnd_line(l, False) for l in c) if l]
        return {"type": t, "coordinates": ls} if ls else None
    raise ValueError(t)

def feats(name, kind, keep=lambda p: True):
    for f in load(name):
        if not keep(f["properties"]):
            continue
        g = rnd_geom(f["geometry"])
        if g:
            yield {"type": "Feature", "properties": {"k": kind}, "geometry": g}

def write(name, features):
    path = os.path.join(out, name)
    with open(path, "w") as f:
        json.dump({"type": "FeatureCollection", "features": list(features)}, f, separators=(",", ":"))
    print(name, os.path.getsize(path))

# State/province lines only where they help orientation (big federal countries).
BIG = {"USA", "CAN", "AUS", "BRA", "RUS", "CHN", "IND", "MEX", "ARG"}

write("world-low.json", [*feats("ne_110m_land", "land"), *feats("ne_110m_admin_0_boundary_lines_land", "border")])
write("world-high.json", [
    *feats("ne_50m_land", "land"),
    *feats("ne_50m_lakes", "lake", lambda p: p.get("scalerank", 9) <= 3),
    *feats("ne_50m_admin_0_boundary_lines_land", "border"),
    *feats("ne_50m_admin_1_states_provinces_lines", "state", lambda p: p.get("adm0_a3") in BIG),
])

places = []
for f in load("ne_50m_admin_0_countries"):
    p = f["properties"]
    if p.get("LABEL_X") is None:
        continue
    places.append({"type": "Feature", "properties": {"k": "country", "n": p["NAME"], "z": p.get("MIN_LABEL", 5)},
                   "geometry": {"type": "Point", "coordinates": [round(p["LABEL_X"], 2), round(p["LABEL_Y"], 2)]}})
for f in load("ne_50m_populated_places_simple"):
    p = f["properties"]
    x, y = f["geometry"]["coordinates"]
    places.append({"type": "Feature", "properties": {"k": "city", "n": p["nameascii"], "z": p.get("min_zoom", 6)},
                   "geometry": {"type": "Point", "coordinates": [round(x, 2), round(y, 2)]}})
write("places.json", places)
