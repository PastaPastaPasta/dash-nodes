Unmodified copies, so the site loads nothing from third-party CDNs (only the `sourceMappingURL`
comments are stripped):

- `maplibre-gl.mjs`, `maplibre-gl-worker.mjs`, `maplibre-gl.css`: [MapLibre GL JS](https://maplibre.org) 6.16.0
  (`dist/` of the npm package), BSD 3-Clause.

Other third-party assets:

- `../fonts/archivo-*.woff2`: [Archivo](https://github.com/Omnibus-Type/Archivo) variable (weight and width axes,
  latin and latin-ext subsets from Google Fonts), SIL Open Font License 1.1.
- `../map/glyphs/`: Noto Sans map label glyphs from [protomaps/basemaps-assets](https://github.com/protomaps/basemaps-assets),
  SIL Open Font License 1.1.
- `../map/*.json`: built from [Natural Earth](https://www.naturalearthdata.com) (public domain) by `tools/basemap.py`.

Not committed here: the Worker build (`worker/scripts/build.mjs`) copies `@dashevo/evo-sdk`'s `dist/evo-sdk.module.js`
(the version pinned in `worker/package.json`, MIT) to `evo-sdk-<version>/`, for evonode pages (`assets/platform-live.js`).
The Rust server's own copy of the site doesn't have it; those pages then skip the live, proof-checked part.
