"""Cleans the image-generated DashHQ Radar icon (radar-master-src.png) into the masters tools/icons.sh
uses. Raster editing only, no vector drawing.

    uv run --with pillow --with numpy --with scipy tools/icons/polish.py

- radar-master.png: the full mark in the source's own pixels and colours (the sweep's electric blue,
  the rings' glow). Only the black corners go transparent and the tile's speckle flattens to one navy.
- radar-master-mark.png: the header's logo (22 to 26 px): the full mark with the source's own ring
  pixels grown to Dash blue, and its white (the D, the sweep's line, the blip) grown, so the rings
  survive the downscale. The innermost ring is mostly behind the D, as in the source.
- radar-master-32.png and radar-master-16.png: the tabs (32 and 48 px; 16 px). The D is the source's,
  grown; the sweep's colours are the source's own, by bearing, stretched over a wider wedge. The rings,
  the sweep's line and the blip are redrawn as clean shapes where the source has them (radii and the
  blip's bearing measured from it), heavy enough to survive: the two outer rings at 32 px, the outer
  one at 16.
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


# Full: pixels within a few levels of navy are the tile's noise. On the antialiased edge the source is
# navy blended with black, which the alpha already does: navy there.
off_navy = np.linalg.norm(src - NAVY, axis=-1)
full = np.where(inside[..., None], over(NAVY, src, smoothstep(off_navy, 4, 12)), NAVY)
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
assert np.isfinite(sweep).all() and sweep[0, 2] > 2 * NAVY[2], "no sweep behind the blip: another source?"

# Mark: the full mark with each ring's own pixels (at least half as far from navy as that ring's
# brightest, the white of the D left out) grown to at least Dash blue, and the white grown.
ring_px = np.zeros(r.shape, bool)
for rad in RINGS:
    band = (np.abs(r - rad) < 3) & (white < 0.5)
    ring_px |= band & (off_navy > 0.5 * np.percentile(off_navy[band], 99))
assert ring_px.sum() > 20_000, "the rings aren't where RINGS expects: another source?"
mark = over(full, np.maximum(full, BLUE), grow(ring_px, 9))
save(over(mark, WHITE, grow(white > 0.5, 5)), "radar-master-mark.png")


def tab(circles, span, gain, line, name):
    """A tab-size mark: circles as (radius, half-width; drawn about 6 px wider), the outermost first.
    The sweep fills the wedge `span` degrees behind the blip out to that circle, the source's sweep
    profile stretched over it and made `gain` times as strong; the blip sits on that circle, joined to
    the centre by a `line` px line."""
    outer = circles[0][0]
    behind = ang - bearing
    t = np.clip(behind / span, 0, 1)
    color = sweep[np.round(t * (len(trail) - 1)).astype(int)]
    color = np.clip(NAVY + (color - NAVY) * gain, 0, 255)
    rgb = np.where(((behind >= 0) & (behind <= span) & (r < outer))[..., None], color, NAVY)
    for rad, half in circles:
        rgb = over(rgb, BLUE, grow(np.abs(r - rad) < 3, half))
    b = np.radians(bearing)
    ty, tx = C - outer * np.sin(b), C + outer * np.cos(b)
    # The line, from a quarter of the way out to the blip: distance to that segment.
    s = np.clip(((xx - C) * (tx - C) + (yy - C) * (ty - C)) / outer**2, 0.25, 1)
    to_line = np.hypot(xx - C - s * (tx - C), yy - C - s * (ty - C))
    w = np.maximum(np.clip(line / 2 + 0.5 - to_line, 0, 1), grow(np.hypot(xx - tx, yy - ty) < 18, 20))
    save(over(rgb, WHITE, np.maximum(w, grow(d_mask, 8))), name)


tab([(RINGS[3], 19), (RINGS[2], 14)], span=65, gain=1.5, line=16, name="radar-master-32.png")
tab([(RINGS[3], 22)], span=65, gain=1.5, line=16, name="radar-master-16.png")
print("wrote radar-master.png, radar-master-mark.png, radar-master-32.png and radar-master-16.png")
