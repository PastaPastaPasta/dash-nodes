// /v24: Dash Core 24's activation (the crawler's activation.rs, /api/v1/{net}/activation).
import { api, chrome, currentNet, el, fmt, href, dateTime, shortHash, badge, labelled, share } from "./common.js";
import { svg, valueAxis } from "./charts.js";

chrome("v24");
const net = currentNet();
const $ = (id) => document.getElementById(id);
const CORE = "https://github.com/dashpay/dash/blob/v24.0.0";
/** A reading older than this is called stale (the collector reads every 30 s, the publisher every minute). */
const STALE_AFTER = 10 * 60;

const stat = (label, value, note, tone) =>
  el("div", { class: tone ? `stat-${tone}` : null }, el("dt", { text: label }), el("dd", { class: "num" }, value, note ? el("small", {}, note) : null));
/** A block and its time: the block's own, or an estimate at the recent pace. */
const when = (m) => (m.time == null ? `block ${fmt(m.height)}` : `block ${fmt(m.height)}, ${m.estimated ? "about " : ""}${dateTime(m.time)}`);
const day = (m) => (m.time == null ? "" : new Date(m.time * 1000).toLocaleDateString("en-GB", { day: "numeric", month: "short", timeZone: "UTC" }));
/**
 * Draws `draw(width)` into `box` now and whenever its width changes. Unlike charts.js's `sized`, calling
 * it again for the same box (a refresh) swaps the drawing without adding another observer.
 */
const sizedBoxes = new Map();
function drawSized(box, draw) {
  let c = sizedBoxes.get(box);
  if (!c) {
    c = { draw, w: 0 };
    sizedBoxes.set(box, c);
    new ResizeObserver(() => redraw(box, true)).observe(box);
  }
  c.draw = draw;
  redraw(box, false);
}
function redraw(box, onResize) {
  const c = sizedBoxes.get(box);
  const w = Math.round(box.clientWidth);
  if (!w || (onResize && w === c.w)) return;
  c.w = w;
  box.replaceChildren(c.draw(w));
}

const STATES = {
  defined: ["none", "Not started"], started: ["warning", "Signalling"], locked_in: ["good", "Locked in"],
  active: ["good", "Active"], failed: ["critical", "Failed"],
};

let shown = null;
function render(doc) {
  const d = doc.deployments.find((x) => x.name === "v24");
  if (!d) throw new Error("no v24 deployment in the data");
  // Redraw only when there's something new, and keep keyboard focus on the same thing.
  const stale = Date.now() / 1000 - doc.updated > STALE_AFTER;
  const key = JSON.stringify([doc.tip.height, doc.error?.message, stale, d.ehf.gate?.checked]);
  if (key === shown) return;
  shown = key;
  const focused = document.activeElement?.dataset?.key;
  const o = d.outlook || {};
  const pace = doc.block_seconds;
  renderBanner(doc, d, stale);
  renderStatus(doc, d, o);
  renderSignal(d, o);
  renderMiners(d, o, pace);
  renderThresholds(d, o, pace);
  renderHow(doc, d);
  if (focused) document.querySelector(`[data-key="${focused}"]`)?.focus();
}

// ---- freshness and trust ----

function renderBanner(doc, d, stale) {
  const notes = [];
  // Times as dates, not "N min ago": the banner is a live region, so it shouldn't change every minute.
  if (doc.error) {
    notes.push(el("p", {}, el("strong", { text: "The latest reading failed. " }),
      `Showing the one from ${dateTime(doc.updated)} (block ${fmt(doc.tip.height)}): ${doc.error.message}`));
  } else if (stale) {
    notes.push(el("p", {}, el("strong", { text: "This reading is stale. " }),
      `It's from ${dateTime(doc.updated)} (block ${fmt(doc.tip.height)}); our dashd or the crawler host may be down.`));
  }
  if (d.rpc == null) {
    notes.push(el("p", {}, el("strong", { text: "Our dashd doesn't report this deployment. " }),
      `It runs ${doc.dashd || "an unknown version"}; states below are worked out from the block headers alone.`));
  } else if (d.consistent === false) {
    notes.push(el("p", {}, el("strong", { text: "Our reading disagrees with our dashd's. " }), d.mismatches.join("; ")));
  }
  $("banner").replaceChildren(...(notes.length ? [el("div", { class: "banner" }, ...notes)] : []));
}

