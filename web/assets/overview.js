import { api, chrome, currentNet, el, fmt, pct, href, countryName, flag, ago, segmented } from "./common.js";
import { Palette, timeChart, svg } from "./charts.js";
import { worldMap } from "./worldmap.js";

chrome("overview");
const net = currentNet();

const SCOPES = [["mn", "Masternodes"], ["evo", "Evonodes"], ["nonmn", "Other full nodes"], ["all", "All nodes"]];
const RANGES = [["1", "24h"], ["7", "7d"], ["30", "30d"], ["90", "90d"], ["365", "1y"]];
const ADDRESS_NETWORKS = { ipv4: "IPv4", ipv6: "IPv6", onion: "Tor", i2p: "I2P", cjdns: "CJDNS" };
const params = new URLSearchParams(location.search);
let scope = SCOPES.some(([s]) => s === params.get("scope")) ? params.get("scope") : "mn";
let days = RANGES.some(([d]) => d === params.get("days")) ? params.get("days") : "30";

const versionPalette = new Palette();
const sizePalette = new Palette();
let versionsChart, sizeChart, summary;
const $ = (id) => document.getElementById(id);

function remember() {
  const u = new URL(location.href);
  u.searchParams.set("scope", scope);
  u.searchParams.set("days", days);
  history.replaceState(null, "", u);
}

const noun = () => ({ all: "nodes", mn: "masternodes", evo: "evonodes", nonmn: "other full nodes" })[scope];
const total = (rows) => rows.reduce((a, r) => a + r[1], 0);

/** "Hetzner Online GmbH" from "AS24940 Hetzner Online GmbH"; one provider can run several ASNs. */
const orgOf = (asn) => asn.replace(/^AS\d+\s*/, "") || asn;
function byOrg(rows) {
  const m = new Map();
  for (const [k, n] of rows) {
    const org = k === "Unknown" ? k : orgOf(k);
    const e = m.get(org) || { n: 0, asns: [] };
    e.n += n;
    if (k !== "Unknown") e.asns.push(k.split(" ")[0]);
    m.set(org, e);
  }
  return [...m.entries()].map(([k, e]) => [k, e.n, e.asns]).sort((a, b) => b[1] - a[1]);
}

// ---- shared pieces ----

/** A 100% bar of the largest groups, each in its own color (color(key, rank)), with a legend that names them. */
function shareBar(bar, legend, rows, { color, label = (k) => k, limit = 5 }) {
  const sum = total(rows);
  const top = rows.slice(0, limit);
  const rest = sum - total(top);
  const parts = [...top.map(([k, n], i) => ({ n, c: color(k, i), l: label(k) })), ...(rest > 0 ? [{ n: rest, c: "var(--other)", l: "Everything else" }] : [])];
  bar.replaceChildren(...parts.map((p) => el("div", { title: `${p.l}: ${fmt(p.n)} (${pct(p.n / sum)})`, style: { flexGrow: String(p.n), background: p.c } })));
  bar.setAttribute("role", "img");
  bar.setAttribute("aria-label", parts.map((p) => `${p.l} ${pct(p.n / sum)}`).join(", "));
  legend?.replaceChildren(...parts.map((p) => el("span", {}, el("i", { style: { background: p.c } }), el("span", { text: p.l }),
    el("b", { class: "num", text: pct(p.n / sum) }))));
}

/**
 * A donut of the largest groups (color(key, rank)) and the rest, with `center` ([value, caption]) in
 * the hole. Each slice names itself on hover; the ranked list beside it carries the numbers.
 */
