// /platform: Platform rewards across the network (the crawler's rewards.rs, /api/v1/{net}/platform).
import { api, chrome, currentNet, el, fmt, pct, href, ago, dateTime, amount, inAbout, shortHash, badge, segmented, searchBox, toDash, labelled, lastSpan } from "./common.js";
import { svg, sized, axisScale, valueAxis, epochAxis } from "./charts.js";
import { upgradeView } from "./platform-upgrade.js";
import { monthlyCell, monthlySortKey } from "./history.js";

chrome("platform");
const net = currentNet();
const $ = (id) => document.getElementById(id);
let doc;

const days = (s) => s / 86400;
const stat = (label, value, note, tone) =>
  el("div", { class: tone ? `stat-${tone}` : null }, el("dt", { text: label }), el("dd", { class: "num" }, value, note ? el("small", { text: note }) : null));
const noPort = (service) => service.replace(/:\d+$/, "");
const mnLink = (protx, text = shortHash(protx)) => el("a", { class: "mono", href: href(`/mn/${protx}`), text });

// ---- the current epoch ----

function renderNow(summary) {
  const n = doc.now;
  const last = doc.epochs[0];
  const elapsed = Date.now() / 1000 - n.start;
  const share = Math.min(1, Math.max(0, elapsed / n.epoch_secs));
  const length = n.epoch_secs >= 86400 ? `${days(n.epoch_secs).toFixed(1)} days` : `${Math.round(n.epoch_secs / 60)} min`;
  $("now-title").textContent = `Epoch ${fmt(n.epoch)} is ${pct(share, 0)} through`;
  $("now-rail").replaceChildren(el("div", { class: "rail", role: "img", "aria-label": `Epoch ${n.epoch}: ${pct(share, 0)} of its expected length` },
    el("span", { style: { width: `${share * 100}%` } })),
  el("div", { class: "rail-axis" }, el("span", { text: `started ${dateTime(n.start)}` }),
    el("span", { text: n.ends > Date.now() / 1000 ? `ends ${inAbout(n.ends)} (estimate)` : "ends with the next block" })));
  $("now-sub").textContent = `An epoch lasts about ${length}. When it ends, its pool is paid to the evonodes that proposed its blocks, ` +
    "straight into each one's masternode identity, from where the owner can withdraw it to Core.";
  const evo = summary?.returns?.evo;
  $("now-stats").replaceChildren(...[
    stat("Blocks so far", fmt(n.blocks), `by ${fmt(n.proposers)} evonode${n.proposers === 1 ? "" : "s"}, as of ${ago(n.updated)}`),
    stat("Pool so far", n.pool_so_far != null ? amount(n.pool_so_far) : "–", "Core block rewards paid in so far; fees come on top"),
    last ? stat("Last epoch's pool", amount(last.pool), `epoch ${fmt(last.epoch)}: ${fmt(last.blocks)} blocks by ${fmt(last.proposers)} evonodes`) : null,
    last ? stat("Per block", amount(last.per_block), "what proposing one block paid last epoch") : null,
    doc.returns.median_monthly != null ? stat("Median evonode, a month", amount(doc.returns.median_monthly),
      `Platform only, at the pace of ${lastSpan(doc.returns.monthly_basis_days) ?? "the last six epochs"}`) : null,
    evo?.median_total_yield != null
      ? stat("Median evonode yield", `${pct(evo.median_total_yield)} a year`, `on 4,000 DASH: ${pct(evo.median_platform_yield)} Platform, ${pct(evo.median_annual_yield)} Core`)
      : doc.returns.median_platform_yield != null ? stat("Median Platform yield", `${pct(doc.returns.median_platform_yield)} a year`, "on 4,000 DASH, Core payments not included") : null,
  ].filter(Boolean));
  $("now").hidden = false;
}

// ---- the protocol upgrade vote ----

