# Icons

The DashHQ Radar icon set. The Worker build (`worker/scripts/build.mjs`) links it from every page:
it replaces each page's `favicon.svg` link and `TODO(icon)` comment with links to these files, adds
`og:image` where the page has Open Graph tags, and puts the 192 and 512 PNGs in the manifest. Part of
a set fails the build. The local Rust server serves `web/` as is, so it keeps `favicon.svg`.

| File | Size | From | Used for |
| --- | --- | --- | --- |
| `favicon.ico` | 16, 32 and 48 px in one file | 16 and 32 px masters | browser tabs; also served as `/favicon.ico` |
| `favicon-16.png` | 16 × 16 | 16 px master | `<link rel="icon" sizes="16x16">` |
| `favicon-32.png` | 32 × 32 | 32 px master | `<link rel="icon" sizes="32x32">` |
| `mark-96.png` | 96 × 96 | mark master | the header's logo (`assets/common.js`) |
| `apple-touch-icon.png` | 180 × 180, opaque | master | iOS home screen (`<link rel="apple-touch-icon">`) |
| `icon-192.png` | 192 × 192 | master | web manifest (`site.webmanifest`) |
| `icon-512.png` | 512 × 512 | master | web manifest, install splash |
| `og.png` | 1200 × 630 | card | link previews (`og:image`, `twitter:card` = `summary_large_image`) |

The sources are in `tools/icons/` (kept out of `web/`, since everything here is published):

- `radar-master-src.png`: the approved image-generated icon, as delivered.
- `radar-master.png`, `radar-master-mark.png`, `radar-master-32.png` and `radar-master-16.png`:
  `polish.py` makes them from the source by raster editing only (no vector drawing). The full mark is
  the source's own pixels and colours, with transparent corners and one navy. The header's mark grows
  the source's ring pixels and white so they survive 22 px. The tab marks keep the source's D and sweep
  colours, and redraw the rings, the sweep's line and the blip as heavier clean shapes where the source
  has them: the two outer rings at 32 px, the outer one at 16.
- `radar-og.png`: `og.html` rendered by headless Chrome (the command is in the file).

Then `tools/icons.sh` (ImageMagick) writes everything above from those masters and the card.
