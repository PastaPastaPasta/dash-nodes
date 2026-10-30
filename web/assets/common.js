// Shared page chrome, network selection, fetching and formatting.

export const NETWORKS = ["mainnet", "testnet"];

export function currentNet() {
  const q = new URLSearchParams(location.search).get("net");
  if (NETWORKS.includes(q)) return q;
  const saved = localStorage.getItem("net");
  return NETWORKS.includes(saved) ? saved : "mainnet";
}

export function setNet(net) {
  localStorage.setItem("net", net);
  const u = new URL(location.href);
  if (net === "mainnet") u.searchParams.delete("net");
  else u.searchParams.set("net", net);
  location.href = u.toString();
}

/** Link to a page on this site, carrying the network along. */
export function href(path) {
  const net = currentNet();
  return net === "mainnet" ? path : `${path}${path.includes("?") ? "&" : "?"}net=${net}`;
}

export async function api(path) {
  const r = await fetch(`/api/v1/${path}`);
  if (!r.ok) {
    let msg = `HTTP ${r.status}`;
    try { msg = (await r.json()).error || msg; } catch { /* not json */ }
    throw new Error(msg);
  }
  return r.json();
}

/** Tiny element builder. Text is always set via textContent (data is untrusted). */
export function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k === "text") e.textContent = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (k === "style" && typeof v === "object") Object.assign(e.style, v);
    else e.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    e.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return e;
}

const nf = new Intl.NumberFormat("en-US");
export const fmt = (n) => (n == null ? "–" : nf.format(Math.round(n)));
export const pct = (x, digits = 1) => (x == null ? "–" : `${(x * 100).toFixed(digits)}%`);
export const dash = (duffs) => (duffs == null ? "–" : `${(duffs / 1e8).toLocaleString("en-US", { maximumFractionDigits: 4 })} DASH`);

export function compact(n) {
  if (Math.abs(n) >= 1e6) return `${+(n / 1e6).toFixed(1)}M`;
  if (Math.abs(n) >= 1e3) return `${+(n / 1e3).toFixed(1)}K`;
  return String(Math.round(n * 10) / 10);
}

export function ago(ts) {
  if (!ts) return "never";
  const s = Math.max(0, Date.now() / 1000 - ts);
  if (s < 90) return "just now";
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 129600) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
}

export function dateTime(ts) {
  return new Date(ts * 1000).toLocaleString("en-GB", { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" }) + " UTC";
}

const regions = (() => { try { return new Intl.DisplayNames(["en"], { type: "region" }); } catch { return null; } })();
export function countryName(code) {
  if (!code || code === "Unknown") return "Unknown";
  try { return regions?.of(code) || code; } catch { return code; }
}
export function flag(code) {
  if (!code || code.length !== 2) return "";
  return String.fromCodePoint(...[...code.toUpperCase()].map((c) => 0x1f1a5 + c.charCodeAt(0)));
}

export function shortHash(h, n = 8) {
  return h && h.length > 2 * n ? `${h.slice(0, n)}…${h.slice(-4)}` : h || "";
}

/** "Frankfurt, Germany" behind its flag; empty when the country is unknown. */
export function place(m) {
  return m.country ? `${flag(m.country)} ${[m.city, countryName(m.country)].filter(Boolean).join(", ")}` : "";
}

export const evoPill = (m) => (m.type === "Evo" ? el("span", { class: "pill evo", text: "Evo" }) : null);

/** Status badge: color is never the only signal, the label always rides along. */
export function badge(kind, label) {
  return el("span", { class: `badge ${kind}` }, el("i"), label);
}

export const STATUSES = ["Reachable", "Unreachable", "PoSe-banned", "Not checked"];

/** One status per masternode, one of STATUSES; PoSe ban outranks the port check. Returns [badge kind, label]. */
export function nodeStatus(m, unchecked = "Not checked") {
  if (m.status === "pose_banned") return ["serious", "PoSe-banned"];
  if (m.reachable === true) return ["good", "Reachable"];
  if (m.reachable === false) return ["critical", "Unreachable"];
  return ["none", unchecked];
}

/** Masternode filters shared by the list and the map, so either page can link to the other. */
const MN_FILTERS = ["q", "type", "status", "version", "country"];

export function mnFilters(params) {
  return Object.fromEntries(MN_FILTERS.map((k) => [k, params.get(k) || ""]));
}

export function mnMatches(m, f) {
  if (f.type && m.type !== f.type) return false;
  if (f.status && nodeStatus(m)[1] !== f.status) return false;
  if (f.version && (m.version || "Unknown") !== f.version) return false;
  if (f.country && (m.country || "Unknown") !== f.country) return false;
  const q = f.q.trim().toLowerCase();
  if (q) {
    const hay = [m.protx, m.service, m.payout_address, m.as_org, m.user_agent, m.platform_node_id, m.city].join(" ").toLowerCase();
    if (!hay.includes(q)) return false;
  }
  return true;
}

/** Writes the filters in `f` plus any `extra` params into the address bar (empty ones removed) without a reload. */
export function rememberParams(f, extra = {}) {
  const u = new URL(location.href);
  for (const [k, v] of [...MN_FILTERS.map((k) => [k, f[k]]), ...Object.entries(extra)]) {
    if (v) u.searchParams.set(k, v);
    else u.searchParams.delete(k);
  }
  history.replaceState(null, "", u);
}

/** The current page's filters as a query string for a link to another page. */
export function filterQuery(f) {
  const s = new URLSearchParams(MN_FILTERS.filter((k) => f[k]).map((k) => [k, f[k]])).toString();
  return s ? `?${s}` : "";
}

/** One option list per filter, with counts, ordered most common first. */
export function countBy(rows, key) {
  const m = new Map();
  for (const r of rows) { const k = key(r); m.set(k, (m.get(k) || 0) + 1); }
  return [...m.entries()].sort((a, b) => b[1] - a[1]);
}

/** A select bound to `f[name]`: choosing stores the value and calls onChange. `any` names the empty option. */
export function filterSelect(f, name, label, options, onChange, any = "All") {
  const s = el("select", { "aria-label": label, onchange: () => { f[name] = s.value; onChange(); } },
    el("option", { value: "", text: any }), ...options.map(([v, t]) => el("option", { value: v, text: t })));
  s.value = f[name];
  return s;
}

/** Search box bound to `f.q`, applied 150 ms after typing stops. */
export function searchBox(f, onChange, placeholder, label) {
  const search = el("input", { type: "search", placeholder, value: f.q, "aria-label": label });
  let timer;
  search.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(() => { f.q = search.value; onChange(); }, 150); });
  return search;
}