// ---- where it stands ----

function headline(d, o) {
  const p = d.params;
  const start = dateTime(p.start_time);
  switch (o.phase) {
    case "waiting_for_signal":
      return o.start_time_reached
        ? "The start time has passed; v24 is waiting for the masternodes' signal to be mined."
        : `v24 can't start before ${start} (the chain's median time). Then masternodes signal it, and miners vote window by window.`;
    case "waiting_for_window":
      return `The masternodes' signal is in block ${fmt(d.ehf.height)}; miners start signalling at block ${fmt(o.earliest_start.height)}.`;
    case "signalling":
      return `Miners are signalling: ${fmt(d.window.count)} of the ${fmt(d.window.threshold)} blocks this window needs, with ${fmt(o.remaining)} blocks to go.`;
    case "locked_in":
      return `v24 is locked in. It activates at ${when(o.activation)}.`;
    case "active":
      return `v24 is active since ${when(o.activation)}.`;
    case "failed":
      return `v24 failed: it reached its timeout without locking in (${when(o.failed)}).`;
    case "timed_out":
      return `v24's timeout (${dateTime(o.timeout)}) passed before the masternodes' signal was mined, so it can't start any more.`;
    default:
      return "";
  }
}

function renderStatus(doc, d, o) {
  const p = d.params;
  const w = d.window;
  const [kind, label] = STATES[d.state] || ["none", d.state];
  $("lede").textContent = headline(d, o);
  // The threshold that applies to the next window that counts: this one while signalling, the first otherwise.
  const t = w?.threshold ?? (o.phase === "signalling" ? null : o.first_threshold) ?? p.threshold_start;
  const thresholdNote = w?.attempt != null
    ? `${fmt(t)} of ${fmt(p.window)} blocks, on attempt ${w.attempt + 1}; ${o.next_threshold != null ? `${share(o.next_threshold, p.window)} next window` : ""}`
    : `${fmt(t)} of ${fmt(p.window)} blocks in the first window, falling to ${share(p.threshold_min, p.window)}`;
  const stats = [
    stat("Status", badge(kind, label), d.since ? `since block ${fmt(d.since)}` : `start time ${dateTime(p.start_time)}`),
    ["defined", "started"].includes(d.state) ? stat("Lock-in threshold", share(t, p.window), thresholdNote) : null,
    o.phase === "signalling"
      ? stat("This window", `${fmt(w.count)} / ${fmt(w.threshold)}`,
        !o.needed ? `threshold reached: locks in at ${when(o.lock_in_if_reached)}`
          : `${o.possible ? `${fmt(o.needed)} more needed in ${fmt(o.remaining)} blocks` : "can't reach it any more"}; ends ${when(o.window_end)}`,
        !o.needed ? "good" : o.possible ? null : "warning")
      : o.earliest_start ? stat("Next window that counts", `block ${fmt(o.earliest_start.height)}`,
        `${o.earliest_start.estimated ? "about " : ""}${dateTime(o.earliest_start.time)}${o.phase === "waiting_for_signal" ? ", at the earliest" : ""}`) : null,
    o.earliest_activation ? stat("Earliest activation", `block ${fmt(o.earliest_activation.height)}`, `about ${dateTime(o.earliest_activation.time)}`) : null,
    o.activation_if_reached ? stat("Activation if this window locks in", `block ${fmt(o.activation_if_reached.height)}`, `about ${dateTime(o.activation_if_reached.time)}`) : null,
    o.activation && o.phase !== "active" ? stat("Activation", `block ${fmt(o.activation.height)}`, `about ${dateTime(o.activation.time)}`) : null,
    stat("Masternode signal", d.ehf.height ? `block ${fmt(d.ehf.height)}` : "not mined yet",
      d.ehf.height ? (d.ehf.block?.time ? dateTime(d.ehf.block.time) : "") : "needed before any window counts", d.ehf.height ? "good" : null),
  ].filter(Boolean);
  $("status-stats").replaceChildren(...stats);
  $("milestones").replaceChildren(...milestones(d, o).map(([what, m, done]) =>
    el("li", { class: done ? "done" : null }, el("span", { class: "what", text: what }),
      el("span", { class: "at" }, m ? `${when(m)}${m.estimated ? " (estimate)" : ""}` : "–"))));
  $("status-note").replaceChildren(
    `Read from our dashd (${doc.dashd || "unknown version"}) at block ${fmt(doc.tip.height)}, ${dateTime(doc.updated)}. `,
    `Times to come are estimates at the last ${fmt(doc.pace_blocks)} blocks' pace, ${Math.round(doc.block_seconds)} s a block; `,
    "a slower or faster chain moves them, and the masternode signal can come later than its earliest block.");
  $("status").hidden = false;
}

