// Masternode map: every masternode with a geolocated address, clustered, colored by status,
// version or type. Same data and filters as the masternode list, so the two link to each other.
import { api, chrome, currentNet, el, fmt, pct, compact, href, countryName, flag, shortHash, badge, nodeStatus, segmented,
  MN_FILTERS, mnFilters, mnMatches, rememberParams, filterQuery, filterControls, countBy } from "./common.js";
import { barList } from "./charts.js";

chrome("map");
const net = currentNet();
const L = window.L;

const params = new URLSearchParams(location.search);
const state = { ...mnFilters(params), color: params.get("color") || "status", protx: params.get("protx") || "" };
let rows = [];
let tip = null;
let markers = new Map(); // protx -> marker, for what's on the map now
let group = null; // masternodes sharing one location, listed in the panel after a cluster click

const CONTINENTS = { AF: "Africa", AN: "Antarctica", AS: "Asia", EU: "Europe", NA: "North America", OC: "Oceania", SA: "South America" };

// A color mode maps a masternode to a legend category and a category to a tone. Tones are the
// CSS color roles (`--good`, `--s1`, …), so markers, clusters and the legend agree in both themes.
const STATUS_TONES = { Reachable: "good", Unreachable: "critical", "PoSe-banned": "serious", "Not checked": "axis" };
const MODES = {
  status: { label: "Status", key: (m) => nodeStatus(m)[1], order: Object.keys(STATUS_TONES), tone: (k) => STATUS_TONES[k] },
  version: { label: "Version" }, // filled in from the data by assignVersionTones()
  type: { label: "Type", key: (m) => m.type, order: ["Regular", "Evo"], tone: (k) => (k === "Evo" ? "s2" : "s1") },
};
if (!MODES[state.color]) state.color = "status";

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

// ---- map ----

const map = L.map("map", { minZoom: 2, maxZoom: 12, worldCopyJump: true, zoomSnap: 0.5 }).setView([30, 10], 2);
L.tileLayer("https://tile.openstreetmap.org/{z}/{x}/{y}.png", {
  maxZoom: 19,
  referrerPolicy: "strict-origin-when-cross-origin", // OSM's tile policy wants a referrer
  attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors',
}).addTo(map);

const clusters = L.markerClusterGroup({
  chunkedLoading: true,
  showCoverageOnHover: false,
  zoomToBoundsOnClick: false,
  spiderfyOnMaxZoom: false,
  maxClusterRadius: 44,
  iconCreateFunction: clusterIcon,
}).addTo(map);

// Geolocation is city-level, so hundreds of masternodes can share one point (a data center's
// city). Clicking such a cluster lists them in the panel instead of exploding a huge spiral.
clusters.on("clusterclick", (e) => {
  const c = e.layer;
  const b = c.getBounds();
  if (!b.getNorthEast().equals(b.getSouthWest()) && map.getZoom() < map.getMaxZoom()) {
    c.zoomToBounds({ padding: [40, 40] });
  } else if (c.getChildCount() <= 12) {
    c.spiderfy();
  } else {
    showGroup(c.getAllChildMarkers().map((mk) => mk.mn));
  }
});

/** Cluster badge: a donut of its members' tones, in legend order, around the count. */
function clusterIcon(cluster) {
  const kids = cluster.getAllChildMarkers();
  const tones = new Map();
  for (const k of kids) tones.set(k.options.tone, (tones.get(k.options.tone) || 0) + 1);
  const mode = MODES[state.color];
  const stops = [];
  let at = 0;
  for (const tone of new Set(mode.order.map(mode.tone))) {
    const n = tones.get(tone);
    if (!n) continue;
    const end = at + (n / kids.length) * 100;
    stops.push(`var(--${tone}) ${at.toFixed(2)}% ${end.toFixed(2)}%`);
    at = end;
  }
  const n = kids.length;
  const size = n < 10 ? 30 : n < 100 ? 36 : n < 1000 ? 44 : 52;
  const ring = el("div", { class: "cluster", style: { width: `${size}px`, height: `${size}px`, background: `conic-gradient(${stops.join(", ")})` } },
    el("span", { text: compact(n) }));
  return L.divIcon({ html: ring, className: "cluster-icon", iconSize: [size, size] });
}

