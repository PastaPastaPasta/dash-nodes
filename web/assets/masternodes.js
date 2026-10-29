import { api, chrome, currentNet, el, fmt, pct, href, countryName, flag, shortHash, badge, ago, nodeStatus, uptimeKind,
  MN_FILTERS, mnFilters, mnMatches, rememberParams, filterQuery, filterControls } from "./common.js";

chrome("masternodes");
const net = currentNet();
const PAGE = 100;

const params = new URLSearchParams(location.search);
const state = {
  ...mnFilters(params),
  sort: params.get("sort") || "registered_height",
  dir: params.get("dir") === "desc" ? -1 : 1,
  page: 0,
};
let rows = [];
let tip = null;

function uptimeBadge(u) {
  if (u == null) return el("span", { class: "muted", text: "–" });
  return badge(uptimeKind(u), pct(u, u >= 0.999 ? 0 : 1));
}

const COLUMNS = [
  { key: "protx", label: "ProTx", get: (m) => m.protx,
    cell: (m) => el("td", {}, el("a", { class: "mono", href: href(`/mn/${m.protx}`), text: shortHash(m.protx) }), " ",
      m.type === "Evo" ? el("span", { class: "pill evo", text: "Evo" }) : null) },
  { key: "service", label: "Address", get: (m) => m.service || "", cell: (m) => el("td", { class: "mono", text: m.service || "–" }) },
  { key: "country", label: "Country", get: (m) => countryName(m.country),
    cell: (m) => el("td", { title: countryName(m.country), text: m.country ? `${flag(m.country)} ${m.country}` : "–" }) },
  { key: "as_org", label: "Hosting", get: (m) => m.as_org || "",
    cell: (m) => el("td", { title: m.asn ? `AS${m.asn} ${m.as_org || ""}` : "", style: { maxWidth: "180px", overflow: "hidden", textOverflow: "ellipsis" }, text: m.as_org || "–" }) },
  { key: "version", label: "Version", get: (m) => m.version || "",
    cell: (m) => el("td", { title: m.user_agent || "", text: m.version || "–" }) },
  { key: "protocol", label: "Protocol", r: true, get: (m) => m.protocol ?? -1, cell: (m) => el("td", { class: "r", text: m.protocol ?? "–" }) },
  { key: "state", label: "Status", get: (m) => nodeStatus(m)[1],
    cell: (m) => { const [k, l] = nodeStatus(m); return el("td", { title: m.last_error || (m.last_seen ? `last answered ${ago(m.last_seen)}` : "") }, badge(k, l)); } },
  { key: "uptime_7d", label: "Uptime 7d", r: true, get: (m) => m.uptime_7d ?? -1, cell: (m) => el("td", { class: "r" }, uptimeBadge(m.uptime_7d)) },
  { key: "uptime_30d", label: "30d", r: true, get: (m) => m.uptime_30d ?? -1, cell: (m) => el("td", { class: "r" }, uptimeBadge(m.uptime_30d)) },
  { key: "last_paid_height", label: "Last paid", r: true, get: (m) => m.last_paid_height || 0,
    cell: (m) => el("td", { class: "r", title: m.last_paid_height ? `block ${fmt(m.last_paid_height)}` : "" },
      m.last_paid_height ? (tip ? `${fmt(tip - m.last_paid_height)} blocks ago` : fmt(m.last_paid_height)) : "never") },
  { key: "registered_height", label: "Registered", r: true, get: (m) => m.registered_height || 0, cell: (m) => el("td", { class: "r", text: fmt(m.registered_height) }) },
  { key: "pose_penalty", label: "PoSe", r: true, get: (m) => m.pose_penalty || 0, cell: (m) => el("td", { class: "r", text: m.pose_penalty ?? "–" }) },
];

function filtered() {
  const out = rows.filter((m) => mnMatches(m, state));
  const col = COLUMNS.find((c) => c.key === state.sort) || COLUMNS[0];
  out.sort((a, b) => {
    const x = col.get(a), y = col.get(b);
    return (x < y ? -1 : x > y ? 1 : 0) * state.dir || (a.protx < b.protx ? -1 : 1);
  });
  return out;
}