/** The road from here: [label, milestone, already happened]. */
function milestones(d, o) {
  const p = d.params;
  const tip = (m) => m && !m.estimated;
  switch (o.phase) {
    case "waiting_for_signal":
    case "waiting_for_window":
      return [
        [`Median time reaches ${dateTime(p.start_time)}`, o.start_time_block, o.start_time_reached],
        ["Masternodes' signal mined", d.ehf.block || null, !!d.ehf.height],
        [`First window that can count opens${d.ehf.height ? "" : ", at the earliest"}`, o.earliest_start, false],
        ["Lock-in, if that window reaches the threshold", o.earliest_lock_in, false],
        ["Activation, at the earliest", o.earliest_activation, false],
      ];
    case "signalling":
      return [
        ["Masternodes' signal mined", d.ehf.block, true],
        [`This window ends (attempt ${d.window.attempt + 1})`, o.window_end, false],
        ["Lock-in, if it reaches the threshold", o.lock_in_if_reached, false],
        ["Activation, if this window locks in", o.activation_if_reached, false],
      ];
    case "locked_in":
      return [["Masternodes' signal mined", d.ehf.block, true], ["Locked in", { height: d.since, time: null, estimated: false }, true], ["Activation", o.activation, tip(o.activation)]];
    case "active":
      return [["Masternodes' signal mined", d.ehf.block, true],
        ["Locked in", { height: o.activation.height - p.window, time: null, estimated: false }, true], ["Active", o.activation, true]];
    default:
      return [];
  }
}

// ---- the masternode signal ----