function markerFor(m) {
  const mode = MODES[state.color];
  const tone = mode.tone(mode.key(m));
  const mk = L.circleMarker([m.lat, m.lon], { radius: m.type === "Evo" ? 6.5 : 5, weight: 1.5, fillOpacity: 1, className: `mk t-${tone}`, tone });
  mk.mn = m;
  mk.bindPopup(() => popup(m), { minWidth: 250, maxWidth: 320 });
  trackPopup(mk, m);
  return mk;
}

/** The open popup's masternode goes into the URL, so the view can be shared. */
function trackPopup(layer, m) {
  layer.on("popupopen", () => { state.protx = m.protx; remember(); });
  layer.on("popupclose", () => { if (state.protx === m.protx) { state.protx = ""; remember(); } });
}

function popup(m) {
  const [k, l] = nodeStatus(m);
  const place = m.country ? `${flag(m.country)} ${[m.city, countryName(m.country)].filter(Boolean).join(", ")}` : "–";
  const paid = !m.last_paid_height ? "never" : tip ? `${fmt(tip - m.last_paid_height)} blocks ago` : `block ${fmt(m.last_paid_height)}`;
  const facts = [
    ["Address", el("span", { class: "mono", text: m.service || "–" })],
    ["Version", m.version || "–"],
    ["Location", place],
    ["Hosting", m.as_org ? `${m.as_org}${m.asn ? ` (AS${m.asn})` : ""}` : "–"],
    ["Uptime, 7 days", m.uptime_7d != null ? pct(m.uptime_7d) : "–"],
    ["Last paid", paid],
  ];
  return el("div", { class: "pop" },
    el("div", { class: "pop-head" }, badge(k, l), m.type === "Evo" ? el("span", { class: "pill evo", text: "Evo" }) : null),
    el("a", { class: "mono pop-id", href: href(`/mn/${m.protx}`), text: shortHash(m.protx, 12) }),
    el("dl", {}, ...facts.flatMap(([a, b]) => [el("dt", { text: a }), el("dd", {}, b)])),
    el("a", { href: href(`/mn/${m.protx}`), text: "Masternode details →" }));
}

function fitAll() {
  if (!markers.size) return;
  map.fitBounds(L.latLngBounds([...markers.values()].map((mk) => mk.getLatLng())).pad(0.1), { maxZoom: 6 });
}

/** Opens one masternode's popup without exploding the cluster it's in. */
function focus(protx) {
  const mk = markers.get(protx);
  if (!mk) return false;
  map.setView(mk.getLatLng(), Math.max(map.getZoom(), 7));
  const p = L.popup({ minWidth: 250, maxWidth: 320 }).setLatLng(mk.getLatLng()).setContent(popup(mk.mn));
  trackPopup(p, mk.mn);
  p.openOn(map);
  return true;
}

// ---- panel ----

const panel = document.getElementById("panel");
const sections = {
  filters: el("div", { class: "map-filters" }),
  color: el("div", { class: "panel-sec" }),
  group: el("div", { class: "panel-sec group", hidden: true }),
  summary: el("p", { class: "sub panel-summary" }),
  stats: el("div"),
};

function buildPanel() {
  sections.filters.replaceChildren(...filterControls(rows, state, () => { group = null; render(); }));
  sections.color.replaceChildren(
    el("div", { class: "panel-row" },
      el("h3", { text: "Color by" }),
      segmented(Object.entries(MODES).map(([k, m]) => [k, m.label]), state.color, (v) => { state.color = v; render(); }, "Color by")),
    el("div", { class: "legend map-legend", id: "map-legend" }));
  panel.replaceChildren(sections.filters, sections.color, sections.group, sections.summary,
    el("div", { class: "panel-row" }, el("button", { class: "btn", type: "button", onclick: fitAll, text: "Fit map to results" })),
    sections.stats);
}

function renderLegend(placed) {
  const mode = MODES[state.color];
  const counts = new Map(countBy(placed, mode.key));
  document.getElementById("map-legend").replaceChildren(...mode.order.filter((k) => counts.get(k)).map((k) =>
    el("span", {}, el("i", { class: `t-${mode.tone(k)}` }), k, el("b", { class: "num", text: fmt(counts.get(k)) }))));
}