export const TYPE_OPTIONS = [["Regular", "Regular"], ["Evo", "Evo"]];
export const versionOptions = (rows) => countBy(rows, (r) => r.version || "Unknown").map(([v, n]) => [v, `${v} (${fmt(n)})`]);
export const countryOptions = (rows) => countBy(rows, (r) => r.country || "Unknown").map(([c, n]) => [c, `${flag(c)} ${countryName(c)} (${fmt(n)})`]);

/**
 * Search box and selects for the shared masternode filters. They edit `f` in place and call
 * onChange; option lists and counts come from `rows`.
 */
export function filterControls(rows, f, onChange) {
  const select = (name, label, options) => el("label", {}, label, filterSelect(f, name, label, options, onChange));
  return [
    searchBox(f, onChange, "ProTx, IP, payout address, hosting…", "Search"),
    select("type", "Type", TYPE_OPTIONS),
    select("status", "Status", STATUSES.map((s) => [s, s])),
    select("version", "Version", versionOptions(rows)),
    select("country", "Country", countryOptions(rows)),
  ];
}

/** Badge kind for a share of port checks answered (0..1). */
export function uptimeKind(share) {
  if (share >= 0.99) return "good";
  if (share >= 0.9) return "warning";
  return "critical";
}

export function tile(label, value, foot) {
  return el("div", { class: "tile" }, el("div", { class: "label", text: label }), el("div", { class: "value" }, value), el("div", { class: "foot" }, foot));
}

export function card(title, sub, body) {
  return el("section", { class: "card" }, el("div", { class: "card-head" }, el("h2", { text: title }), el("span", { class: "sub", text: sub })), body);
}