function renderUpgrade() {
  const view = upgradeView(doc, Date.now() / 1000, { fmt, pct, inAbout });
  $("upgrade").dataset.state = view.state;
  $("upgrade-title").replaceChildren(view.title, ...(view.badge ? [badge(view.badge.tone, view.badge.text)] : []));
  $("upgrade-lede").textContent = view.lede;
  const m = view.meter;
  if (!m) $("upgrade-meter").replaceChildren();
  else {
    const share = (x) => `${Math.min(100, (x / Math.max(1, m.enabled)) * 100)}%`;
    $("upgrade-meter").replaceChildren(
      el("div", { class: "meter", role: "img", "aria-label": `${m.votes} of ${m.enabled} enabled evonodes voted for ${view.version}; ${m.required} needed` },
        el("span", { class: m.met ? "met" : null, style: { width: share(m.votes) } }),
        el("i", { style: { left: share(m.required) }, title: `${fmt(m.required)} needed` })),
      el("div", { class: "meter-axis" }, el("span", { text: `${fmt(m.votes)} voted for ${view.version}` }),
        el("span", { style: { left: share(m.required) }, text: `${fmt(m.required)} needed` }),
        el("span", { text: `${fmt(m.enabled)} enabled` })));
  }
  $("upgrade-stats").replaceChildren(...view.stats.map((x) => stat(x.label, x.value, x.note, x.tone)));
  $("upgrade-note").textContent = view.note;
  $("upgrade").hidden = false;
}

// ---- who proposes next ----

function renderNext() {
  const s = doc.schedule;
  if (!s?.next?.length) return;
  const byProtx = new Map(doc.evonodes.map((e) => [e.protx, e]));
  $("next-lede").textContent = `Evonodes take turns proposing blocks: one validator set at a time, in ProTx order, then the next set. ` +
    `At about ${Math.round(s.block_secs)} seconds a block lately, this is when the next ones are due. An estimate from the validator sets ${ago(s.ts)}: ` +
    "a missed turn, or a new set, shifts it.";
  $("next-list").replaceChildren(...s.next.slice(0, 12).map((x) => {
    const e = byProtx.get(x.protx);
    return el("li", {}, el("span", { class: "slot num", text: `+${x.slots}` }),
      el("span", { class: "who" }, mnLink(x.protx), e?.service ? el("small", { class: "muted mono", text: noPort(e.service) }) : null),
      el("span", { class: "when", text: inAbout(x.eta) }));
  }));
  $("next").hidden = false;
}

// ---- epochs ----

/** A band per epoch from what the 10th to the 90th percentile evonode earned, with the median marked. */
function spreadChart(box, epochs) {
  const list = [...epochs].reverse().filter((e) => e.credits_spread.p50 != null);
  sized(box, (W) => {
    const H = 220, L = 40, B = 22, T = 8;
    const { step, max } = axisScale(Math.max(0, ...list.map((e) => toDash(e.credits_spread.p90))) || 1, 4);
    const slot = (W - L) / Math.max(1, list.length);
    const row = (v) => T + (H - T - B) * (1 - v / max);
    const y = (c) => row(toDash(c));
    const center = (i) => L + slot * i + slot / 2;
    const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H, class: "spread", role: "img",
      "aria-label": `Per-epoch rewards of the middle 80% of evonodes, epochs ${list[0]?.epoch}–${list.at(-1)?.epoch}` });
    valueAxis(root, { L, W, step, max, row });
    list.forEach((e, i) => {
      const x = center(i), w = Math.max(2, Math.min(14, slot * 0.6));
      const s = e.credits_spread;
      const g = svg("g", { tabindex: 0 });
      const title = svg("title");
      title.textContent = `Epoch ${e.epoch} (${dateTime(e.start)}): the middle 80% of evonodes earned ${amount(s.p10)} to ${amount(s.p90)}, median ${amount(s.p50)}; pool ${amount(e.pool)}`;
      g.append(title, svg("rect", { x: x - w / 2, y: y(s.p90), width: w, height: Math.max(1, y(s.p10) - y(s.p90)), rx: 2, class: "band" }),
        svg("rect", { x: x - w / 2 - 1, y: y(s.p50) - 1, width: w + 2, height: 2.5, class: "median" }));
      root.append(g);
    });
    epochAxis(root, list, { W, x: center, y: H - 6 });
    return root;
  });
}

