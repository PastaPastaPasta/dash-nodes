import { api, chrome, currentNet, el, fmt, pct, segmented, countryName, flag, ago, dateTime, badge, tile, card } from "./common.js";
import { Palette, timeChart, barList } from "./charts.js";

chrome("overview");
const net = currentNet();

const SCOPES = [["all", "All nodes"], ["mn", "Masternodes"], ["evo", "Evonodes"], ["nonmn", "Non-masternodes"]];
const RANGES = [["1", "24h"], ["7", "7d"], ["30", "30d"], ["90", "90d"], ["365", "1y"]];
const params = new URLSearchParams(location.search);
let scope = SCOPES.some(([s]) => s === params.get("scope")) ? params.get("scope") : "mn";
let days = RANGES.some(([d]) => d === params.get("days")) ? params.get("days") : "30";
let share = params.get("view") !== "count";

const versionPalette = new Palette();
const sizePalette = new Palette();
let versionsChart, sizeChart, summary;

function remember() {
  const u = new URL(location.href);
  u.searchParams.set("scope", scope);
  u.searchParams.set("days", days);
  u.searchParams.set("view", share ? "share" : "count");
  history.replaceState(null, "", u);
}

const scopeNoun = () => ({ all: "nodes", mn: "masternodes", evo: "evonodes", nonmn: "non-masternode nodes" })[scope];

function versionLabel(k) {
  return k === "Unknown" ? "Unknown (not reached in 7 days)" : k;
}

function delta(now, then) {
  if (now == null || then == null) return null;
  const d = now - then;
  if (d === 0) return el("span", { text: "no change in 24h" });
  return el("span", { class: d > 0 ? "delta-up" : "delta-down", text: `${d > 0 ? "+" : "−"}${fmt(Math.abs(d))} in 24h` });
}

function renderTiles() {
  const c = summary.crawl, prev = summary.day_ago, d = summary.dists;
  const mnStatus = Object.fromEntries(d.mn?.status || []);
  const mnType = Object.fromEntries(d.mn?.type || []);
  const versions = (d.mn?.version || []).filter(([k]) => k !== "Unknown");
  const mnTotal = c.mn_total || 0;
  const tiles = [
    tile("Reachable nodes", fmt(c.reachable), delta(c.reachable, prev?.reachable) || `${fmt(c.tried)} addresses tried`),
    tile("Masternodes", fmt(mnTotal),
      `${fmt(mnType.Regular)} regular · ${fmt(mnType.Evo)} evo · ${fmt(mnStatus.pose_banned || 0)} PoSe-banned`),
    tile("Masternodes reachable", pct(mnTotal ? c.mn_reachable / mnTotal : null),
      `${fmt(mnTotal - c.mn_reachable)} didn't answer this crawl`),
  ];
  if (versions.length) {
    const [top, n] = versions[0];
    const next = versions[1];
    tiles.push(tile("Most common masternode version", top,
      `${pct(n / mnTotal)} of masternodes${next ? ` · next ${next[0]} at ${pct(next[1] / mnTotal)}` : ""}`));
  }
  document.getElementById("tiles").replaceChildren(...tiles);
  document.getElementById("lede").textContent =
    `Crawl #${c.id} at ${dateTime(c.started)} (${ago(c.finished)}), chain tip ${fmt(c.tip_height)}.`;
}