function renderSignal(d, o) {
  const g = d.ehf.gate;
  if (d.ehf.height) {
    $("signal-lede").textContent = `The masternodes' signal for v24 is in ${when(d.ehf.block)}. From the first window after it, ` +
      "miners' blocks decide.";
    $("signal-meter").replaceChildren();
    $("signal-table").replaceChildren();
    $("signal-table").parentElement.hidden = true;
    $("signal-note").textContent = "";
    $("signal").hidden = false;
    return;
  }
  if (!g) { $("signal").hidden = true; return; }
  if (g.error) {
    $("signal-lede").textContent = `We couldn't read the signing quorum from our dashd: ${g.error}`;
    $("signal-meter").replaceChildren();
    $("signal-table").parentElement.hidden = true;
    $("signal-note").textContent = "";
    $("signal").hidden = false;
    return;
  }
  $("signal-table").parentElement.hidden = false;
  const sel = g.quorums.find((q) => q.selected) || g.quorums[0];
  const startNote = o.start_time_reached ? "It can sign now" : `It can sign once the chain's median time passes ${dateTime(d.params.start_time)}`;
  $("signal-lede").textContent = `Before miners' votes count, one ${g.llmq} quorum (${fmt(g.size)} masternodes) must sign the v24 signal: ` +
    `${fmt(g.threshold)} of its members, running Dash Core 24. ${startNote}; a miner then puts it in a block. ` +
    "Which quorum signs depends on the request and on which quorums are active then (a new one forms about every day), " +
    `so here are the ${fmt(g.quorums.length)} active now, the one that would sign now first.`;
  // Not coloured as "met" even past the mark: announcing 24 doesn't prove a member will sign (see the note).
  const mark = `${(g.threshold / g.size) * 100}%`;
  const meter = el("div", { class: "meter", role: "img",
    "aria-label": `${fmt(sel.current)} of ${fmt(sel.members)} members of the quorum that would sign now announce Dash Core 24; ${fmt(g.threshold)} signatures are needed` },
  el("span", { style: { width: `${Math.min(100, (sel.current / g.size) * 100)}%` } }),
  el("i", { style: { left: mark } }));
  $("signal-meter").replaceChildren(meter, el("div", { class: "meter-axis" },
    el("span", { text: `${fmt(sel.current)} of ${fmt(sel.members)} announce Dash Core 24` }),
    el("span", { style: { left: mark }, text: `${fmt(g.threshold)} must sign` }),
    el("span")));
  const heads = ["Quorum", "Formed at", "Announce 24", "Older", "No answer", "Valid members"];
  $("signal-table").replaceChildren(
    el("thead", {}, el("tr", {}, ...heads.map((h, i) => el("th", { class: i > 1 ? "r" : null, text: h })))),
    el("tbody", {}, ...g.quorums.map((q) => labelled(heads, el("tr", {},
      // Block hashes start with zeros: show what tells them apart.
      el("td", {}, el("span", { class: "mono", title: q.hash, text: shortHash(q.hash.replace(/^0+/, ""), 8) }), q.selected ? [" ", el("span", { class: "pill", text: "would sign now" })] : null),
      el("td", { text: q.height ? `block ${fmt(q.height)}` : "–" }),
      el("td", { class: "r" }, el("strong", { text: fmt(q.current) }), el("small", { class: "muted", text: ` ${share(q.current, q.members)}` })),
      el("td", { class: "r", text: fmt(q.older) }),
      el("td", { class: "r", text: fmt(q.unknown) }),
      el("td", { class: "r", text: `${fmt(q.valid)} of ${fmt(q.members)}` }))))));
  $("signal-note").replaceChildren(
    `"Announce 24" counts valid members whose latest handshake with our crawler, in the past week, gave protocol ${fmt(g.min_protocol)} or newer. `,
    "That's an estimate of who can sign, not the trigger itself: the signal needs that many actual signatures from one quorum, and members we couldn't reach may still sign. ",
    "It can also count too many: release candidates 1 to 4 announce the same version and protocol but never sign v24 on mainnet, so a masternode still on one should upgrade. ",
    "The share of all masternodes on 24 is on the ", el("a", { href: href("/#readiness"), text: "overview" }), `. Checked ${dateTime(g.checked)}; request id `,
    el("code", { text: g.request_id }), ".");
  $("signal").hidden = false;
}

// ---- miners ----

function renderMiners(d, o, pace) {
  const p = d.params;
  const started = ["started", "locked_in", "active", "failed"].includes(d.state) || d.windows.some((w) => w.state === "started");
  $("miners-lede").textContent = started
    ? `Each ${fmt(p.window)}-block window, blocks that set bit ${p.bit} (with the top version bits 001) count as signalling. ` +
      `A window with as many as its threshold locks v24 in. In the last ${fmt(d.recent.blocks)} blocks, ${fmt(d.recent.count)} signalled.`
    : `Miners can't vote yet. From the first window that counts, each ${fmt(p.window)}-block window's blocks that set bit ${p.bit} ` +
      `(with the top version bits 001) count; ${fmt(p.threshold_start)} of them (${share(p.threshold_start, p.window)}) lock v24 in, ` +
      `and each window that falls short lowers the bar for the next, down to ${fmt(p.threshold_min)} (${share(p.threshold_min, p.window)}). ` +
      `In the last ${fmt(d.recent.blocks)} blocks, ${fmt(d.recent.count)} already set bit ${p.bit}.`;
  renderWindow(d);
  renderWindows(d, o, pace);
  $("miners").hidden = false;
}