function showGroup(list) {
  group = list;
  renderGroup();
  sections.group.scrollIntoView({ block: "nearest", behavior: "smooth" });
}

const STATUS_RANK = Object.fromEntries(Object.keys(STATUS_TONES).map((k, i) => [k, i]));

function renderGroup() {
  sections.group.hidden = !group;
  if (!group) return;
  const m0 = group[0];
  const place = m0.country ? `${flag(m0.country)} ${[m0.city, countryName(m0.country)].filter(Boolean).join(", ")}` : "Same location";
  const sorted = [...group].sort((a, b) => STATUS_RANK[nodeStatus(a)[1]] - STATUS_RANK[nodeStatus(b)[1]] || (a.service || "").localeCompare(b.service || ""));
  sections.group.replaceChildren(
    el("div", { class: "panel-row" }, el("h2", { text: place }),
      el("button", { class: "more", type: "button", onclick: () => { group = null; renderGroup(); }, text: "Close" })),
    el("p", { class: "sub", text: `${fmt(group.length)} masternodes geolocate to this point.` }),
    el("ul", { class: "mn-list" }, ...sorted.map((m) => {
      const [k, l] = nodeStatus(m);
      return el("li", {},
        el("span", { class: `badge ${k}`, title: l, "aria-label": l }, el("i")),
        el("a", { class: "mono", href: href(`/mn/${m.protx}`), text: m.service || shortHash(m.protx) }),
        m.type === "Evo" ? el("span", { class: "pill evo", text: "Evo" }) : null,
        el("span", { class: "muted", text: m.version || "–" }));
    })));
}

function renderStats(shown, placed) {
  const missing = shown.length - placed.length;
  sections.summary.textContent = `${fmt(placed.length)} of ${fmt(shown.length)} masternodes on the map` +
    (missing ? `; ${fmt(missing)} have no geolocated address (none registered, or not crawled yet).` : ".");
  const box = (title, list, label) => {
    const body = el("div");
    barList(body, list, { label, limit: 6 });
    return el("div", { class: "panel-sec" }, el("h3", { text: title }), body);
  };
  sections.stats.replaceChildren(
    box("Countries", countBy(placed, (m) => m.country || "Unknown"), (c) => (c === "Unknown" ? c : `${flag(c)} ${countryName(c)}`)),
    box("Hosting", countBy(placed, (m) => m.as_org || "Unknown")),
    box("Continents", countBy(placed, (m) => CONTINENTS[m.continent] || "Unknown")));
}

function remember() {
  rememberParams({
    ...Object.fromEntries(MN_FILTERS.map((k) => [k, state[k]])),
    color: state.color === "status" ? "" : state.color,
    protx: state.protx,
  });
  document.getElementById("to-list").href = href(`/masternodes${filterQuery(state)}`);
}

function render() {
  const shown = rows.filter((m) => mnMatches(m, state));
  const placed = shown.filter((m) => m.lat != null && m.lon != null);
  map.closePopup();
  clusters.clearLayers();
  markers = new Map(placed.map((m) => [m.protx, markerFor(m)]));
  clusters.addLayers([...markers.values()]);
  renderLegend(placed);
  renderGroup();
  renderStats(shown, placed);
  remember();
}

async function main() {
  panel.replaceChildren(el("div", { class: "empty", text: "Loading masternodes…" }));
  try {
    const data = await api(`${net}/masternodes`);
    rows = data.masternodes;
    tip = data.tip;
  } catch (e) {
    panel.replaceChildren(el("div", { class: "banner", text: `Couldn't load the masternode list: ${e.message}` }));
    return;
  }
  const countries = new Set(rows.map((m) => m.country).filter(Boolean)).size;
  document.getElementById("lede").textContent =
    `${fmt(rows.length)} registered masternodes in ${fmt(countries)} countries, each placed by the city its IP address geolocates to.`;
  assignVersionTones();
  buildPanel();
  const wanted = state.protx;
  render();
  if (!(wanted && focus(wanted))) fitAll();
}

main();
