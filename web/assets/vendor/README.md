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