/** The current window: signalling blocks so far against its threshold, and where the pace leads. */
function renderWindow(d) {
  const p = d.params;
  const w = d.window;
  const box = $("window-box");
  if (d.state !== "started" || !w || w.threshold == null) { box.hidden = true; return; }
  box.hidden = false;
  const marks = w.signalling || "";
  const cum = [0];
  for (const c of marks) cum.push(cum.at(-1) + (c === "#" ? 1 : 0));
  const n = marks.length;
  const rate = n ? w.count / n : 0;
  const projected = Math.round(w.count + rate * (p.window - n));
  $("window-title").textContent = `This window (attempt ${w.attempt + 1}), block by block`;
  $("window-key").textContent = `blocks ${fmt(w.start.height)}–${fmt(w.end.height)}`;
  $("window-legend").replaceChildren(
    el("span", {}, el("i", { class: "line", style: { background: "var(--s1)" } }), "signalling blocks so far"),
    el("span", {}, el("i", { class: "line", style: { background: "var(--ink)" } }), `threshold, ${fmt(w.threshold)}`),
    el("span", {}, el("i", { class: "line dashed" }), `at this pace: ${fmt(projected)}`));
  drawSized($("window-chart"), (W) => {
    const H = 220, L = 46, B = 22, T = 10, R = 10;
    const max = p.window;
    const row = (v) => T + (H - T - B) * (1 - v / max);
    const col = (i) => L + (W - L - R) * (i / p.window);
    const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H, class: "window-chart", role: "img",
      "aria-label": `${fmt(w.count)} of ${fmt(n)} blocks so far signalled; the window needs ${fmt(w.threshold)} of ${fmt(p.window)}; at this pace it ends with about ${fmt(projected)}` });
    valueAxis(root, { L, W: W - R, step: 1000, max, row });
    root.append(svg("line", { x1: L, x2: W - R, y1: row(w.threshold), y2: row(w.threshold), class: "threshold" }));
    let dpath = `M${col(0)},${row(0)}`;
    const step = Math.max(1, Math.floor(n / 400));
    for (let i = step; i <= n; i += step) dpath += `L${col(i).toFixed(1)},${row(cum[i]).toFixed(1)}`;
    dpath += `L${col(n).toFixed(1)},${row(cum[n]).toFixed(1)}`;
    root.append(svg("path", { d: dpath, class: "actual" }));
    root.append(svg("line", { x1: col(n), x2: col(p.window), y1: row(cum[n]), y2: row(projected), class: "projection" }));
    for (const [v, anchor] of [[0, "start"], [p.window, "end"]]) {
      const t = svg("text", { x: col(v), y: H - 6, "text-anchor": anchor, class: "tick" });
      t.textContent = v ? `block ${fmt(p.window)}` : "block 1";
      root.append(t);
    }
    return root;
  });
}