const LOGO = `<svg viewBox="0 0 32 32" aria-hidden="true"><rect width="32" height="32" rx="8" fill="var(--brand)"/><circle cx="10" cy="11" r="2.6" fill="var(--brand-ink)"/><circle cx="22" cy="9" r="2.6" fill="var(--brand-ink)"/><circle cx="16" cy="22" r="2.6" fill="var(--brand-ink)"/><path d="M10 11 22 9 16 22Z" fill="none" stroke="var(--brand-ink)" stroke-width="1.6" stroke-linejoin="round" opacity=".75"/></svg>`;
const SUN = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>`;
const MORE = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M4 7h16M4 12h16M4 17h10"/></svg>`;
const MOON = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/></svg>`;

function applyTheme() {
  const t = localStorage.getItem("theme");
  if (t) document.documentElement.dataset.theme = t;
}
applyTheme();

function isDark() {
  const t = document.documentElement.dataset.theme;
  return t ? t === "dark" : matchMedia("(prefers-color-scheme: dark)").matches;
}

/** Renders header and footer. `page` marks the current nav item; full-screen pages skip the footer. */
export function chrome(page, { footer = true } = {}) {
  const net = currentNet();
  document.title = `${document.title}${net === "testnet" ? " · testnet" : ""}`;
  const nav = el("nav", { class: "main" },
    ...[["overview", "/", "Overview"], ["masternodes", "/masternodes", "Masternodes"], ["map", "/map", "Map"], ["api", "/api", "API"]].map(([id, path, label]) =>
      el("a", { href: href(path), "aria-current": id === page ? "page" : null, text: label })));
  const seg = el("div", { class: "seg net", role: "group", "aria-label": "Network" },
    ...NETWORKS.map((n) => el("button", {
      type: "button", "data-net": n, "aria-pressed": String(n === net),
      onclick: () => n !== net && setNet(n), "aria-label": n === "mainnet" ? "Mainnet" : "Testnet",
    }, n === "mainnet" ? "Main" : "Test", el("span", { class: "net-full", text: "net" }))));
  const status = el("span", { class: "chain-status", id: "chain-status" }, el("span", { class: "dot" }), "connecting…");
  const theme = el("button", { class: "theme-toggle", type: "button", "aria-label": "Toggle dark mode", title: "Toggle dark mode" });
  const paintTheme = () => { theme.innerHTML = isDark() ? SUN : MOON; };
  const themeChanged = () => { paintTheme(); window.dispatchEvent(new Event("themechange")); };
  theme.addEventListener("click", () => {
    const next = isDark() ? "light" : "dark";
    localStorage.setItem("theme", next);
    document.documentElement.dataset.theme = next;
    themeChanged();
  });
  paintTheme();
  const brand = el("a", { class: "brand", href: href("/") });
  brand.innerHTML = LOGO;
  brand.append(el("span", { text: "Dash Nodes" }), el("small", { text: "nodes.dashhq.org" }));
  if (net === "testnet") brand.append(el("b", { class: "net-badge", text: "Testnet" }));
  // Phones: the network switch, theme and status fold into a menu behind one button.
  const more = el("button", { class: "more-toggle", type: "button", "aria-label": "Network and display settings", "aria-expanded": "false", "aria-controls": "top-right" });
  more.innerHTML = MORE;
  const right = el("div", { class: "top-right", id: "top-right" }, status, seg, theme);
  const header = el("header", { class: "top" }, el("div", { class: "top-inner" }, brand, nav, more, right));
  const setMenu = (open) => { header.classList.toggle("menu-open", open); more.setAttribute("aria-expanded", String(open)); };
  more.addEventListener("click", (e) => { e.stopPropagation(); setMenu(!header.classList.contains("menu-open")); });
  document.addEventListener("click", (e) => { if (!right.contains(e.target)) setMenu(false); });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") setMenu(false); });
  document.body.prepend(header);

  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => { if (!localStorage.getItem("theme")) themeChanged(); });
  if (footer) document.body.append(el("footer", { class: "bottom" }, el("div", { class: "inner" },
    el("span", {}, "An independent Dash network monitor. Crawls the P2P network every 15 minutes from one vantage point; reachability is as seen from there."),
    el("span", {},
      el("a", { href: href("/api"), text: "API" }), " · ",
      "IP geolocation by ", el("a", { href: "https://db-ip.com", text: "DB-IP" }), " (CC BY 4.0)"))));
  refreshStatus();
  setInterval(refreshStatus, 60_000);
}

const STALE_AFTER = 45 * 60; // three missed crawls

async function refreshStatus() {
  const box = document.getElementById("chain-status");
  try {
    const s = await api("status");
    const n = s.networks[currentNet()];
    if (!n) { box.replaceChildren(el("span", { class: "dot warn" }), "network not monitored"); return; }
    const c = n.chain;
    let dot = "ok", text;
    if (!c.synced) {
      dot = "warn";
      text = `dashd syncing · ${(c.verification_progress * 100).toFixed(1)}%`;
    } else {
      text = `block ${fmt(c.blocks)}`;
      if (n.crawl.last_finished) text += ` · crawled ${ago(n.crawl.last_finished)}`;
      if (n.crawl.running) text += " · crawling now";
    }
    if (c.error) dot = "warn";
    // The site keeps serving the last published data if the crawler stops; say so once it's clearly old.
    const age = n.crawl.last_finished ? Date.now() / 1000 - n.crawl.last_finished : 0;
    const stale = age > STALE_AFTER;
    if (stale) { dot = "warn"; text = `last crawl ${ago(n.crawl.last_finished)}`; }
    document.getElementById("stale-bar")?.remove();
    if (stale) {
      const bar = el("div", { class: "stale-bar", id: "stale-bar", role: "status" },
        `This data is from ${ago(n.crawl.last_finished)}; the crawler hasn't published since.`);
      // Full-screen pages float it over the map; others get a strip under the header.
      if (document.body.classList.contains("app")) document.querySelector("main").prepend(bar);
      else document.querySelector("header.top").after(bar);
    }
    box.replaceChildren(el("span", { class: `dot ${dot}` }), text);
    box.title = c.dashd_version ? `Our dashd: ${c.dashd_version}` : "";
  } catch {
    box.replaceChildren(el("span", { class: "dot warn" }), "status unavailable");
  }
}

/** Segmented control over [value, text, disabled?] options. Returns the element; calls onChange(value). */
export function segmented(options, value, onChange, label) {
  const root = el("div", { class: "seg", role: "group", "aria-label": label });
  const paint = (v) => root.querySelectorAll("button").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.v === v)));
  for (const [v, text, disabled] of options) {
    root.append(el("button", { type: "button", "data-v": v, disabled, onclick: () => { paint(v); onChange(v); }, text }));
  }
  paint(value);
  return root;
}