const EPOCH_HEADS = ["Epoch", "Started", "Length", "Blocks", "Evonodes paid", "Pool", "Per block", "Median evonode"];
function epochRow(e) {
  return labelled(EPOCH_HEADS, el("tr", {},
    el("td", { class: "num", text: fmt(e.epoch) }),
    el("td", { text: dateTime(e.start) }),
    el("td", { class: "r", text: e.duration >= 86400 ? `${days(e.duration).toFixed(2)} d` : `${Math.round(e.duration / 60)} min` }),
    el("td", { class: "r", text: fmt(e.blocks) }),
    el("td", { class: "r", text: fmt(e.proposers) }),
    el("td", { class: "r", title: `Core block rewards ${amount(e.core_rewards)}, fees ${amount(e.fees)}`, text: amount(e.pool) }),
    el("td", { class: "r", text: amount(e.per_block) }),
    el("td", { class: "r", text: e.credits_spread.p50 != null ? amount(e.credits_spread.p50) : "–" })));
}

function renderEpochs() {
  const list = doc.epochs;
  if (!list.length) return;
  const first = list.at(-1);
  $("epochs-lede").textContent = `Each finished epoch as Drive recorded it when it paid it out: the ${list.length > 1 ? `${fmt(list.length)} most recent, ` : ""}` +
    `from epoch ${fmt(first.epoch)} (${dateTime(first.start).replace(/,.*/, "")}) on. ` +
    "Core block rewards make up nearly all of each pool; fees are the rest.";
  $("epochs-key").textContent = "middle 80% of evonodes, line at the median, in DASH";
  $("epochs-chart").classList.add("chart");
  spreadChart($("epochs-chart"), list.slice(0, 60));
  let shown = 10;
  const render = () => {
    $("epochs-table").replaceChildren(el("thead", {}, el("tr", {}, ...EPOCH_HEADS.map((h, i) => el("th", { class: i > 1 ? "r" : null, text: h })))),
      el("tbody", {}, ...list.slice(0, shown).map(epochRow)));
    $("epochs-pager").replaceChildren(shown < list.length
      ? el("button", { class: "btn", type: "button", onclick: () => { shown += 30; render(); }, text: `Show more (${fmt(list.length - shown)} older)` })
      : "");
  };
  render();
  $("epochs").hidden = false;
}

// ---- every evonode ----

const SHOW = [["all", "All"], ["proposed", "Proposed this epoch"], ["idle", "Not in a validator set"], ["pending", "Upgraded, vote not counted"]];
const COLUMNS = [
  { key: "protx", label: "Evonode", get: (e) => e.protx, cell: (e) => el("td", {}, mnLink(e.protx), e.status !== "enabled" ? [" ", badge("serious", "PoSe-banned")] : null,
    e.service ? el("small", { class: "under mono", text: noPort(e.service) }) : null) },
  { key: "epoch_blocks", label: "Blocks this epoch", r: true, get: (e) => e.epoch_blocks, cell: (e) => el("td", { class: "r", text: fmt(e.epoch_blocks) }) },
  { key: "last_epoch", label: "Last epoch", r: true, get: (e) => e.last_epoch,
    cell: (e) => el("td", { class: "r" }, amount(e.last_epoch), el("small", { class: "under", text: `${fmt(e.last_blocks)} blocks` })) },
  { key: "d30", label: "30 days", r: true, get: (e) => e.d30, cell: (e) => el("td", { class: "r", text: amount(e.d30) }) },
  { key: "monthly", label: "A month (est.)", r: true, get: monthlySortKey, cell: (e) => {
    const m = monthlyCell(e, { amount });
    return el("td", { class: "r", title: m.title }, m.text, m.under ? el("small", { class: "under", text: m.under }) : null);
  } },
  { key: "balance", label: "Claimable", r: true, get: (e) => e.balance ?? -1, cell: (e) => el("td", { class: "r", text: e.balance != null ? amount(e.balance) : "–" }) },
  { key: "next", label: "Next block", r: true, get: (e) => e.next ?? Infinity,
    cell: (e) => el("td", { class: "r", title: e.next ? dateTime(e.next) : doc.schedule ? "Not in a validator set that takes turns now" : "Unknown: the validator sets haven't been read lately" },
      e.next ? inAbout(e.next) : doc.schedule ? "–" : "unknown") },
  { key: "vote", label: "Vote this epoch", r: true, get: (e) => e.vote ?? 0, cell: (e) => el("td", { class: "r", text: e.vote ?? "–" }) },
  { key: "drive", label: "Drive", get: (e) => e.drive || "", cell: (e) => el("td", { text: e.drive || "–" }) },
];