function donut(root, rows, { color, label = (k) => k, limit = 5, center }) {
  const sum = total(rows) || 1;
  const top = rows.slice(0, limit);
  const rest = sum - total(top);
  const parts = [...top.map(([k, n], i) => ({ n, c: color(k, i), l: label(k) })), ...(rest > 0 ? [{ n: rest, c: "var(--other)", l: "Everything else" }] : [])];
  const R = 74, r = 50, C = 80;
  const at = (a, rad) => `${(C + rad * Math.sin(a)).toFixed(2)} ${(C - rad * Math.cos(a)).toFixed(2)}`;
  root.setAttribute("viewBox", "0 0 160 160");
  root.setAttribute("role", "img");
  root.setAttribute("aria-label", parts.map((p) => `${typeof p.l === "string" ? p.l : p.l.textContent} ${pct(p.n / sum)}`).join(", "));
  root.replaceChildren();
  let a0 = 0;
  for (const p of parts) {
    const a1 = a0 + (p.n / sum) * 2 * Math.PI;
    // A full circle can't be one arc: draw it as two halves.
    const d = p.n >= sum
      ? `M${at(0, R)}A${R} ${R} 0 1 1 ${at(Math.PI, R)}A${R} ${R} 0 1 1 ${at(0, R)}M${at(0, r)}A${r} ${r} 0 1 0 ${at(Math.PI, r)}A${r} ${r} 0 1 0 ${at(0, r)}Z`
      : `M${at(a0, R)}A${R} ${R} 0 ${a1 - a0 > Math.PI ? 1 : 0} 1 ${at(a1, R)}L${at(a1, r)}A${r} ${r} 0 ${a1 - a0 > Math.PI ? 1 : 0} 0 ${at(a0, r)}Z`;
    const slice = svg("path", { d, fill: p.c, stroke: "var(--surface)", "stroke-width": 2, "fill-rule": "evenodd" });
    const t = svg("title");
    t.textContent = `${typeof p.l === "string" ? p.l : p.l.textContent}: ${fmt(p.n)} (${pct(p.n / sum)})`;
    slice.append(t);
    root.append(slice);
    a0 = a1;
  }
  if (center) {
    const v = svg("text", { x: C, y: C + 4, "text-anchor": "middle", class: "donut-value" });
    v.textContent = center[0];
    const c = svg("text", { x: C, y: C + 22, "text-anchor": "middle", class: "donut-caption" });
    c.textContent = center[1];
    root.append(v, c);
  }
}

/** Ranked rows with a share bar each, dotted with the same colors as the chart beside them. */
function ranked(box, rows, { label = (k) => k, title, color, limit = 8 }) {
  const sum = total(rows);
  let expanded = false;
  const render = () => {
    const shown = expanded ? rows : rows.slice(0, limit);
    const max = Math.max(1, ...shown.map((r) => r[1]));
    const list = el("ol", { class: "ranked" }, ...shown.map((r, i) => el("li", { title: title?.(r) || null },
      el("i", { style: { background: color(r[0], i) } }),
      el("span", { class: "k" }, label(r[0])),
      el("span", { class: "track" }, el("span", { style: { width: `${(r[1] / max) * 100}%` } })),
      el("span", { class: "v num", text: fmt(r[1]) }),
      el("span", { class: "p num", text: pct(r[1] / sum) }))));
    const parts = [rows.length ? list : el("p", { class: "empty", text: "No data yet." })];
    if (rows.length > limit) {
      parts.push(el("button", { class: "link", type: "button", onclick: () => { expanded = !expanded; render(); },
        text: expanded ? "Show fewer" : `Show all ${rows.length}` }));
    }
    box.replaceChildren(...parts);
  };
  render();
}

/** The five largest groups get a series color, in rank order; the rest and "Unknown" the axis gray. */
const rankColor = (k, i) => (k === "Unknown" || i >= 5 ? "var(--axis)" : `var(--s${i + 1})`);

// ---- hero ----

function renderHero() {
  const c = summary.crawl, prev = summary.day_ago, all = summary.dists.all || {}, mn = summary.dists.mn || {};
  const type = Object.fromEntries(mn.type || []);
  const status = Object.fromEntries(mn.status || []);
  const diff = prev ? c.reachable - prev.reachable : null;
  $("hero-title").textContent = `${fmt(c.reachable)} Dash nodes answered our last crawl`;
  $("hero-sub").textContent = `Among them ${fmt(c.mn_reachable)} of the ${fmt(c.mn_total)} enabled masternodes. ` +
    (diff ? `That's ${fmt(Math.abs(diff))} ${diff > 0 ? "more" : "fewer"} nodes than a day ago. ` : "") +
    `Crawled ${ago(c.finished)}, at block ${fmt(c.tip_height)}.`;
  // Every stat leads somewhere: the list it counts, or the section that explains it.
  const stat = (value, label, note, to, tone) => el("div", { class: tone ? `stat-${tone}` : null },
    el("dt", { text: label }),
    el("dd", { class: "num" }, el("a", { href: to, class: "stat-link" }, value), note ? el("small", { text: note }) : null));
  const reach = Object.fromEntries(mn.reachable || []);
  const answered = reach.yes || 0, silent = reach.no || 0;
  const share = answered + silent ? answered / (answered + silent) : null;
  $("hero-stats").replaceChildren(
    stat(pct(share), "Masternodes reachable", silent ? `${fmt(silent)} didn't answer` : "all answered",
      href(silent ? "/masternodes?reach=Unreachable" : "/masternodes"), share == null ? null : share >= 0.98 ? "good" : share >= 0.9 ? "warning" : "critical"),
    stat(fmt(c.mn_total), "Enabled masternodes", type.Evo ? `${fmt(type.Evo)} of them evonodes` : null, href("/masternodes?status=Enabled")),
    stat(fmt(status.pose_banned || 0), "PoSe-banned", null, href("/masternodes?status=PoSe-banned")),
    stat(fmt((all.country || []).filter(([k]) => k !== "Unknown").length), "Countries", null, "#where"),
    stat(fmt(byOrg(all.asn || []).filter(([k]) => k !== "Unknown").length), "Hosting providers", null, "#where"),
    stat(fmt(c.tried), "Addresses tried", null, "#reach"),
  );
  $("to-map").href = href("/map");
  $("to-list").href = href("/masternodes");
}

