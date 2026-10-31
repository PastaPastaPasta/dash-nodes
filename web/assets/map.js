// Network map: masternodes (or every reachable node, counted per city) on a self-hosted Natural Earth
// basemap. MapLibre draws it on the GPU so zoom and pan stay smooth; clusters are small DOM donuts.
import { Map as GLMap, Marker, Popup, NavigationControl, AttributionControl, LngLatBounds } from "/assets/vendor/maplibre-gl.mjs";
import { api, chrome, currentNet, el, fmt, pct, compact, href, place, shortHash, badge, nodeStatus, evoPill, mnFilters, mnMatches,
  rememberParams, filterQuery, countBy, segmented, filterSelect, searchBox, REACH, REGISTRATION, registration, poseBadge, TYPE_OPTIONS, versionOptions,
  countryOptions, hostOptions, DKG_OPTIONS } from "./common.js";
import { bottomSheet } from "./sheet.js";

chrome("map", { footer: false });
const net = currentNet();
const params = new URLSearchParams(location.search);

const state = {
  ...mnFilters(params),
  layer: params.get("layer") === "nodes" ? "nodes" : "mn",
  color: params.get("color") || "reach",
  hidden: new Set((params.get("hide") || "").split(",").filter(Boolean)),
  protx: params.get("protx") || "",
};
let rows = [];
let byProtx = new Map();
let tip = null;
let geo = null; // per-city counts of every reachable node, when the API has them
let group = null; // masternodes at one place, listed in the panel

// ---- color modes ----
// A mode maps a masternode to a legend category and a category to a tone. Tones are CSS color roles,
// so markers, donuts and chips agree in both themes.
const REACH_TONES = { Reachable: "good", Unreachable: "critical", "Not checked": "axis" };
const MODES = {
  reach: { label: "Reachability", key: (m) => nodeStatus(m)[1], order: REACH, tone: (k) => REACH_TONES[k] },
  pose: { label: "PoSe", key: registration, order: REGISTRATION, tone: (k) => (k === "PoSe-banned" ? "serious" : "s1") },
  version: { label: "Version" }, // filled in from the data by assignVersionTones()
  type: { label: "Type", key: (m) => m.type, order: ["Regular", "Evo"], tone: (k) => (k === "Evo" ? "s3" : "s1") },
};
const TONES = ["good", "critical", "serious", "axis", "s1", "s2", "s3", "s4", "s5", "other"];
if (state.color === "status") state.color = "reach"; // the old name
if (!Object.hasOwn(MODES, state.color)) state.color = "reach";
// The list links here with ?reach=… or ?status=…; on the map that's the chips of that mode. The map
// colors by one mode at a time, so with both, reachability becomes the chips and the PoSe filter
// stays a filter.
const chipped = [["reach", "reach"], ["status", "pose"]].find(([param]) => state[param]);
if (chipped) {
  const [param, mode] = chipped;
  state.color = mode;
  state.hidden = new Set(MODES[mode].order.filter((k) => k !== state[param]));
  state[param] = "";
}

/** The five most common versions keep their own colors whatever the filters; the rest share one. */
function assignVersionTones() {
  const top = countBy(rows.filter((m) => m.version), (m) => m.version).slice(0, 5).map(([v]) => v);
  const slot = new Map(top.map((v, i) => [v, `s${i + 1}`]));
  Object.assign(MODES.version, {
    key: (m) => (!m.version ? "Unknown" : slot.has(m.version) ? m.version : "Other"),
    order: [...top, "Other", "Unknown"],
    tone: (k) => slot.get(k) || (k === "Unknown" ? "axis" : "other"),
  });
}

const phone = () => matchMedia("(max-width: 900px)").matches;
const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(`--${name}`).trim();

// ---- basemap ----
// Two levels of detail: 1:110m below HIGH, 1:50m (with lakes and state lines) from HIGH up. The
// detailed file is only fetched once someone zooms in.
const HIGH = 3.2;
const FONT = ["Noto Sans Regular"];
const BASE_PAINT = [
  ["ocean", "background-color", "ocean"],
  ["land-low", "fill-color", "land"], ["border-low", "line-color", "border-line"],
  ["land-high", "fill-color", "land"], ["lake", "fill-color", "ocean"], ["state", "line-color", "state-line"],
  ["border-high", "line-color", "border-line"],
  ["country-label", "text-color", "label"], ["country-label", "text-halo-color", "land"],
  ["city-label", "text-color", "label"], ["city-label", "text-halo-color", "land"],
];
const kind = (k) => ["==", ["get", "k"], k];

