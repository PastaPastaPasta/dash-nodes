# Icons

The DashHQ Radar icon set. The Worker build (`worker/scripts/build.mjs`) links it from every page:
it replaces each page's `favicon.svg` link and `TODO(icon)` comment with links to these files, adds
`og:image` where the page has Open Graph tags, and puts the 192 and 512 PNGs in the manifest. Part of
a set fails the build. The local Rust server serves `web/` as is, so it keeps `favicon.svg`.

| File | Size | From | Used for |
| --- | --- | --- | --- |
| `favicon.ico` | 16, 32 and 48 px in one file | small master | browser tabs; also served as `/favicon.ico` |
| `favicon-16.png` | 16 × 16 | small master | `<link rel="icon" sizes="16x16">` |
| `favicon-32.png` | 32 × 32 | small master | `<link rel="icon" sizes="32x32">` |
| `mark-96.png` | 96 × 96 | small master | the header's logo (`assets/common.js`) |
| `apple-touch-icon.png` | 180 × 180, opaque | master | iOS home screen (`<link rel="apple-touch-icon">`) |
| `icon-192.png` | 192 × 192 | master | web manifest (`site.webmanifest`) |
| `icon-512.png` | 512 × 512 | master | web manifest, install splash |
| `og.png` | 1200 × 630 | card | link previews (`og:image`, `twitter:card` = `summary_large_image`) |

The sources are in `tools/icons/` (kept out of `web/`, since everything here is published):

- `radar-master-src.png`: the approved image-generated icon, as delivered.
- `radar-master.png` and `radar-master-small.png`: `polish.py` cleans the source into the full mark
  (exact Dash blue rings, one navy, transparent corners) and the tab-size one (the D in a single heavy
  ring, for 16 and 32 px). Raster editing only.
- `radar-og.png`: `og.html` rendered by headless Chrome (the command is in the file).

Then `tools/icons.sh` (ImageMagick) writes everything above from those three.