function renderDists() {
  const d = summary.dists[scope] || {};
  const box = document.getElementById("dists");
  const cards = [];
  const list = (dim, title, label, limit = 8) => {
    const rows = d[dim];
    if (!rows?.length) return;
    const body = el("div");
    barList(body, rows, { label, limit });
    cards.push(card(title, `${fmt(rows.reduce((a, r) => a + r[1], 0))} ${scopeNoun()}`, body));
  };
  const reach = d.reachable;
  if (reach?.length) {
    const m = Object.fromEntries(reach);
    const yes = m.yes || 0, no = m.no || 0, unchecked = m.unchecked || 0, tot = yes + no + unchecked;
    const parts = [
      { kind: "good", color: "var(--good)", title: "Reachable", label: "Answered this crawl", n: yes },
      { kind: "critical", color: "var(--critical)", title: "Not reachable", label: "Didn't answer", n: no },
    ];
    if (unchecked) parts.push({ kind: "none", color: "var(--axis)", title: "Not checked", label: "Not checked (IPv6)", n: unchecked });
    const body = el("div", {},
      el("div", { class: "share", role: "img", "aria-label": `${yes} reachable, ${no} not reachable` },
        ...parts.map((p) => el("div", { style: { width: `${(p.n / tot) * 100}%`, background: p.color }, title: `${p.title}: ${fmt(p.n)}` }))),
      el("div", { class: "bars" },
        ...parts.map((p) => [el("div", { class: "k" }, badge(p.kind, p.label)), el("div"),
          el("div", { class: "v" }, fmt(p.n), el("small", { text: pct(p.n / tot) }))])),
      el("p", { class: "muted", style: { fontSize: "12px", margin: "10px 0 0" },
        text: scope === "all"
          ? "Out of nodes that answered at least once in the last 7 days, plus every enabled masternode."
          : "Enabled masternodes whose registered Core P2P address answered our handshake." }));
    cards.push(card("Reachability", `${fmt(tot)} ${scopeNoun()}`, body));
  }
  list("version", "Versions", versionLabel);
  list("ua", "User agents", (k) => el("code", { text: k || "(empty)" }));
  list("protocol", "Protocol versions", (k) => k);
  list("country", "Countries", (k) => `${flag(k)} ${countryName(k)}`.trim());
  list("asn", "Hosting networks (ASN)", (k) => k);
  if (scope === "mn" || scope === "evo") {
    list("status", "Registration status", (k) => ({ enabled: "Enabled", pose_banned: "PoSe-banned" })[k] || k);
  }
  if (scope === "all") list("client", "Client software", (k) => k);
  box.replaceChildren(...cards);
}

async function loadHistory() {
  versionsChart?.loading();
  sizeChart?.loading();
  const [v, s] = await Promise.all([
    api(`${net}/history?scope=${scope}&dim=version&days=${days}&top=6`),
    api(`${net}/history?dim=totals&days=${days}`),
  ]);
  const labels = { reachable_nodes: "Reachable nodes", masternodes: "Enabled masternodes", masternodes_reachable: "Masternodes reachable" };
  document.getElementById("versions-title").textContent = `Versions over time · ${SCOPES.find(([k]) => k === scope)[1].toLowerCase()}`;
  const vopts = { stacked: true, share, palette: versionPalette, label: versionLabel, title: "Versions over time" };
  const sopts = { stacked: false, palette: sizePalette, label: (k) => labels[k] || k, title: "Network size" };
  if (versionsChart) versionsChart.update(v, vopts);
  else versionsChart = timeChart(document.getElementById("versions-chart"), v, vopts);
  if (sizeChart) sizeChart.update(s, sopts);
  else sizeChart = timeChart(document.getElementById("size-chart"), s, sopts);
}

async function loadHistorySafely() {
  try {
    await loadHistory();
  } catch (e) {
    document.getElementById("banner").replaceChildren(el("div", { class: "banner", text: `Couldn't load history: ${e.message}` }));
  }
}

function renderFilters() {
  document.getElementById("filters").replaceChildren(
    el("label", {}, "Scope", segmented(SCOPES, scope, (v) => { scope = v; remember(); renderDists(); loadHistorySafely(); }, "Scope")),
    el("label", {}, "Range", segmented(RANGES, days, (v) => { days = v; remember(); loadHistorySafely(); }, "Time range")),
  );
  document.getElementById("versions-mode").replaceChildren(
    segmented([["share", "Share"], ["count", "Count"]], share ? "share" : "count", (v) => { share = v === "share"; remember(); loadHistorySafely(); }, "Values"));
}

async function main() {
  try {
    summary = await api(`${net}/summary`);
  } catch (e) {
    document.getElementById("banner").replaceChildren(el("div", { class: "banner", text: `Couldn't load data: ${e.message}` }));
    return;
  }
  if (!summary.crawl) {
    document.getElementById("banner").replaceChildren(el("div", { class: "banner" },
      el("strong", { text: "No crawl finished yet. " }),
      "The first crawl starts once our dashd has synced and loaded the masternode list; check back in a few minutes."));
    return;
  }
  renderTiles();
  renderFilters();
  renderDists();
  await loadHistorySafely();
}

main();