function style() {
  return {
    version: 8,
    glyphs: "/assets/map/glyphs/{fontstack}/{range}.pbf",
    sources: {
      low: { type: "geojson", data: "/assets/map/world-low.json" },
      places: { type: "geojson", data: "/assets/map/places.json" },
    },
    layers: [
      { id: "ocean", type: "background", paint: { "background-color": css("ocean") } },
      { id: "land-low", type: "fill", source: "low", maxzoom: HIGH, filter: kind("land"), paint: { "fill-color": css("land") } },
      { id: "border-low", type: "line", source: "low", maxzoom: HIGH, filter: kind("border"),
        paint: { "line-color": css("border-line"), "line-width": 0.7 } },
      { id: "country-label", type: "symbol", source: "places", minzoom: 1.6,
        filter: ["all", kind("country"), ["<=", ["get", "z"], ["+", ["zoom"], 1.2]]],
        layout: { "text-field": ["get", "n"], "text-font": FONT, "text-size": ["interpolate", ["linear"], ["zoom"], 2, 10.5, 6, 14],
          "text-max-width": 7, "text-letter-spacing": 0.04, "text-padding": 6 },
        paint: { "text-color": css("label"), "text-halo-color": css("land"), "text-halo-width": 1.2, "text-opacity": 0.85 } },
      { id: "city-label", type: "symbol", source: "places", minzoom: 4.5,
        filter: ["all", kind("city"), ["<=", ["get", "z"], ["zoom"]]],
        layout: { "text-field": ["get", "n"], "text-font": FONT, "text-size": 11, "text-padding": 4,
          "text-variable-anchor": ["top", "bottom", "left", "right"], "text-radial-offset": 0.6 },
        paint: { "text-color": css("label"), "text-halo-color": css("land"), "text-halo-width": 1.2, "text-opacity": 0.7 } },
    ],
  };
}

function addDetail() {
  if (map.getSource("high")) return;
  map.addSource("high", { type: "geojson", data: "/assets/map/world-high.json" });
  const before = "country-label";
  map.addLayer({ id: "land-high", type: "fill", source: "high", minzoom: HIGH, filter: kind("land"), paint: { "fill-color": css("land") } }, before);
  map.addLayer({ id: "lake", type: "fill", source: "high", minzoom: HIGH, filter: kind("lake"), paint: { "fill-color": css("ocean") } }, before);
  map.addLayer({ id: "state", type: "line", source: "high", minzoom: HIGH, filter: kind("state"),
    paint: { "line-color": css("state-line"), "line-width": 0.6 } }, before);
  map.addLayer({ id: "border-high", type: "line", source: "high", minzoom: HIGH, filter: kind("border"),
    paint: { "line-color": css("border-line"), "line-width": ["interpolate", ["linear"], ["zoom"], 3, 0.7, 8, 1.4] } }, before);
}

const toneMatch = () => ["match", ["get", "tone"], ...TONES.flatMap((t) => [t, css(t)]), css("other")];

function repaint() {
  for (const [id, prop, token] of BASE_PAINT) if (map.getLayer(id)) map.setPaintProperty(id, prop, css(token));
  if (map.getLayer("mn-point")) {
    map.setPaintProperty("mn-point", "circle-color", toneMatch());
    map.setPaintProperty("mn-point", "circle-stroke-color", css("land"));
    map.setPaintProperty("mn-selected", "circle-stroke-color", css("ink"));
  }
}

const map = new GLMap({
  container: "map",
  style: style(),
  center: [12, 30],
  zoom: 1.4,
  minZoom: 0.8,
  maxZoom: 9,
  dragRotate: false,
  pitchWithRotate: false,
  touchPitch: false,
  fadeDuration: 0,
  attributionControl: false,
});
map.touchZoomRotate.disableRotation();
map.keyboard.disableRotation();
map.addControl(new NavigationControl({ showCompass: false }), "bottom-right");
map.addControl(new AttributionControl({ compact: true, customAttribution: "Basemap © Natural Earth · Geolocation by DB-IP (CC BY 4.0)" }), "bottom-right");
// Phones: start with the attribution folded into its (i) button instead of across the map.
if (phone()) {
  map.once("load", () => document.querySelector(".maplibregl-ctrl-attrib")?.classList.remove("maplibregl-compact-show"));
}
map.on("zoom", () => { if (map.isStyleLoaded() && map.getZoom() >= HIGH - 0.6) addDetail(); });
window.addEventListener("themechange", () => { repaint(); refreshClusters(true); });