function remember() {
  rememberParams({
    ...Object.fromEntries(MN_FILTERS.map((k) => [k, state[k]])),
    sort: state.sort === "registered_height" ? "" : state.sort,
    dir: state.dir < 0 ? "desc" : "",
  });
  document.getElementById("to-map").href = href(`/map${filterQuery(state)}`);
}

function render() {
  const list = filtered();
  const pages = Math.max(1, Math.ceil(list.length / PAGE));
  state.page = Math.min(state.page, pages - 1);
  const head = el("tr", {}, ...COLUMNS.map((c) => el("th", {
    class: `sortable${c.r ? " r" : ""}`, scope: "col", tabindex: 0,
    "aria-sort": state.sort === c.key ? (state.dir > 0 ? "ascending" : "descending") : null,
    onclick: () => sortBy(c.key), onkeydown: (e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), sortBy(c.key)),
    text: c.label,
  })));
  const body = list.slice(state.page * PAGE, (state.page + 1) * PAGE).map((m) => el("tr", {}, ...COLUMNS.map((c) => c.cell(m))));
  const table = document.getElementById("table");
  table.replaceChildren(el("thead", {}, head), el("tbody", {}, ...(body.length ? body : [el("tr", {}, el("td", { colspan: COLUMNS.length, class: "empty", text: rows.length ? "No masternode matches these filters." : "No masternode list yet." }))])));
  document.getElementById("pager").replaceChildren(
    el("span", { text: `${fmt(list.length)} of ${fmt(rows.length)} masternodes` }),
    el("button", { class: "btn", type: "button", disabled: state.page === 0, onclick: () => { state.page--; render(); }, text: "Previous" }),
    el("span", { text: `Page ${state.page + 1} of ${pages}` }),
    el("button", { class: "btn", type: "button", disabled: state.page >= pages - 1, onclick: () => { state.page++; render(); }, text: "Next" }));
  remember();
}

function sortBy(key) {
  if (state.sort === key) state.dir = -state.dir;
  else { state.sort = key; state.dir = key.startsWith("uptime") || key === "pose_penalty" ? -1 : 1; }
  render();
}

function renderFilters() {
  document.getElementById("filters").replaceChildren(...filterControls(rows, state, () => { state.page = 0; render(); }));
}

function csv() {
  const cols = ["protx", "type", "status", "service", "reachable", "version", "user_agent", "protocol", "country", "asn", "as_org",
    "uptime_7d", "uptime_30d", "last_seen", "registered_height", "last_paid_height", "pose_penalty", "pose_ban_height", "payout_address", "platform_node_id"];
  // Operators choose their user agent: neutralize spreadsheet formulas, then quote.
  const esc = (v) => {
    if (v == null) return "";
    let s = String(v);
    if (/^[=+\-@\t\r]/.test(s)) s = `'${s}`;
    return /[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  const text = [cols.join(","), ...filtered().map((m) => cols.map((c) => esc(m[c])).join(","))].join("\n");
  const a = el("a", { href: URL.createObjectURL(new Blob([text], { type: "text/csv" })), download: `dash-${net}-masternodes.csv` });
  a.click();
  URL.revokeObjectURL(a.href);
}

async function main() {
  document.getElementById("csv").addEventListener("click", csv);
  try {
    const data = await api(`${net}/masternodes`);
    rows = data.masternodes;
    tip = data.tip;
    const reach = rows.filter((m) => m.status === "enabled" && m.reachable === true).length;
    const enabled = rows.filter((m) => m.status === "enabled").length;
    document.getElementById("lede").textContent =
      `${fmt(rows.length)} registered, ${fmt(enabled)} enabled; ${pct(enabled ? reach / enabled : null)} of enabled masternodes answered our latest port check.`;
  } catch (e) {
    document.getElementById("lede").textContent = `Couldn't load the masternode list: ${e.message}`;
  }
  renderFilters();
  render();
}

main();