/** Every window that counted, plus the ones to come, as the share of blocks that signalled against each threshold. */
function renderWindows(d, o, pace) {
  const p = d.params;
  const past = d.windows.filter((w) => w.state === "started");
  const slots = past.map((w) => ({ start: w.start, attempt: w.attempt, threshold: w.threshold, count: w.count, mined: w.mined, current: w.mined < p.window }));
  // Windows to come: from the first that can count (or the next one), until the threshold bottoms out.
  let next = null;
  if (o.phase === "waiting_for_signal" || o.phase === "waiting_for_window") next = { start: o.earliest_start, attempt: 0 };
  else if (o.phase === "signalling" && o.next_window) next = { start: o.next_window, attempt: d.window.attempt + 1 };
  if (next) {
    for (let a = next.attempt, i = 0; i < 12; a++, i++) {
      const t = d.schedule[Math.min(a, d.schedule.length - 1)].threshold;
      const height = next.start.height + i * p.window;
      const time = next.start.time != null ? next.start.time + i * p.window * pace : null;
      slots.push({ start: { height, time, estimated: true }, attempt: a, threshold: t, count: null, future: true });
      if (t <= p.threshold_min) break;
    }
  }
  const list = slots.slice(-24);
  $("windows-key").textContent = "share of each window's blocks, threshold marked";
  $("windows-legend").replaceChildren(
    el("span", {}, el("i", { style: { background: "var(--s1)" } }), "signalled"),
    el("span", {}, el("i", { class: "line", style: { background: "var(--ink)" } }), "threshold"),
    el("span", {}, el("i", { class: "ghost" }), "windows to come (estimated dates)"));
  const box = $("windows-chart");
  if (!slots.length) { sizedBoxes.delete(box); box.replaceChildren(el("p", { class: "muted", text: "No window to show yet." })); return; }
  drawSized(box, (W) => {
    const H = 220, L = 46, B = 34, T = 10;
    const row = (v) => T + (H - T - B) * (1 - v);
    const slot = (W - L) / list.length;
    const center = (i) => L + slot * i + slot / 2;
    // A group of focusable windows, each an image with its own description; the table below has the numbers.
    const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H, class: "windows-chart", role: "group",
      "aria-label": `Signalling share and threshold for ${list.length} windows` });
    valueAxis(root, { L, W, step: 0.2, max: 1, row, format: (v) => `${Math.round(v * 100)}%` });
    const bw = Math.max(3, Math.min(26, slot * 0.62));
    list.forEach((s, i) => {
      const x = center(i);
      const g = svg("g", { tabindex: 0, role: "img", "data-key": `window-${s.start.height}`, class: s.future ? "future" : s.current ? "current" : null });
      const title = svg("title");
      const tShare = s.threshold / p.window;
      title.textContent = s.future
        ? `Attempt ${s.attempt + 1}, from ${when(s.start)} (estimate): needs ${fmt(s.threshold)} of ${fmt(p.window)} (${share(s.threshold, p.window)})`
        : `Attempt ${s.attempt + 1}, from block ${fmt(s.start.height)}: ${fmt(s.count)} of ${fmt(s.mined)} blocks ${s.current ? "so far " : ""}signalled; ` +
          `${s.current ? "needs" : "needed"} ${fmt(s.threshold)} (${share(s.threshold, p.window)})${s.current ? "" : s.count >= s.threshold ? ", locked in" : ", short"}`;
      g.setAttribute("aria-label", title.textContent);
      g.append(title, svg("rect", { x: x - bw / 2, y: row(1), width: bw, height: row(0) - row(1), rx: 2, class: "slot" }));
      if (!s.future) {
        const v = s.count / p.window;
        g.append(svg("rect", { x: x - bw / 2, y: row(v), width: bw, height: Math.max(0, row(0) - row(v)), rx: 2, class: s.count >= s.threshold ? "bar met" : "bar" }));
      }
      g.append(svg("rect", { x: x - bw / 2 - 2, y: row(tShare) - 1, width: bw + 4, height: 2.5, class: "mark" }));
      root.append(g);
    });
    const every = Math.ceil(list.length / Math.max(2, Math.floor(W / 64)));
    list.forEach((s, i) => {
      if ((list.length - 1 - i) % every) return;
      const t = svg("text", { x: center(i), y: H - 18, "text-anchor": "middle", class: "tick" });
      t.textContent = `#${s.attempt + 1}`;
      const t2 = svg("text", { x: center(i), y: H - 5, "text-anchor": "middle", class: "tick" });
      t2.textContent = s.start.time != null ? day(s.start) : fmt(s.start.height);
      root.append(t, t2);
    });
    return root;
  });
}

// ---- thresholds ----

function renderThresholds(d, o, pace) {
  const p = d.params;
  const heads = ["Window", "From block", "Starts", "Threshold", "Share", "Signalled"];
  const done = new Map(d.windows.filter((w) => w.attempt != null).map((w) => [w.attempt, w]));
  // Windows to come are dated from the next one (anchored where the tip is, like the milestones): the
  // earliest first window while waiting, the next window while signalling, none once it's decided.
  const anchor = o.phase === "signalling" ? [d.window.attempt + 1, o.next_window]
    : o.earliest_start ? [0, o.earliest_start] : null;
  const decided = ["locked_in", "active", "failed"].includes(d.state);
  // The schedule's windows, then (on a network where it took longer) the last ones that counted.
  const beyond = [...done.keys()].filter((a) => a >= d.schedule.length).sort((a, b) => a - b).slice(-3);
  const floor = d.schedule.at(-1).threshold;
  const rows = [...d.schedule, ...beyond.map((a) => ({ attempt: a, threshold: floor, gap: a > d.schedule.length && a === beyond[0] }))].map((s) => {
    const w = done.get(s.attempt);
    const k = anchor ? s.attempt - anchor[0] : -1;
    const start = w ? w.start : !decided && k >= 0 ? { height: anchor[1].height + k * p.window,
      time: anchor[1].time != null ? Math.round(anchor[1].time + k * p.window * pace) : null, estimated: true } : null;
    const isNow = d.window?.attempt === s.attempt && d.state === "started";
    return labelled(heads, el("tr", { class: [isNow ? "now" : null, s.gap ? "gap" : null].filter(Boolean).join(" ") || null },
      el("td", {}, `#${s.attempt + 1}`, isNow ? [" ", el("span", { class: "pill", text: "now" })] : null),
      el("td", { class: "num", text: start ? fmt(start.height) : "–" }),
      el("td", { text: start?.time != null ? `${start.estimated ? "about " : ""}${dateTime(start.time)}` : "–" }),
      el("td", { class: "r num", text: `${fmt(s.threshold)} of ${fmt(p.window)}` }),
      el("td", { class: "r num", text: share(s.threshold, p.window) }),
      el("td", { class: "r num", text: w ? `${fmt(w.count)}${w.mined < p.window ? ` of ${fmt(w.mined)} so far` : ""}` : "–" })));
  });
  $("thresholds-lede").textContent = `The threshold starts at ${fmt(p.threshold_start)} of ${fmt(p.window)} blocks and drops after each window ` +
    `that misses it: ${fmt(p.threshold_start)} − attempt² × ${fmt(p.window)} ÷ 100 ÷ ${p.falloff_coeff}, rounded down at each step, ` +
    `never below ${fmt(p.threshold_min)}. ` + (decided ? "" : done.size ? "Dates of windows to come are estimates."
      : "Window dates are estimates from the earliest possible start; if the signal comes later, they all move by whole windows.");
  $("thresholds-table").replaceChildren(
    el("thead", {}, el("tr", {}, ...heads.map((h, i) => el("th", { class: i > 2 ? "r" : null, text: h })))),
    el("tbody", {}, ...rows));
  $("thresholds").hidden = false;
}