// ---- data on the map ----

/** Masternodes matching the search and selects; the chips then hide categories of these on the map. */
const filtered = () => rows.filter((m) => mnMatches(m, state));

function visible() {
  const mode = MODES[state.color];
  return filtered().filter((m) => !state.hidden.has(mode.key(m)));
}

const collection = (features) => ({ type: "FeatureCollection", features });
const feature = (lon, lat, properties) => ({ type: "Feature", geometry: { type: "Point", coordinates: [lon, lat] }, properties });

function mnFeatures(list) {
  const mode = MODES[state.color];
  return collection(list.filter((m) => m.lat != null && m.lon != null)
    .map((m) => feature(m.lon, m.lat, { protx: m.protx, tone: mode.tone(mode.key(m)), evo: m.type === "Evo" })));
}

function nodeFeatures() {
  return collection((geo?.points || []).map((p, i) => feature(p.lon, p.lat, { i, nodes: p.nodes, masternodes: p.masternodes })));
}

function addDataLayers() {
  // Masternodes that share a point (one city) stay a cluster at every zoom; opening it lists them.
  map.addSource("mn", {
    type: "geojson", data: mnFeatures([]), cluster: true, clusterRadius: 44, clusterMaxZoom: 9,
    clusterProperties: Object.fromEntries(TONES.map((t) => [t, ["+", ["case", ["==", ["get", "tone"], t], 1, 0]]])),
  });
  map.addLayer({ id: "mn-point", type: "circle", source: "mn", filter: ["!", ["has", "point_count"]],
    paint: {
      "circle-color": toneMatch(),
      "circle-radius": ["interpolate", ["linear"], ["zoom"], 1, ["case", ["get", "evo"], 4.5, 3.5], 8, ["case", ["get", "evo"], 8, 6.5]],
      "circle-stroke-color": css("land"), "circle-stroke-width": 1.5,
    } });
  map.addLayer({ id: "mn-selected", type: "circle", source: "mn", filter: ["==", ["get", "protx"], state.protx || "-"],
    paint: { "circle-radius": 11, "circle-color": "rgba(0,0,0,0)", "circle-stroke-color": css("ink"), "circle-stroke-width": 2 } });
  map.addSource("nodes", {
    type: "geojson", data: nodeFeatures(), cluster: true, clusterRadius: 50, clusterMaxZoom: 9,
    clusterProperties: { nodes: ["+", ["get", "nodes"]], masternodes: ["+", ["get", "masternodes"]] },
  });
  // MapLibre only loads sources a layer uses; this invisible one keeps "nodes" queryable for its donuts.
  map.addLayer({ id: "nodes-anchor", type: "circle", source: "nodes", paint: { "circle-radius": 0, "circle-opacity": 0 } });
  map.on("click", "mn-point", (e) => {
    const m = byProtx.get(e.features[0].properties.protx);
    if (m) openPopup(m);
  });
  map.on("mouseenter", "mn-point", () => { map.getCanvas().style.cursor = "pointer"; });
  map.on("mouseleave", "mn-point", () => { map.getCanvas().style.cursor = ""; });
  // Donuts follow the clustering: refreshed at most every 120 ms while frames render, and once the map is idle.
  let last = 0;
  map.on("render", () => { const now = performance.now(); if (now - last > 120) { last = now; refreshClusters(); } });
  map.on("idle", () => refreshClusters());
}

// Clusters (and, for all nodes, every city) are DOM donuts: one marker per cluster on screen, kept
// while that cluster stays in view, dropped when the data or theme changes.
let shown = new Map();

