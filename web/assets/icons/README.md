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
  `polish.py` makes them from the source by raster editing only. The full mark keeps the source's
  colours, with transparent corners and one navy. The others are simplified for their sizes but keep the
  sweep and blip that make it a radar: the header's mark has all four rings thickened, the 32 px one
  the two outer rings, and the 16 px one the outer ring with the sweep filling up to it.
- `radar-og.png`: `og.html` rendered by headless Chrome (the command is in the file).

Then `tools/icons.sh` (ImageMagick) writes everything above from those masters and the card.