// ---- how it works ----

function renderHow(doc, d) {
  const p = d.params;
  const src = (path, text) => el("a", { href: `${CORE}/${path}`, text });
  const evo = doc.deployments.find((x) => x.name === "evo_shares");
  $("how-body").replaceChildren(
    el("p", {}, `v24 is a hard fork (deployment bit ${p.bit}) with two gates. First the masternodes: after ${dateTime(p.start_time)} `,
      `(by the chain's median time past), a ${doc.mnhf} quorum signs the v24 request and a miner puts the signature in a block. `,
      "This is DIP-0023's enhanced hard fork signal (EHF). Until it's mined, miners' blocks don't count, however many signal."),
    el("p", {}, `Then the miners: the chain is cut into ${fmt(p.window)}-block windows, starting at heights that are multiples of ${fmt(p.window)}. `,
      "The first window whose previous block is at or past both the start time and the signal's block is the first that counts. ",
      `A window in which at least the threshold of blocks signal locks v24 in; the next window is locked in, and v24 is active from the one after. `,
      p.timeout ? `A window that doesn't lock in and whose last block's median time is at or past ${dateTime(p.timeout)} fails it.` : "There's no timeout on this network."),
    el("p", {}, "We read every block header, count the signals ourselves with Core's rules, and check the states against our dashd's ",
      el("code", { text: "getdeploymentinfo" }), ". Careful with that RPC's ", el("code", { text: "statistics" }), " on a window's last block: ",
      "its count is for the window just ended, but its threshold is already the next window's."),
    evo ? el("p", {}, `A second deployment, evo_shares (bit ${evo.params.bit}), has the same window and thresholds, but Dash Core 24.0.0 doesn't sign its masternode signal, so it can't start until a later release does. `,
      `It's ${STATES[evo.state]?.[1].toLowerCase() || evo.state} on ${net}.`) : null,
    el("p", { class: "muted small" }, "Sources, Dash Core v24.0.0: ", src("src/chainparams.cpp", "chainparams.cpp"), " (the parameters), ",
      src("src/versionbits.cpp", "versionbits.cpp"), " (states and thresholds), ", src("src/evo/mnhftx.cpp", "evo/mnhftx.cpp"), " and ",
      src("src/llmq/ehf_signals.cpp", "llmq/ehf_signals.cpp"), " (the masternode signal). The data: ",
      el("a", { href: `/api/v1/${net}/activation`, text: `/api/v1/${net}/activation` }), "."));
}

async function main() {
  try {
    render(await api(`${net}/activation`));
  } catch (e) {
    $("lede").textContent = "Where Dash Core 24's activation stands.";
    $("banner").replaceChildren(el("div", { class: "banner" }, el("strong", { text: "The activation data didn't load. " }),
      `${e.message}. It's read from our dashd every 30 seconds; try again in a minute.`));
  }
}

main();
setInterval(() => api(`${net}/activation`).then(render, () => {}), 60_000);