function refreshClusters(reset = false) {
  if (reset) {
    for (const m of shown.values()) m.remove();
    shown = new Map();
  }
  const src = state.layer; // the layer names its source
  if (!map.getSource(src) || !map.isSourceLoaded(src)) return;
  const next = new Map();
  for (const f of map.querySourceFeatures(src, src === "mn" ? { filter: ["has", "point_count"] } : undefined)) {
    const p = f.properties;
    const id = p.cluster ? `c${p.cluster_id}` : `p${p.i}`;
    if (next.has(id)) continue;
    next.set(id, shown.get(id) || new Marker({ element: src === "mn" ? mnDonut(f) : nodeDonut(f) }).setLngLat(f.geometry.coordinates).addTo(map));
  }
  for (const [id, mk] of shown) if (!next.has(id)) mk.remove();
  shown = next;
}

function donut(segments, total, label, onClick) {
  const size = Math.round(26 + Math.min(30, Math.sqrt(total) * 1.6));
  let at = 0;
  const stops = segments.filter(([, n]) => n).map(([tone, n]) => {
    const end = at + (n / total) * 100;
    const s = `var(--${tone}) ${at.toFixed(2)}% ${end.toFixed(2)}%`;
    at = end;
    return s;
  });
  return el("button", {
    type: "button", class: "map-donut", "aria-label": label, title: label,
    style: { width: `${size}px`, height: `${size}px`, background: `conic-gradient(${stops.join(", ")})` },
    onclick: (e) => { e.stopPropagation(); Promise.resolve(onClick()).catch(() => {}); },
  }, el("span", { text: compact(total) }));
}

function mnDonut(f) {
  const p = f.properties;
  const mode = MODES[state.color];
  const parts = mode.order.map((k) => [k, p[mode.tone(k)]]).filter(([, n]) => n);
  const label = `${fmt(p.point_count)} masternodes: ${parts.map(([k, n]) => `${fmt(n)} ${k}`).join(", ")}`;
  const tones = [...new Set(mode.order.map(mode.tone))];
  return donut(tones.map((t) => [t, p[t]]), p.point_count, label, () => openCluster(f));
}

function nodeDonut(f) {
  const p = f.properties;
  const label = `${fmt(p.nodes)} reachable nodes, ${fmt(p.masternodes)} of them masternodes`;
  return donut([["s1", p.masternodes], ["s3", p.nodes - p.masternodes]], p.nodes, label, () => {
    if (p.cluster) zoomInto("nodes", p.cluster_id, f.geometry.coordinates, () => showCity(p));
    else showCity(p);
  });
}

/** Zooms into a cluster; when it can't split any further (one place), `atBottom` runs instead. */
async function zoomInto(src, id, center, atBottom) {
  const zoom = await map.getSource(src).getClusterExpansionZoom(id);
  if (zoom > map.getMaxZoom()) return atBottom();
  map.easeTo({ center, zoom, duration: 450 });
  // From the keyboard, the focused donut is about to be replaced: keep focus on the map instead.
  if (document.activeElement?.matches(".map-donut:focus-visible")) map.getCanvas().focus();
}

function openCluster(f) {
  const id = f.properties.cluster_id;
  zoomInto("mn", id, f.geometry.coordinates, async () => {
    const leaves = await map.getSource("mn").getClusterLeaves(id, Infinity, 0);
    showGroup(leaves.map((l) => byProtx.get(l.properties.protx)).filter(Boolean));
  });
}

// ---- popup ----

let popup = null;

function nodeCard(m) {
  const paid = !m.last_paid_height ? "never" : tip ? `${fmt(tip - m.last_paid_height)} blocks ago` : `block ${fmt(m.last_paid_height)}`;
  const [k, l] = nodeStatus(m);
  const facts = [
    ["Address", el("span", { class: "mono", text: m.service || "–" })],
    ["Version", m.version || "–"],
    ["Location", place(m) || "–"],
    ["Hosting", m.as_org || "–"],
    ["Uptime, 7 days", m.uptime_7d != null ? pct(m.uptime_7d) : "–"],
    ["Last paid", paid],
  ];
  return el("div", { class: "pop" },
    el("div", { class: "pop-head" }, badge(k, l), poseBadge(m), evoPill(m)),
    el("a", { class: "mono pop-id", href: href(`/mn/${m.protx}`), text: shortHash(m.protx, 12) }),
    el("dl", {}, ...facts.flatMap(([a, b]) => [el("dt", { text: a }), el("dd", {}, b)])),
    el("a", { class: "btn", href: href(`/mn/${m.protx}`), text: "Open masternode" }));
}