async function heroPoints() {
  try {
    return (await api(`${net}/geo`)).points;
  } catch {
    // Older servers: masternodes only.
    const { masternodes } = await api(`${net}/masternodes`);
    const at = new Map();
    for (const m of masternodes) {
      if (m.lat == null || m.reachable !== true) continue;
      const k = `${m.lat},${m.lon}`;
      at.set(k, { lat: m.lat, lon: m.lon, nodes: (at.get(k)?.nodes || 0) + 1 });
    }
    return [...at.values()];
  }
}

// ---- sections ----

const versionOf = (ua) => /^\/Dash Core:([^/(]+)/.exec(ua)?.[1] || ua || "(empty)";

function renderSoftware() {
  const d = summary.dists[scope] || {};
  const versions = (d.version || []).filter(([k]) => k !== "Unknown");
  const unknown = (d.version || []).find(([k]) => k === "Unknown")?.[1] || 0;
  const sum = total(d.version || []);
  versionPalette.assign(versions.slice(0, 5).map(([k]) => k));
  shareBar($("sw-bar"), $("sw-legend"), [...versions, ...(unknown ? [["Unknown", unknown]] : [])].sort((a, b) => b[1] - a[1]), {
    color: (k) => (k === "Unknown" ? "var(--axis)" : versionPalette.color(k)),
    label: (k) => (k === "Unknown" ? "No answer this week" : k),
  });
  const [a, b] = versions;
  let lede = a ? `${a[0]} runs on ${pct(a[1] / sum)} of ${noun()}` : `No ${noun()} answered yet.`;
  if (b) lede += ` and ${b[0]} on ${pct(b[1] / sum)}`;
  if (a) {
    const rest = sum - a[1] - (b?.[1] || 0) - unknown;
    lede += rest > 0 ? `; the other ${pct(rest / sum)} are spread over ${fmt(versions.length - 2)} more versions.` : ".";
  }
  if (unknown) lede += ` ${fmt(unknown)} haven't answered in a week, so their version is unknown.`;
  $("sw-lede").textContent = lede;
  const proto = (d.protocol || []).filter(([k]) => k !== "Unknown");
  $("sw-proto").textContent = proto.length ? `Protocol ${proto.map(([k, n]) => `${k} on ${pct(n / total(proto))}`).join(", ")}.` : "";
  const ua = d.ua || [];
  $("sw-table").replaceChildren(
    el("thead", {}, el("tr", {}, el("th", { text: "Version" }), el("th", { text: "User agent" }), el("th", { class: "r", text: noun()[0].toUpperCase() + noun().slice(1) }), el("th", { class: "r", text: "Share" }))),
    el("tbody", {}, ...ua.map(([k, n]) => el("tr", {},
      el("td", { text: k === "Unknown" ? "Unknown" : versionOf(k) }),
      el("td", {}, k === "Unknown" ? el("span", { class: "muted", text: "no answer this week" }) : el("code", { text: k || "(empty)" })),
      el("td", { class: "r", text: fmt(n) }), el("td", { class: "r", text: pct(n / total(ua)) })))));
}

function renderWhere() {
  const d = summary.dists[scope] || {};
  const orgs = byOrg(d.asn || []);
  const countries = d.country || [];
  const sumO = total(orgs), sumC = total(countries);
  const known = (rows) => rows.filter(([k]) => k !== "Unknown");
  // Masternode scopes link each row to the list filtered to it.
  const listed = scope === "mn" || scope === "evo";
  const toList = (param, value, text) => (listed && value !== "Unknown"
    ? el("a", { class: "quiet", href: href(`/masternodes?${param}=${encodeURIComponent(value)}${scope === "evo" ? "&type=Evo" : ""}`), text })
    : text);
  donut($("host-donut"), orgs, { color: rankColor, center: [fmt(known(orgs).length), "providers"] });
  ranked($("host-list"), orgs, { color: rankColor, title: (r) => r[2].join(", "), label: (k) => toList("host", k, k) });
  const cName = (k) => (k === "Unknown" ? "Unknown" : `${flag(k)} ${countryName(k)}`);
  donut($("country-donut"), countries, { color: rankColor, label: cName, center: [fmt(known(countries).length), "countries"] });
  ranked($("country-list"), countries, { color: rankColor, label: (k) => toList("country", k, cName(k)) });
  // Lead with real providers and countries; "Unknown" (not geolocated yet) only counts in the totals.
  const [o0] = known(orgs), [c0] = known(countries);
  if (!o0) { $("where-lede").textContent = ""; return; }
  $("where-lede").textContent = `${o0[0]} hosts ${pct(o0[1] / sumO)} of ${noun()}, and the three largest providers host ${pct(total(known(orgs).slice(0, 3)) / sumO)}.` +
    (c0 ? ` ${countryName(c0[0])} alone has ${pct(c0[1] / sumC)}.` : "");
}

function renderReach() {
  const d = summary.dists[scope] || {};
  const c = summary.crawl;
  const reach = Object.fromEntries(d.reachable || []);
  const yes = reach.yes || 0, no = reach.no || 0, unchecked = reach.unchecked || 0;
  const rows = [["yes", yes], ["no", no], ["unchecked", unchecked]].filter(([, n]) => n);
  const names = { yes: "Answered", no: "Didn't answer", unchecked: "Not checked" };
  const colors = { yes: "var(--good)", no: "var(--critical)", unchecked: "var(--axis)" };
  $("reach-bar").hidden = !rows.length;
  $("reach-legend").hidden = !rows.length;
  if (rows.length) shareBar($("reach-bar"), $("reach-legend"), rows, { color: (k) => colors[k], label: (k) => names[k] });
  const lede = $("reach-lede");
  if (scope === "nonmn") {
    lede.textContent = `${fmt(total(d.version || []))} other full nodes accept connections. Nodes that only connect out can't be seen from outside, so the real number is higher.`;
  } else if (scope === "all") {
    lede.textContent = `${fmt(c.reachable)} nodes answered out of ${fmt(c.tried)} addresses tried. Counted are nodes that answered at least once this week, plus every enabled masternode.`;
  } else {
    lede.textContent = yes + no
      ? `${pct(yes / (yes + no))} of enabled ${noun()} answered our handshake on their registered address; ${fmt(no)} didn't.`
      : "";
  }
  if (no && scope !== "all") {
    lede.append(" ", el("a", { href: href(`/masternodes?reach=Unreachable${scope === "evo" ? "&type=Evo" : ""}`), text: "See which" }));
  }
  renderNetworks();
}

function renderNetworks() {
  const box = $("networks");
  const nets = summary.networks;
  if (!nets?.length) { box.replaceChildren(); return; }
  box.replaceChildren(
    el("h3", { text: "Address networks" }),
    el("div", { class: "table-wrap" }, el("table", { class: "data" },
      el("thead", {}, el("tr", {}, el("th", { text: "Network" }), el("th", { class: "r", text: "Known addresses" }),
        el("th", { class: "r", text: "Reachable now" }), el("th", { class: "r", text: "Masternodes" }))),
      el("tbody", {}, ...nets.map((n) => el("tr", {},
        el("td", { text: ADDRESS_NETWORKS[n.network] || n.network }),
        el("td", { class: "r", text: fmt(n.known) }),
        n.crawled
          ? el("td", { class: "r", text: fmt(n.reachable) })
          : el("td", { class: "r muted", title: n.reason || "", text: n.reason ? `not crawled: ${n.reason}` : "not crawled" }),
        el("td", { class: "r", text: n.crawled ? fmt(n.masternodes) : "–" })))))));
}

let historyRequest = 0;

async function loadHistory() {
  const mine = ++historyRequest;
  versionsChart?.loading();
  sizeChart?.loading();
  const [v, s] = await Promise.all([
    api(`${net}/history?scope=${scope}&dim=version&days=${days}&top=6`),
    api(`${net}/history?dim=totals&days=${days}`),
  ]);
  if (mine !== historyRequest) return; // a newer scope or range was picked meanwhile
  const labels = { reachable_nodes: "Reachable nodes", masternodes: "Enabled masternodes", masternodes_reachable: "Masternodes reachable" };
  const vopts = { stacked: true, share: true, palette: versionPalette, label: (k) => (k === "Unknown" ? "No answer this week" : k), title: "Version share over time", height: 240 };
  // The three counts sit close together far from zero: an axis that starts at zero flattens them.
  const sopts = { stacked: false, zero: false, palette: sizePalette, label: (k) => labels[k] || k, title: "Network size", height: 220 };
  if (versionsChart) versionsChart.update(v, vopts);
  else versionsChart = timeChart($("versions-chart"), v, vopts);
  if (sizeChart) sizeChart.update(s, sopts);
  else sizeChart = timeChart($("size-chart"), s, sopts);
}

async function loadHistorySafely() {
  try {
    await loadHistory();
  } catch (e) {
    $("banner").replaceChildren(el("div", { class: "banner", text: `Couldn't load history: ${e.message}` }));
  }
}

const GRADE_TONES = { A: "var(--good)", B: "color-mix(in srgb, var(--good) 60%, var(--surface))", C: "var(--warning)", D: "var(--serious)", F: "var(--critical)", failed: "var(--axis)" };
const gradeLabel = (k) => (k === "failed" ? "Didn't answer" : `Grade ${k}`);
const median = (sorted) => (sorted?.length ? sorted[sorted.length >> 1] : null);

/** Speed tests: one grade bar per test location, from /bench (served by the Worker; absent elsewhere). */
async function renderSpeed() {
  const b = await api(`${net}/bench`).catch(() => null);
  const vs = (b?.vantages || []).filter((v) => v.tested);
  $("speed-section").hidden = !vs.length;
  if (!vs.length) return;
  // Failed tests have no score but still count as tests that didn't earn an A or B.
  const all = vs.flatMap((v) => v.scores || []);
  const runs = all.length + vs.reduce((a, v) => a + (v.failed || 0), 0);
  const good = all.filter((x) => x >= 75).length;
  $("speed-lede").textContent = `Once every two weeks we download the latest 50,000 headers and the masternode list from every enabled masternode. ` +
    `${pct(runs ? good / runs : null, 0)} of the latest tests earned an A or B.`;
  $("speed-vantages").replaceChildren(...vs.map((v) => {
    const bar = el("div", { class: "sharebar" });
    const legend = el("div", { class: "share-legend" });
    // Same denominator as the lede: every latest test, the ones that never got an answer too.
    const rows = [...["A", "B", "C", "D", "F"].map((g) => [g, v.grades?.[g] || 0]), ["failed", v.failed || 0]];
    shareBar(bar, legend, rows.filter(([, n]) => n), { color: (k) => GRADE_TONES[k], label: gradeLabel, limit: rows.length });
    return el("div", { class: "speed-vantage" },
      el("h3", { text: v.location || v.vantage }),
      bar, legend,
      el("p", { class: "muted small", text: `${fmt(v.tested)} tested this cycle${v.failed ? `, ${fmt(v.failed)} didn't answer` : ""}. ` +
        `Median ${fmt(median(v.headers_per_sec))} headers/s, ping ${fmt(median(v.ping_ms))} ms. Updated ${ago(v.updated)}.` }));
  }));
}

function renderSections() {
  renderSoftware();
  renderWhere();
  renderReach();
}

function renderControls() {
  $("scope-bar").replaceChildren(
    el("span", { class: "muted", text: "Describing" }),
    segmented(SCOPES, scope, (v) => { scope = v; remember(); renderSections(); loadHistorySafely(); }, "Which nodes"));
  $("sw-range").replaceChildren(segmented(RANGES, days, (v) => { days = v; remember(); loadHistorySafely(); }, "Time range"));
}

async function main() {
  try {
    summary = await api(`${net}/summary`);
  } catch (e) {
    $("hero-title").textContent = "The network data didn't load";
    $("hero-sub").textContent = `${e.message}. Reload the page to try again.`;
    return;
  }
  if (!summary.crawl) {
    $("hero-title").textContent = "The first crawl hasn't finished yet";
    $("hero-sub").textContent = "It starts once our dashd has synced and loaded the masternode list. Check back in a few minutes.";
    return;
  }
  renderHero();
  heroPoints().then((points) => worldMap($("hero-map"), points)).catch(() => {});
  renderControls();
  renderSections();
  renderSpeed();
  await loadHistorySafely();
}

main();
