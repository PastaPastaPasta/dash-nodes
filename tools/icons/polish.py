"""Cleans the image-generated DashHQ Radar icon (radar-master-src.png) into the two masters tools/icons.sh
uses: radar-master.png (the full mark) and radar-master-small.png (the 16 and 32 px favicons: the D in
one heavy ring, so it reads in a browser tab). Raster editing only: every shape comes from the source.

    uv run --with pillow --with numpy --with scipy tools/icons/polish.py

- Each pixel is split into the tile's navy, Dash blue and white (the source has no red, so red is white).
- Every ring is scaled so its core is exactly Dash blue #008DE4 (the generator drew the inner ones dimmer),
  as is the sweep's leading edge; the tile's noise goes, leaving one navy.
- The rounded square's alpha comes from the source too: the black corners become transparent.
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
SMALL_RING = 347  # the ring the small mark keeps
SMALL_STROKE = 60  # its width there: about 2 px at 32 and 1 px at 16
SMALL_BOLD = 8  # the D's strokes grow by this much, so its counter stays open at 16 px
SMALL_BLIP = 22  # and the blip by this much: about 2 px across at 32


def smoothstep(x, lo, hi):
    t = np.clip((x - lo) / (hi - lo), 0, 1)
    return t * t * (3 - 2 * t)


src = np.asarray(Image.open(HERE / "radar-master-src.png").convert("RGB"), float)
h, w, _ = src.shape
assert (h, w) == (1024, 1024), "expects the 1024 px source"
yy, xx = np.mgrid[0:h, 0:w]
r = np.hypot(xx - 511.5, yy - 511.5)
ang = np.degrees(np.arctan2(511.5 - yy, xx - 511.5))  # 0 = right, 90 = up

# Tile: everything not black. Filled, so nothing inside can turn transparent; the edge keeps its
# antialiasing from the source.
edge = np.clip(src[..., 2] / NAVY[2], 0, 1)
inside = ndimage.binary_erosion(ndimage.binary_fill_holes(edge > 0.5), iterations=3)
tile = np.where(inside, 1.0, edge)

# White (the D, the sweep's line, the blip): red channel. Hardened so the D is solid white with a
# one-pixel antialiased edge.
white = smoothstep(src[..., 0] / 255, 0.30, 0.80)

# Blue: what's left after taking white out, projected from navy towards Dash blue.
rest = (src - white[..., None] * WHITE) / np.maximum(1 - white[..., None], 1e-3)
d = BLUE - NAVY
blue = np.clip(((rest - NAVY) @ d) / (d @ d), 0, None)

# The sweep: the wedge from the blip's bearing (about 41 degrees) back towards 12 o'clock.
wedge = (ang > 38) & (ang < 115) & (r > 120) & (r < 470)
gain = np.ones_like(blue)
for ring in RINGS:
    band = (np.abs(r - ring) < 9) & ~wedge
    gain[np.abs(r - ring) < 9] = 1 / np.percentile(blue[band], 99)
gain[wedge] = 1 / np.percentile(blue[wedge], 99.5)
blue = smoothstep(np.clip(blue * gain, 0, 1), 0.06, 1.0)  # below 6 % is the tile's noise


def compose(blue, white):
    rgb = NAVY * (1 - blue[..., None]) + BLUE * blue[..., None]
    rgb = rgb * (1 - white[..., None]) + WHITE * white[..., None]
    a = tile[..., None] * 255
    return Image.fromarray(np.concatenate([rgb, a], axis=-1).round().clip(0, 255).astype(np.uint8), "RGBA")


compose(blue, white).save(HERE / "radar-master.png", optimize=True)

# Small: the D (white shapes thicker than the sweep's line, minus the blip) and one ring, made heavy by
# dilating its own pixels; the blip, enlarged, moves in along its bearing onto that ring.
solid = ndimage.binary_opening(white > 0.5, iterations=6)
labels, n = ndimage.label(solid)
sizes = ndimage.sum(solid, labels, range(1, n + 1))
blip_label = 1 + max((i for i in range(n) if sizes[i] < 3000), key=lambda i: sizes[i])
d_mask = solid & (labels != blip_label) & np.isin(labels, [i + 1 for i in range(n) if sizes[i] >= 3000])


def grow(mask, px):
    """Grows a mask by px in every direction (round, unlike a square kernel), antialiased."""
    return np.clip(px + 0.5 - ndimage.distance_transform_edt(~mask), 0, 1)


d_white = grow(d_mask, SMALL_BOLD)

ring = (np.abs(r - SMALL_RING) < 4) & (blue > 0.5) & (ndimage.distance_transform_edt(~d_mask) > 10)
ring_blue = grow(ring, SMALL_STROKE / 2)

# The blip is the farthest white from the centre; just the dot, not the line it ends.
far = np.where(white > 0.5, r, 0)
fy, fx = np.unravel_index(np.argmax(far), far.shape)
dot = (white > 0.5) & (np.hypot(xx - fx, yy - fy) < 26)
by, bx = ndimage.center_of_mass(dot)
bearing = np.arctan2(511.5 - by, bx - 511.5)
ty, tx = 511.5 - SMALL_RING * np.sin(bearing), 511.5 + SMALL_RING * np.cos(bearing)
blip = grow(ndimage.shift(dot.astype(float), (ty - by, tx - bx), order=1) > 0.5, SMALL_BLIP)

compose(ring_blue * (1 - blip), np.maximum(d_white, blip)).save(HERE / "radar-master-small.png", optimize=True)
print("wrote radar-master.png and radar-master-small.png")