/** A masternode's details: a popup on wide screens, the bottom sheet on phones (a popup would hide under it). */
function openPopup(m) {
  popup?.remove();
  if (phone()) {
    // Back returns to the list this node was picked from, if it's still there.
    const back = () => {
      select("");
      if (group) { nodeBox.hidden = true; renderGroup(); groupBox.querySelector("button")?.focus(); } else setMode("peek");
    };
    const backButton = el("button", { class: "link", type: "button", onclick: back, text: group ? "← Back to the list" : "Close" });
    nodeBox.replaceChildren(el("div", { class: "mp-row" }, backButton), nodeCard(m));
    nodeBox.hidden = false;
    groupBox.hidden = true;
    setMode("detail");
    select(m.protx);
    backButton.focus({ preventScroll: true });
    // Keep the node in view above the sheet.
    map.easeTo({ center: [m.lon, m.lat], zoom: Math.max(map.getZoom(), 5), offset: [0, -sheet.visible() / 2], duration: 400 });
    return;
  }
  const p = new Popup({ offset: 12, maxWidth: "320px", className: "mp-popup", focusAfterOpen: false })
    .setLngLat([m.lon, m.lat]).setDOMContent(nodeCard(m)).addTo(map);
  p.on("close", () => { if (popup === p) { popup = null; select(""); } });
  popup = p;
  select(m.protx);
}

function select(protx) {
  state.protx = protx;
  if (map.getLayer("mn-selected")) map.setFilter("mn-selected", ["==", ["get", "protx"], protx || "-"]);
  remember();
}

// ---- panel ----

const panel = document.getElementById("panel");
const head = document.getElementById("panel-head");
const body = document.getElementById("panel-body");
const handle = document.getElementById("panel-handle");
// Phones: the panel is a bottom sheet (sheet.js). What it shows is its mode: "peek" (search and
// headline), "open" (all the controls) or "detail" (one place or masternode). How far it's pulled up is
// its rest: collapsed, half or full. Tap the handle to open or fold it; drag or flick it anywhere.
const sheet = bottomSheet({
  panel, handle, head, body, active: phone,
  // Dragged down to the edge, it folds to the search; pulled up from there, the controls show.
  onRest: (rest, byUser) => {
    if (!byUser) return;
    if (rest === "collapsed" && panel.dataset.mode !== "peek") setMode("peek", false);
    else if (rest !== "collapsed" && panel.dataset.mode === "peek") setMode("open", false);
  },
});
handle.addEventListener("click", () => setMode(panel.dataset.mode === "peek" ? "open" : "peek"));

/** Shows `mode`; on phones the sheet moves to its usual rest for it, unless `move` is false. */
function setMode(mode, move = true) {
  // Leaving a masternode's details drops its selection, and the place list it came from shows again.
  if (mode !== "detail" && !nodeBox.hidden) {
    nodeBox.hidden = true;
    groupBox.hidden = !group;
    select("");
  }
  panel.dataset.mode = mode;
  handle.setAttribute("aria-expanded", String(mode !== "peek"));
  if (move) sheet.snap(mode === "peek" ? "collapsed" : "half");
}
panel.addEventListener("keydown", (e) => { if (e.key === "Escape" && phone() && panel.dataset.mode !== "peek") { setMode("peek"); handle.focus(); } });

// Showing a place or folding it away: the sheet follows on phones, the floating panel stays as it is.
const showDetails = () => setMode(phone() ? "detail" : "open");
const foldDetails = () => { if (phone()) setMode("peek"); };

const headline = el("p", { class: "mp-headline" });
const chips = el("div", { class: "chips", role: "group", "aria-label": "Show or hide categories" });
const groupBox = el("section", { class: "mp-group", hidden: true, "aria-live": "polite" });
const nodeBox = el("section", { class: "mp-node", hidden: true, "aria-live": "polite" });
const foot = el("p", { class: "mp-foot" });

function refilter() {
  group = null;
  render();
}

const pick = (name, label, options) => filterSelect(state, name, label, options, refilter, `Any ${label.toLowerCase()}`);

