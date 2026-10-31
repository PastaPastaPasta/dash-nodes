"""Cleans the image-generated DashHQ Radar icon (radar-master-src.png) into the masters tools/icons.sh
uses. Raster editing only: every shape and colour comes from the source.

    uv run --with pillow --with numpy --with scipy tools/icons/polish.py

- radar-master.png: the full mark in the source's own colours (the sweep's electric blue, the rings'
  glow). Only the black corners go transparent and the tile's speckle flattens to one navy.
- radar-master-mark.png: the header's logo (22 to 26 px): the same with every ring, the sweep's line
  and the D thickened, so all four rings survive the downscale.
- radar-master-32.png: 32 and 48 px tabs: the D, the two outer rings, the sweep and the blip.
- radar-master-16.png: 16 px tabs: the D, the outer ring and the sweep filling up to it, which is what
  still says radar at that size.
"""
from pathlib import Path

import numpy as np
from PIL import Image
from scipy import ndimage

HERE = Path(__file__).parent
NAVY = np.array([0, 26, 63], float)  # the tile's own navy (#001A3F)
BLUE = np.array([0, 141, 228], float)  # Dash blue #008DE4
WHITE = np.array([255, 255, 255], float)
RINGS = [164, 257, 347, 443]  # ring radii in the 1024 px source, from the centre
SWEEP_TAIL = 90  # the bearing (degrees, 0 = right, 90 = up) where the source's sweep has faded out


def smoothstep(x, lo, hi):
    t = np.clip((x - lo) / (hi - lo), 0, 1)
    return t * t * (3 - 2 * t)


def grow(mask, px):
    """Grows a mask by px in every direction (round, unlike a square kernel), antialiased."""
    return np.clip(px + 0.5 - ndimage.distance_transform_edt(~mask), 0, 1)


def over(rgb, color, a):
    """Paints color over rgb with coverage a."""
    return rgb * (1 - a[..., None]) + color * a[..., None]


src = np.asarray(Image.open(HERE / "radar-master-src.png").convert("RGB"), float)
assert src.shape[:2] == (1024, 1024), "expects the 1024 px source"
C = 511.5  # its centre
yy, xx = np.mgrid[0:1024, 0:1024]
r = np.hypot(xx - C, yy - C)
ang = np.degrees(np.arctan2(C - yy, xx - C))

# Tile: everything not black. Filled, so nothing inside can turn transparent; the edge keeps its
# antialiasing from the source.
edge = np.clip(src[..., 2] / NAVY[2], 0, 1)
inside = ndimage.binary_erosion(ndimage.binary_fill_holes(edge > 0.5), iterations=3)
tile = np.where(inside, 1.0, edge)


def save(rgb, name):
    a = tile[..., None] * 255
    Image.fromarray(np.concatenate([rgb, a], -1).round().clip(0, 255).astype(np.uint8), "RGBA").save(
        HERE / name, optimize=True)


# Full: pixels within a few levels of navy are the tile's noise.
off_navy = np.linalg.norm(src - NAVY, axis=-1)
full = over(NAVY, src, smoothstep(off_navy, 4, 12))
save(full, "radar-master.png")

# White (the D, the sweep's line, the blip): the red channel, since the source has no red.
white = smoothstep(src[..., 0] / 255, 0.30, 0.80)

# The D: white shapes thicker than the sweep's line, bigger than the blip.
solid = ndimage.binary_opening(white > 0.5, iterations=6)
labels, n = ndimage.label(solid)
sizes = ndimage.sum(solid, labels, range(1, n + 1))
d_mask = np.isin(labels, 1 + np.flatnonzero(sizes >= 3000))
assert d_mask.sum() > 50_000, "no D where expected: another source?"

# The blip is the farthest white from the centre; its bearing is the sweep's leading edge.
far = np.where(white > 0.5, r, 0)
fy, fx = np.unravel_index(np.argmax(far), far.shape)
by, bx = ndimage.center_of_mass((white > 0.5) & (np.hypot(xx - fx, yy - fy) < 18))
bearing = np.degrees(np.arctan2(C - by, bx - C))
assert 30 < bearing < 55, "the blip isn't up and to the right: another source?"

# The sweep's colour by how far behind the leading edge, from the source: the median over the wedge's
# radii at each bearing, the white line left out.
trail = np.arange(int(bearing) + 1, SWEEP_TAIL + 1)
sweep = np.array([np.median(src[(np.abs(ang - a) < 0.5) & (r > 180) & (r < 420) & (white < 0.05)], axis=0)
                  for a in trail])
assert sweep[0, 2] > 2 * NAVY[2], "no sweep behind the blip: another source?"

# Mark: the full mark with every ring thickened to at least Dash blue, and the white grown.
rings = np.zeros(r.shape, bool)
for rad in RINGS:
    rings |= (np.abs(r - rad) < 3) & (off_navy > 120)
mark = over(full, np.maximum(full, BLUE), grow(rings, 9))
save(over(mark, WHITE, grow(white > 0.5, 5)), "radar-master-mark.png")


def tab(rings, span, gain, line, name):
    """A tab-size mark: rings as (radius, stroke), the outermost first. The sweep fills the wedge
    `span` degrees behind the blip out to that ring, its colours from the source's own sweep made
    `gain` times as strong; the blip sits on that ring, joined to the centre by a `line` px line."""
    outer = rings[0][0]
    behind = ang - bearing
    t = np.clip(behind / span, 0, 1)
    color = sweep[np.round(t * (len(trail) - 1)).astype(int)]
    color = np.clip(NAVY + (color - NAVY) * gain, 0, 255)
    rgb = np.where(((behind >= 0) & (behind <= span) & (r < outer))[..., None], color, NAVY)
    for rad, stroke in rings:
        rgb = over(rgb, BLUE, grow(np.abs(r - rad) < 3, stroke / 2))
    b = np.radians(bearing)
    ty, tx = C - outer * np.sin(b), C + outer * np.cos(b)
    # The line, from a quarter of the way out to the blip: distance to that segment.
    s = np.clip(((xx - C) * (tx - C) + (yy - C) * (ty - C)) / outer**2, 0.25, 1)
    to_line = np.hypot(xx - C - s * (tx - C), yy - C - s * (ty - C))
    w = np.maximum(np.clip(line / 2 + 0.5 - to_line, 0, 1) if line else 0, grow(np.hypot(xx - tx, yy - ty) < 18, 20))
    save(over(rgb, WHITE, np.maximum(w, grow(d_mask, 8))), name)


tab([(443, 38), (347, 28)], span=65, gain=1.5, line=16, name="radar-master-32.png")
tab([(443, 44)], span=65, gain=1.5, line=16, name="radar-master-16.png")
print("wrote radar-master.png, radar-master-mark.png, radar-master-32.png and radar-master-16.png")
