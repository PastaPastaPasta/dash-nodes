# Icon slot (TODO: icon)

The DashHQ Radar icon goes here. Until the whole set below is in this directory, every page keeps
`/assets/favicon.svg` (the old Dash Nodes mark) and nothing links to these names.

| File | Size | Used for |
| --- | --- | --- |
| `favicon.ico` | 16, 32 and 48 px in one file | browser tabs; also served as `/favicon.ico` |
| `favicon-16.png` | 16 × 16 | `<link rel="icon" sizes="16x16">` |
| `favicon-32.png` | 32 × 32 | `<link rel="icon" sizes="32x32">` |
| `apple-touch-icon.png` | 180 × 180, opaque | iOS home screen (`<link rel="apple-touch-icon">`) |
| `icon-192.png` | 192 × 192 | web manifest (`site.webmanifest`) |
| `icon-512.png` | 512 × 512 | web manifest, install splash |
| `og.png` | 1200 × 630 | link previews (`og:image`, `twitter:card` = `summary_large_image`) |

Make them all from one square master PNG (1024 px or more), and optionally a 1200 × 630 one for
link previews (without it, `og.png` is the master centred on the site's navy):

```sh
tools/icons.sh path/to/master.png [path/to/og-master.png]
```

Keep the masters out of `web/` (everything there is published). The Worker build
(`worker/scripts/build.mjs`) does the rest when it sees the set: in each page it replaces the
`favicon.svg` link and the `TODO(icon)` comment with links to these files (and `og:image` where the
page has Open Graph tags), and puts the 192 and 512 PNGs in the manifest. Part of a set fails the
build. The local Rust server serves `web/` as is, so it keeps `favicon.svg`.

The header's small logo is still the inline SVG `LOGO` in `assets/common.js`; swap it separately
if the new mark should replace it there too.