function buildPanel() {
  const filtersButton = el("button", { class: "btn mp-filters-button", type: "button", onclick: () => setMode("open"), text: "Filters" });
  head.replaceChildren(el("div", { class: "mp-search-row" }, searchBox(state, refilter, "Search ProTx, IP, city, hosting", "Search masternodes"), filtersButton), headline);
  const layers = segmented([["mn", "Masternodes"], ["nodes", geo ? "All nodes" : "All nodes (soon)", !geo]], state.layer,
    (v) => { state.layer = v; refilter(); }, "Show");
  const colors = segmented(Object.entries(MODES).map(([k, m]) => [k, m.label]), state.color,
    (v) => { state.color = v; state.hidden.clear(); render(); }, "Color by");
  body.replaceChildren(
    el("div", { class: "mp-row" }, el("span", { class: "mp-label", text: "Show" }), layers),
    el("div", { class: "mp-row mn-only" }, el("span", { class: "mp-label", text: "Color" }), colors),
    chips,
    el("div", { class: "mp-filters mn-only" }, pick("type", "Type", TYPE_OPTIONS), pick("version", "Version", versionOptions(rows)),
      pick("country", "Country", countryOptions(rows)), pick("host", "Hosting", hostOptions(rows, state.host)),
      pick("dkg", "DKG readiness", DKG_OPTIONS)),
    groupBox, nodeBox, foot);
}

function renderChips() {
  if (state.layer === "nodes") {
    chips.replaceChildren(
      el("span", { class: "chip static" }, el("i", { class: "t-s1" }), "Masternodes"),
      el("span", { class: "chip static" }, el("i", { class: "t-s3" }), "Other full nodes"));
    return;
  }
  const mode = MODES[state.color];
  const counts = new Map(countBy(filtered(), mode.key));
  chips.replaceChildren(...mode.order.filter((k) => counts.get(k)).map((k) => {
    const on = !state.hidden.has(k);
    return el("button", {
      type: "button", class: "chip", "aria-pressed": String(on), title: on ? `Hide ${k}` : `Show ${k}`,
      onclick: () => { if (on) state.hidden.add(k); else state.hidden.delete(k); refilter(); },
    }, el("i", { class: `t-${mode.tone(k)}` }), el("span", { text: k }), el("b", { class: "num", text: fmt(counts.get(k)) }));
  }));
}

function showGroup(list) {
  group = list;
  renderGroup();
  showDetails();
}

const groupHead = (title, onClose) =>
  el("div", { class: "mp-row" }, el("h2", { text: title }), el("button", { class: "link", type: "button", onclick: onClose, text: "Close" }));

function showCity(p) {
  group = null;
  groupBox.hidden = false;
  groupBox.replaceChildren(
    groupHead((!p.cluster && place(geo.points[p.i])) || "This area", () => { groupBox.hidden = true; foldDetails(); }),
    el("p", { class: "sub", text: `${fmt(p.nodes)} reachable nodes: ${fmt(p.masternodes)} masternodes and ${fmt(p.nodes - p.masternodes)} other full nodes.` }));
  showDetails();
}

function renderGroup() {
  if (state.layer === "nodes") return;
  groupBox.hidden = !group;
  if (!group) return;
  const rank = (m) => REACH.indexOf(nodeStatus(m)[1]);
  const sorted = [...group].sort((a, b) => rank(a) - rank(b) || (a.service || "").localeCompare(b.service || ""));
  const places = new Set(group.map(place));
  const title = places.size === 1 ? place(group[0]) || "Same location" : `${places.size} places nearby`;
  const reach = group.filter((m) => nodeStatus(m)[1] === "Reachable").length;
  groupBox.replaceChildren(
    groupHead(title, () => { group = null; renderGroup(); foldDetails(); }),
    el("p", { class: "sub", text: `${fmt(group.length)} masternodes, ${fmt(reach)} reachable.` }),
    el("ul", { class: "mn-list" }, ...sorted.map((m) => {
      const [k, l] = nodeStatus(m);
      return el("li", {}, el("button", { type: "button", class: "mn-row", onclick: () => openPopup(m) },
        el("span", { class: `badge ${k}`, title: l, "aria-label": l }, el("i")),
        el("span", { class: "mono", text: m.service || shortHash(m.protx) }),
        evoPill(m),
        m.status === "pose_banned" ? el("span", { class: "pill banned", text: "PoSe-banned" }) : null,
        el("span", { class: "muted", text: m.version || "–" })));
    })));
}

