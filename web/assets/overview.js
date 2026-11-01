import { api, chrome, currentNet, el, fmt, pct, href, countryName, flag, ago, segmented, DKG_CHECKS } from "./common.js";
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
/** The versions chart: share over time, or share now as a pie. */
const SW_VIEWS = [["time", "Over time"], ["now", "Now"]];
let swView = params.get("versions") === "now" ? "now" : "time";

const sizePalette = new Palette();
let versionsChart, sizeChart, summary;
const $ = (id) => document.getElementById(id);

function remember() {
  const u = new URL(location.href);
  u.searchParams.set("scope", scope);
  u.searchParams.set("days", days);
  if (swView === "now") u.searchParams.set("versions", "now");
  else u.searchParams.delete("versions");
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
  root.hidden = !rows.length;
  if (!rows.length) return;
  const sum = total(rows);
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
  const stat = (value, label, note, to, tone, onclick) => el("div", { class: tone ? `stat-${tone}` : null },
    el("dt", { text: label }),
    el("dd", { class: "num" }, el("a", { href: to, class: "stat-link", onclick }, value), note ? el("small", { text: note }) : null));
  // These three count every node, not just masternodes: their sections switch to "All nodes" to match.
  const showAll = (e) => {
    if (e.button || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return; // a new tab keeps this one as it is
    if (scope !== "all") { scope = "all"; remember(); renderControls(); renderSections(); loadHistorySafely(); }
  };
  const reach = Object.fromEntries(mn.reachable || []);
  const answered = reach.yes || 0, silent = reach.no || 0;
  const share = answered + silent ? answered / (answered + silent) : null;
  $("hero-stats").replaceChildren(
    // The counts are of enabled masternodes; so are the lists they link to.
    stat(pct(share), "Masternodes reachable", silent ? `${fmt(silent)} didn't answer` : share != null ? "all answered" : null,
      href(silent ? "/masternodes?reach=Unreachable&status=Enabled" : "/masternodes?status=Enabled"), share == null ? null : share >= 0.98 ? "good" : share >= 0.9 ? "warning" : "critical"),
    stat(fmt(c.mn_total), "Enabled masternodes", type.Evo ? `${fmt(type.Evo)} of them evonodes` : null, href("/masternodes?status=Enabled")),
    stat(fmt(status.pose_banned || 0), "PoSe-banned", null, href("/masternodes?status=PoSe-banned")),
    stat(fmt((all.country || []).filter(([k]) => k !== "Unknown").length), "Countries", "all nodes", "#where", null, showAll),
    stat(fmt(byOrg(all.asn || []).filter(([k]) => k !== "Unknown").length), "Hosting providers", "all nodes", "#where", null, showAll),
    stat(fmt(c.tried), "Addresses tried", null, "#reach", null, showAll),
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

// ---- versions: a release ladder ----

const vparts = (v) => v.split(".").map((n) => parseInt(n, 10) || 0);
/** Newest first: 24.0.0, 23.1.9, 23.1.8. */
function byNewest(a, b) {
  const x = vparts(a), y = vparts(b);
  for (let i = 0; i < Math.max(x.length, y.length); i++) if ((x[i] || 0) !== (y[i] || 0)) return (y[i] || 0) - (x[i] || 0);
  return 0;
}
/** Whether a version is Dash Core's ("23.1.9"), not another client's ("Lksc Core 5.18"). */
const isCoreVersion = (v) => /^\d+\./.test(v);

/** Dash Core's releases (/api/v1/releases) by version: {candidate, published}; null if unavailable. */
let releases = null;
/** Dash Core's final releases, newest first. */
let finals = [];

function loadReleases(doc) {
  releases = new Map();
  for (const r of doc.releases) {
    const had = releases.get(r.version);
    // A version with a final release is a release, whatever candidates came before it.
    if (!had || had.candidate) releases.set(r.version, { candidate: r.prerelease, published: r.published });
  }
  finals = [...releases].filter(([, r]) => !r.candidate).map(([v]) => v).sort(byNewest);
}

/** "release", "candidate" (only release candidates so far) or "build" (matches no release: dev or custom). */
const releaseStatus = (v) => (!releases ? "release" : !releases.has(v) ? "build" : releases.get(v).candidate ? "candidate" : "release");

/** Brand-blue tints by age: the latest release in full, then a step paler for each older one. */
const TINTS = [100, 72, 52, 40];

/**
 * Versions colored by release: Dash Core's latest release in the brand blue and each older release a
 * step paler (by its place among all releases, so a release nobody runs still counts), release
 * candidates teal, builds that match no release ochre. Without the release list the most common
 * versions get the tints instead. Set from every version in view, so the ladder and the chart agree
 * (the chart's `assign` leaves it be).
 */
class ReleasePalette {
  constructor() { this.colors = new Map(); }
  /** rows: [[version, count]], most common first. */
  set(rows) {
    const rank = (v, i) => (releases ? finals.indexOf(v) : i);
    this.colors = new Map(rows.map(([v], i) => {
      const s = releaseStatus(v);
      if (s !== "release") return [v, `var(--rel-${s})`];
      return [v, `color-mix(in srgb, var(--brand) ${TINTS[Math.min(rank(v, i), TINTS.length - 1)]}%, var(--page))`];
    }));
  }
  assign() {}
  color(k) { return k === "Unknown" ? "var(--axis)" : this.colors.get(k) || "var(--other)"; }
}
const versionPalette = new ReleasePalette();

/** What a version is, for the ladder: "latest release", "release candidate", "not a release", or when it came out. */
function releaseNote(v) {
  if (!releases) return "";
  const s = releaseStatus(v);
  if (s === "candidate") return "release candidate";
  if (s === "build") return "not a release";
  if (finals[0] === v) return "latest release";
  return new Date(releases.get(v).published).toLocaleDateString("en-GB", { month: "short", year: "numeric", timeZone: "UTC" });
}


/**
 * What a version is, for ordering: 0 releases, 1 release candidates, 2 other builds, 3 other clients
 * (including a user agent that reads "Other"). The chart's folded "Other" band and "Unknown" (no answer)
 * sort after those.
 */
const KINDS = { release: 0, candidate: 1, build: 2 };
const kindOf = (v) => (isCoreVersion(v) ? KINDS[releaseStatus(v)] : 3);
const stackKind = (k) => (k === "Unknown" ? 5 : k === "Other" ? 4 : kindOf(k));
/** The ladder's order: the latest release first, older releases after it, then candidates and builds. */
const ladderOrder = (a, b) => stackKind(a) - stackKind(b) || byNewest(a, b);

/**
 * The ladder's groups, the latest release always first (even if nothing runs it yet): releases newest
 * first, those under 1% after the latest folded into one row; then release candidates, builds that match
 * no release, and other clients. Without the release list, one group by share.
 */
function ladderGroups(versions, sum) {
  const one = (k, n) => ({ key: k, label: k, n });
  const fold = (rows, label) => (rows.length === 1 ? one(...rows[0]) : { key: rows[0][0], label, n: total(rows), merged: true });
  const split = (rows, keep) => {
    const shown = rows.filter(keep), rest = rows.filter((r) => !shown.includes(r));
    return [shown, rest];
  };
  if (!releases) {
    const [shown, rest] = split(versions, ([, n], i) => i < 8 && n / sum >= 0.005);
    return [{ rows: [...shown.map((r) => one(...r)), ...(rest.length ? [fold(rest, `${rest.length} more versions`)] : [])] }];
  }
  const kinds = [[], [], [], []];
  for (const row of versions) kinds[kindOf(row[0])].push(row);
  for (const k of kinds) k.sort(([a], [b]) => byNewest(a, b));
  if (finals[0] && !kinds[0].some(([v]) => v === finals[0])) kinds[0].unshift([finals[0], 0]);
  const [rel, older] = split(kinds[0], ([v, n]) => v === finals[0] || n / sum >= 0.01);
  return [
    { label: "Releases", rows: [...rel.map((r) => one(...r)), ...(older.length ? [fold(older, `${older.length} other releases`)] : [])] },
    { label: "Release candidates", rows: kinds[1].map((r) => one(...r)) },
    { label: "Builds that aren't releases", rows: kinds[2].map((r) => one(...r)) },
    { label: "Other clients", rows: kinds[3].length ? [fold(kinds[3], `${kinds[3].length} other clients`)] : [] },
  ].filter((g) => g.rows.length);
}

/** The ladder's rows, flat and in order, for the pie. */
let ladderRows = [];

function renderLadder() {
  const d = summary.dists[scope] || {};
  const versions = (d.version || []).filter(([k]) => k !== "Unknown");
  const unknown = (d.version || []).find(([k]) => k === "Unknown")?.[1] || 0;
  const sum = total(d.version || []);
  const note = (r) => (r.merged || !isCoreVersion(r.key) ? "" : releaseNote(r.key));
  const row = (r) => el("tr", {},
    el("th", { class: "ver", scope: "row" }, el("span", { text: r.label }), note(r) ? [" ", el("small", { text: note(r) })] : null),
    el("td", { class: "bar", "aria-hidden": "true" }, r.n ? el("i", { style: { width: `${(r.n / sum) * 100}%`, background: r.merged ? "var(--other)" : versionPalette.color(r.key) } }) : null),
    el("td", { class: "r", text: pct(sum ? r.n / sum : 0) }), el("td", { class: "r n", text: fmt(r.n) }));
  const groups = ladderGroups(versions, sum);
  const none = unknown ? [{ key: "Unknown", label: "No answer this week", n: unknown }] : [];
  ladderRows = [...groups.flatMap((g) => g.rows), ...none];
  const head = (label) => el("tr", { class: "group" }, el("th", { colspan: 4, scope: "rowgroup", text: label }));
  $("sw-ladder").replaceChildren(
    el("thead", {}, el("tr", {}, el("th", { text: "Version" }), el("td", { "aria-hidden": "true" }),
      el("th", { class: "r", text: "Share" }), el("th", { class: "r n", text: "Count" }))),
    ...groups.map((g) => el("tbody", {}, g.label ? head(g.label) : null, ...g.rows.map(row))),
    ...(none.length ? [el("tbody", { class: "unknown" }, ...none.map(row))] : []));
  if (swView === "now") renderVersionPie();
}

/** "Share now": the ladder's rows as a pie, the latest release's share in the middle (0% before anything runs it). */
function renderVersionPie() {
  const rows = ladderRows.filter((r) => r.n > 0);
  const sum = rows.reduce((a, r) => a + r.n, 0);
  const color = (r) => (r.key === "Unknown" ? "var(--axis)" : r.merged ? "var(--other)" : versionPalette.color(r.key));
  const latest = finals[0] && ladderRows.find((r) => r.key === finals[0] && !r.merged);
  donut($("versions-pie"), rows.map((r) => [r.key, r.n]), {
    limit: rows.length,
    color: (_, i) => color(rows[i]),
    label: (k) => rows.find((r) => r.key === k).label,
    center: latest ? [pct(sum ? latest.n / sum : 0), "latest release"] : null,
  });
  $("versions-pie-legend").replaceChildren(...rows.map((r) => el("span", {}, el("i", { style: { background: color(r) } }), r.label, el("b", { text: ` ${pct(r.n / sum)}` }))));
}

function renderSoftware() {
  const d = summary.dists[scope] || {};
  const versions = (d.version || []).filter(([k]) => k !== "Unknown");
  const unknown = (d.version || []).find(([k]) => k === "Unknown")?.[1] || 0;
  const sum = total(d.version || []);
  versionPalette.set(versions);
  renderLadder();
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
    ? el("a", { class: "quiet", href: href(`/masternodes?${param}=${encodeURIComponent(value)}&status=Enabled${scope === "evo" ? "&type=Evo" : ""}`), text })
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
    lede.append(" ", el("a", { href: href(`/masternodes?reach=Unreachable&status=Enabled${scope === "evo" ? "&type=Evo" : ""}`), text: "See which" }));
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

/**
 * Narrows the version history to `n` named series, the latest release always among them (the API keeps
 * the most common over the whole range, which on a long range can leave out a new release); the rest
 * fold into "Other".
 */
function keepVersions(doc, n) {
  const named = doc.series.filter((x) => x.key !== "Other" && x.key !== "Unknown");
  const keep = named.slice(0, n);
  const latest = named.find((x) => x.key === finals[0]);
  if (latest && !keep.includes(latest)) keep.splice(n - 1, 1, latest);
  const rest = named.filter((x) => !keep.includes(x));
  if (!rest.length) return;
  let other = doc.series.find((x) => x.key === "Other");
  if (!other) { other = { key: "Other", values: doc.t.map(() => 0) }; }
  other = { key: "Other", values: other.values.map((v, i) => rest.reduce((a, x) => a + x.values[i], v)) };
  doc.series = [...keep, other, ...doc.series.filter((x) => x.key === "Unknown")];
}

async function loadHistory() {
  const mine = ++historyRequest;
  versionsChart?.loading();
  sizeChart?.loading();
  const [v, s] = await Promise.all([
    api(`${net}/history?scope=${scope}&dim=version&days=${days}&top=20`),
    api(`${net}/history?dim=totals&days=${days}`),
  ]);
  if (mine !== historyRequest) return; // a newer scope or range was picked meanwhile
  keepVersions(v, 6);
  // Stacked in the ladder's order read upwards: the latest release on top, no answer at the bottom.
  // Without the release list the ladder goes by share, and so does the stack.
  const share = (k) => v.series.find((x) => x.key === k).values.reduce((a, b) => a + b, 0);
  v.series.sort((a, b) => (releases ? ladderOrder(b.key, a.key) : stackKind(b.key) - stackKind(a.key) || share(a.key) - share(b.key)));
  const t0 = v.t.length >= 2 ? v.t[0] : null;
  $("sw-since").textContent = t0 && t0 > Date.now() / 1000 - Number(days) * 86400 + 86400
    ? `Our history starts ${new Date(t0 * 1000).toLocaleDateString("en-GB", { day: "numeric", month: "short", year: "numeric", timeZone: "UTC" })}, so this range shows less than it asks for.`
    : "";
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

/** A stat tile without a link: label, value, an optional note and an optional tone (stat-good, stat-warning). */
const plainStat = (label, value, note, tone) =>
  el("div", { class: tone ? `stat-${tone}` : null }, el("dt", { text: label }), el("dd", { class: "num" }, value, note ? el("small", { text: note }) : null));

/** Realized returns (summary.returns): the median yield over the last 90 days, by type. */
function renderEarn() {
  const r = summary.returns;
  const reg = r?.regular?.median_annual_yield, evo = r?.evo?.median_annual_yield;
  $("earn-section").hidden = reg == null && evo == null;
  if ($("earn-section").hidden) return;
  // Evonodes also earn Platform rewards: shown on their own and with Core payments, when collected.
  const e = r.evo;
  const withPlatform = e?.median_total_yield != null;
  $("earn-lede").replaceChildren(...[`What masternodes were actually paid over the last ${r.window_days} days, against their collateral, ` +
    "counting every enabled masternode registered for at least that long. " +
    (withPlatform ? "Evonodes are also paid Platform rewards for the Platform blocks they propose. " : "") + "Each masternode's page shows its own.",
  ...(withPlatform ? [" ", el("a", { href: href("/platform"), text: "Platform rewards in detail" }), "."] : [])]);
  $("earn-stats").replaceChildren(...[
    plainStat("Regular masternodes", reg != null ? `${pct(reg)} a year` : "–", `median of ${fmt(r.regular.masternodes)}, on 1,000 DASH`),
    withPlatform
      ? plainStat("Evonodes", `${pct(e.median_total_yield)} a year`, `median of ${fmt(e.masternodes)}, on 4,000 DASH, Core payments and Platform rewards together`)
      : plainStat("Evonodes", evo != null ? `${pct(evo)} a year` : "–", `median of ${fmt(r.evo.masternodes)}, on 4,000 DASH; core-chain payments only, Platform rewards aren't counted`),
    withPlatform ? plainStat("Evonodes, Core payments", evo != null ? `${pct(evo)} a year` : "–", "median, from the Core blocks that paid them") : null,
    withPlatform ? plainStat("Evonodes, Platform rewards", `${pct(e.median_platform_yield)} a year`,
      `median, from the Platform epochs of the last ${Math.round(e.platform_span_days)} days`) : null].filter(Boolean));
}

/** Evonode Platform health (summary.platform): the latest check of every evonode's Platform endpoints. */
function renderPlatform() {
  const p = summary.platform;
  $("platform-section").hidden = !p?.checked;
  if (!p?.checked) return;
  $("platform-lede").textContent = `${pct(p.healthy / p.checked)} of ${fmt(p.checked)} enabled evonodes passed our latest Platform check: ` +
    `their Tenderdash P2P port was open and their DAPI answered over valid TLS, as themselves. Checked every 15 minutes, last ${ago(p.updated)}.`;
  $("platform-stats").replaceChildren(
    plainStat("Healthy", `${fmt(p.healthy)} of ${fmt(p.checked)}`, null, p.healthy / p.checked >= 0.95 ? "good" : "warning"),
    plainStat("DAPI answered", fmt(p.https_ok)),
    plainStat("Tenderdash P2P open", fmt(p.p2p_ok)),
    plainStat("Certificates expiring", fmt(p.cert_expiring_3d), "within 3 days", p.cert_expiring_3d ? "warning" : null),
    plainStat("Platform height", p.max_height != null ? fmt(p.max_height) : "–", "highest seen"));
  for (const [bar, legend, rows] of [["drive-bar", "drive-legend", p.drive], ["td-bar", "td-legend", p.tenderdash]]) {
    const palette = new Palette();
    palette.assign(rows.map(([k]) => k));
    shareBar($(bar), $(legend), rows, { color: (k) => (k === "Unknown" ? "var(--axis)" : palette.color(k)), label: (k) => (k === "Unknown" ? "No answer" : k) });
  }
}

/**
 * DKG readiness (summary.dkg_readiness): how many enabled masternodes pass each of the next Core
 * release's DKG connection checks, with links to the ones that fail.
 */
function renderDkg() {
  const d = summary.dkg_readiness;
  const hidden = !d?.masternodes;
  $("dkg-section").hidden = hidden;
  if (hidden) return;
  const { rules } = d;
  const list = (filter) => href(`/masternodes?dkg=${filter}&status=Enabled`);
  const known = (c) => c.pass + c.fail + c.unknown;
  $("dkg-title").textContent = `Ready for ${rules.release} quorums`;
  const ready = (n) => pct(n / d.masternodes);
  // The protocol check counts only once the release is out (PROTOCOL_CHECK in readiness.rs); summaries
  // from before the flag counted it.
  const proto = rules.protocol_check !== false && d.checks.protocol;
  $("dkg-lede").textContent = `Masternodes running ${rules.release} vote nodes that fail these checks bad when they set up a quorum (DKG), ` +
    "which costs PoSe score. Reachability and Platform bans count already. " +
    (proto ? `${ready(proto.pass)} of ${fmt(d.masternodes)} enabled masternodes already run protocol ${rules.min_protocol} or newer, ` +
      `and ${ready(d.overall.pass)} pass every check. ` +
      (d.version_only ? `${fmt(d.version_only)} fail only the version check and need just the upgrade. ` : "") +
      (d.beyond_version ? `${fmt(d.beyond_version)} fail another check that needs a fix.` : "")
      : `${ready(d.overall.pass)} of ${fmt(d.masternodes)} enabled masternodes pass every check. ` +
        `The version check starts once ${rules.release} is released.`);
  const notes = { protocol: `needs ${rules.min_protocol}+`, platform_ban: "evonodes only" };
  const stat = (label, c, filter) => el("div", { class: c.fail ? "stat-warning" : c.pass ? "stat-good" : null },
    el("dt", { text: label }),
    // Nothing known yet (e.g. dashd hasn't reported Platform bans): no share to give.
    el("dd", { class: "num" }, c.pass + c.fail ? pct(c.pass / known(c)) : "–", el("small", {},
      c.fail ? el("a", { href: list(filter), text: `${fmt(c.fail)} fail` }) : "none fail",
      c.unknown ? `, ${fmt(c.unknown)} unknown` : "", notes[filter] ? ` · ${notes[filter]}` : "")));
  $("dkg-stats").replaceChildren(
    el("div", { class: d.overall.pass === d.masternodes ? "stat-good" : null },
      el("dt", { text: "Pass every check" }),
      el("dd", { class: "num" }, pct(d.overall.pass / d.masternodes),
        el("small", {}, d.overall.pass ? el("a", { href: list("ready"), text: `${fmt(d.overall.pass)} ready` }) : "0 ready",
          ` of ${fmt(d.masternodes)}`, d.overall.unknown ? `, ${fmt(d.overall.unknown)} unknown` : ""))),
    ...DKG_CHECKS.filter(([k]) => d.checks[k] && known(d.checks[k])).map(([k, label]) => stat(label, d.checks[k], k)));
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
  $("sw-view").replaceChildren(segmented(SW_VIEWS, swView, (v) => { swView = v; remember(); showVersionsView(); }, "Versions chart"));
  showVersionsView();
}

/** Shows the chosen versions chart: the time series with its range picker, or the pie. */
function showVersionsView() {
  const now = swView === "now";
  $("sw-chart-title").textContent = now ? "Share now" : "Share over time";
  for (const id of ["sw-range", "versions-chart", "sw-since"]) $(id).hidden = now;
  $("versions-pie-box").hidden = !now;
  if (now) renderVersionPie();
}

async function main() {
  // Optional: without it the ladder ranks versions by number alone.
  const releaseList = api("releases").catch(() => null);
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
  const rel = await releaseList;
  if (Array.isArray(rel?.releases)) loadReleases(rel);
  renderHero();
  heroPoints().then((points) => worldMap($("hero-map"), points, $("hero-pings"))).catch(() => {});
  renderControls();
  renderSections();
  renderDkg();
  renderEarn();
  renderPlatform();
  renderSpeed();
  await loadHistorySafely();
}

main();