function renderAll() {
  const v = doc.upgrade.candidate;
  const state = { q: "", show: "all", sort: "monthly", dir: -1, page: 0 };
  const PAGE = 50;
  $("all-lede").textContent = `Every registered evonode: what it earned, what it can withdraw, and when its next block is due. ` +
    `"A month" is at the pace of ${lastSpan(doc.returns.monthly_basis_days) ?? "its last six finished epochs, once there are six"}` +
    `${doc.evonodes.some((e) => "monthly_since" in e) ? ", or a newer evonode's own pace since it registered" : ""}. Click a column to sort; open an evonode for its history.`;
  const matches = (e) => {
    if (state.show === "proposed" && !e.epoch_blocks) return false;
    if (state.show === "idle" && (e.next || e.status !== "enabled")) return false;
    if (state.show === "pending" && !(v != null && e.supports >= v && e.vote !== v && e.status === "enabled")) return false;
    const q = state.q.trim().toLowerCase();
    return !q || `${e.protx} ${e.service || ""} ${e.drive || ""}`.toLowerCase().includes(q);
  };
  const render = () => {
    const col = COLUMNS.find((c) => c.key === state.sort);
    const list = doc.evonodes.filter(matches).sort((a, b) => {
      const x = col.get(a), y = col.get(b);
      return (x < y ? -1 : x > y ? 1 : 0) * state.dir || (a.protx < b.protx ? -1 : 1);
    });
    const pages = Math.max(1, Math.ceil(list.length / PAGE));
    state.page = Math.min(state.page, pages - 1);
    const head = el("tr", {}, ...COLUMNS.map((c) => el("th", {
      class: `sortable${c.r ? " r" : ""}`, scope: "col", tabindex: 0, text: c.label,
      "aria-sort": state.sort === c.key ? (state.dir > 0 ? "ascending" : "descending") : null,
      onclick: () => sortBy(c.key), onkeydown: (ev) => (ev.key === "Enter" || ev.key === " ") && (ev.preventDefault(), sortBy(c.key)),
    })));
    const labels = COLUMNS.map((c) => c.label);
    const rows = list.slice(state.page * PAGE, (state.page + 1) * PAGE).map((e) => labelled(labels, el("tr", {}, ...COLUMNS.map((c) => c.cell(e)))));
    $("all-table").replaceChildren(el("thead", {}, head), el("tbody", {}, ...(rows.length ? rows
      : [el("tr", {}, el("td", { colspan: COLUMNS.length, class: "empty", text: "No evonode matches." }))])));
    $("all-pager").replaceChildren(
      el("span", { text: `${fmt(list.length)} of ${fmt(doc.evonodes.length)} evonodes` }),
      el("button", { class: "btn", type: "button", disabled: state.page === 0, onclick: () => { state.page--; render(); }, text: "Previous" }),
      el("span", { text: `Page ${state.page + 1} of ${pages}` }),
      el("button", { class: "btn", type: "button", disabled: state.page >= pages - 1, onclick: () => { state.page++; render(); }, text: "Next" }));
  };
  const sortBy = (key) => {
    if (state.sort === key) state.dir = -state.dir;
    else { state.sort = key; state.dir = key === "protx" || key === "next" || key === "drive" ? 1 : -1; }
    state.page = 0;
    render();
  };
  const show = SHOW.filter(([k]) => (k !== "pending" || (v != null && doc.upgrade.phase === "voting")) && (k !== "idle" || doc.schedule));
  $("all-filters").replaceChildren(searchBox(state, () => { state.page = 0; render(); }, "ProTx, IP or Drive version", "Search evonodes"),
    segmented(show, state.show, (k) => { state.show = k; state.page = 0; render(); }, "Show"));
  render();
  $("all").hidden = false;
}

async function main() {
  try {
    doc = await api(`${net}/platform`);
  } catch (e) {
    $("banner").replaceChildren(el("div", { class: "banner", text: `Couldn't load Platform rewards: ${e.message}` }));
    return;
  }
  if (!doc.now) {
    $("banner").replaceChildren(el("div", { class: "banner", text: "Platform rewards haven't been collected yet. They appear within 15 minutes of the crawler starting." }));
    return;
  }
  if (net === "testnet") $("lede").append(" On testnet its DASH has no value.");
  const summary = await api(`${net}/summary`).catch(() => null);
  renderNow(summary);
  renderUpgrade();
  renderNext();
  renderEpochs();
  renderAll();
}

main();