function renderHeadline() {
  if (state.layer === "nodes") {
    const pts = geo?.points || [];
    const total = pts.reduce((a, p) => a + p.nodes, 0);
    const mns = pts.reduce((a, p) => a + p.masternodes, 0);
    headline.replaceChildren(el("b", { class: "num", text: fmt(total) }), ` reachable nodes in ${fmt(pts.length)} places; ${fmt(mns)} are masternodes.`);
    foot.textContent = geo?.unlocated ? `${fmt(geo.unlocated)} reachable nodes on Tor or I2P have no location, so they aren't drawn.` : "";
    return;
  }
  // The same masternodes the Reachable chip counts (PoSe-banned ones answer too, sometimes), then split.
  const list = filtered();
  const answered = (ms) => ms.filter((m) => m.reachable === true).length;
  const enabled = list.filter((m) => m.status === "enabled");
  const banned = list.filter((m) => m.status !== "enabled");
  headline.replaceChildren(el("b", { class: "num", text: fmt(answered(list)) }), ` of ${fmt(list.length)} masternodes answered the latest crawl`,
    banned.length
      ? `: ${fmt(answered(enabled))} of ${fmt(enabled.length)} enabled and ${fmt(answered(banned))} of ${fmt(banned.length)} PoSe-banned.`
      : ".");
  const missing = list.filter((m) => m.lat == null).length;
  // On the list, one visible chip of the reachability or PoSe mode is that filter.
  const only = (mode) => {
    const shown = state.color === mode ? MODES[mode].order.filter((k) => !state.hidden.has(k)) : [];
    return shown.length === 1 ? shown[0] : "";
  };
  const query = filterQuery({ ...state, reach: only("reach") || state.reach, status: only("pose") || state.status });
  foot.replaceChildren(
    missing ? `${fmt(missing)} aren't on the map: no address registered, or not crawled yet. ` : "",
    el("a", { href: href(`/masternodes${query}`), text: "Open these as a list" }));
}

function remember() {
  rememberParams(state, {
    layer: state.layer === "mn" ? "" : state.layer,
    color: state.color === "reach" ? "" : state.color,
    hide: [...state.hidden].join(","),
    protx: state.protx,
  });
}

function render() {
  panel.dataset.layer = state.layer;
  const list = visible();
  map.getSource("mn").setData(mnFeatures(state.layer === "mn" ? list : []));
  map.getSource("nodes").setData(state.layer === "nodes" ? nodeFeatures() : collection([]));
  if (state.layer !== "mn") popup?.remove();
  if (popup && !list.some((m) => m.protx === state.protx)) popup.remove();
  renderChips();
  renderHeadline();
  renderGroup();
  remember();
  // Cluster ids change with the data: drop the donuts now, rebuild once the source has re-clustered.
  refreshClusters(true);
}

function fit(list) {
  const pts = list.filter((m) => m.lat != null);
  if (!pts.length) return;
  const b = new LngLatBounds();
  for (const m of pts) b.extend([m.lon, m.lat]);
  const wide = matchMedia("(min-width: 900px)").matches;
  map.fitBounds(b, { padding: wide ? { top: 60, bottom: 60, left: 420, right: 70 } : 30, maxZoom: 5, duration: 0 });
}

async function main() {
  headline.textContent = "Loading the network…";
  head.replaceChildren(headline);
  try {
    const [data, g] = await Promise.all([api(`${net}/masternodes`), api(`${net}/geo`).catch(() => null)]);
    rows = data.masternodes;
    byProtx = new Map(rows.map((m) => [m.protx, m]));
    tip = data.tip;
    geo = g;
  } catch (e) {
    headline.textContent = `Couldn't load the masternode list: ${e.message}`;
    return;
  }
  if (!geo) state.layer = "mn";
  assignVersionTones();
  if (!map.loaded()) await new Promise((r) => map.once("load", r));
  addDataLayers();
  buildPanel();
  render();
  const m = state.protx && byProtx.get(state.protx);
  if (m?.lat != null) {
    map.jumpTo({ center: [m.lon, m.lat], zoom: 6 });
    openPopup(m);
  } else {
    if (state.protx) select("");
    fit(visible());
  }
}

main();
