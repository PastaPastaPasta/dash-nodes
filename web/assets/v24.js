// /v24: Dash Core 24's activation (the crawler's activation.rs, /api/v1/{net}/activation).
import { api, chrome, currentNet, el, everyVisible, fmt, href, dateTime, shortHash, badge, labelled, share } from "./common.js";
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
    c = { draw, w: 0, observer: new ResizeObserver(() => redraw(box, true)) };
    sizedBoxes.set(box, c);
    c.observer.observe(box);
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
  // Everything shown but the reading's own time (a same-height reorg changes counts, not the tip).
  const key = JSON.stringify([{ ...doc, updated: null }, stale]);
  if (key === shown) return;
  const focused = document.activeElement?.dataset?.key;
  const openTip = openNote?.note.isConnected ? openNote.note.id : null;
  const o = d.outlook || {};
  const pace = doc.block_seconds;
  renderBanner(doc, d, stale);
  renderStatus(doc, d, o);
  renderLockIn(doc, d);
  renderPools(d);
  if (openTip) $(openTip)?.showPopover();
  if (openNote?.note.closest("section[hidden]")) openNote.note.hidePopover();
  else if (openNote && !openNote.note.isConnected) { openNote.controller.abort(); openNote = null; }
  renderSignal(d, o);
  renderMiners(d, o, pace);
  renderThresholds(doc, d, o, pace);
  renderHow(doc, d);
  if (focused) document.querySelector(`[data-key="${focused}"]`)?.focus();
  shown = key; // only once drawn, so a reading that failed to draw is tried again
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
    case "signalling": {
      const now = `Miners are signalling: ${fmt(d.window.count)} of the ${fmt(d.window.threshold)} blocks this window needs, with ${fmt(o.remaining)} blocks to go.`;
      if (o.possible !== false) return now;
      const n = o.next_attempt;
      return n ? `${now} It can't get there, so the earliest activation is ${when(n.activation)}, if window #${n.attempt + 1} locks in.`
        : `${now} It can't get there, and no window starts after it before the timeout (${dateTime(p.timeout)}): v24 can no longer activate in this deployment.`;
    }
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
    ["defined", "started"].includes(d.state) && o.phase !== "signalling" ? stat("Lock-in threshold", share(t, p.window), thresholdNote) : null,
    o.phase === "signalling" ? null
      : o.earliest_start ? stat("Next window that counts", `block ${fmt(o.earliest_start.height)}`,
        `${o.earliest_start.estimated ? "about " : ""}${dateTime(o.earliest_start.time)}${o.phase === "waiting_for_signal" ? ", at the earliest" : ""}`) : null,
    o.earliest_activation ? stat("Earliest activation", `block ${fmt(o.earliest_activation.height)}`, `about ${dateTime(o.earliest_activation.time)}`) : null,
    ...activationStats(o),
    o.activation && o.phase !== "active" ? stat("Activation", `block ${fmt(o.activation.height)}`, `about ${dateTime(o.activation.time)}`) : null,
    stat("Masternode signal", d.ehf.height ? `block ${fmt(d.ehf.height)}` : "not mined yet",
      d.ehf.height ? (d.ehf.block?.time ? dateTime(d.ehf.block.time) : "") : "needed before any window counts", d.ehf.height ? "good" : null),
  ].filter(Boolean);
  $("status-stats").replaceChildren(...stats);
  $("milestones").replaceChildren(...milestones(d, o).map(([what, m, done]) =>
    el("li", { class: done ? "done" : null }, el("span", { class: "what", text: what }),
      el("span", { class: "at" }, m ? `${when(m)}${m.estimated ? " (estimate)" : ""}` : "–"))));
  $("status-note").textContent = `Our dashd (${doc.dashd || "unknown version"}), block ${fmt(doc.tip.height)}, ${dateTime(doc.updated)}. ` +
    `Times to come are estimates at ${Math.round(doc.block_seconds)} s a block.`;
  $("status").hidden = false;
}

/** While signalling: this window's activation if it can still lock in, else the earliest one that can. */
function activationStats(o) {
  if (o.phase !== "signalling") return [];
  if (o.possible) return [stat("Activation if this window locks in", `block ${fmt(o.activation_if_reached.height)}`, `about ${dateTime(o.activation_if_reached.time)}`)];
  const n = o.next_attempt;
  if (!n) return [stat("Activation", "no longer possible", "this window can't lock in, and it's the last before the timeout", "critical")];
  return [stat("Earliest activation", `block ${fmt(n.activation.height)}`,
    `about ${dateTime(n.activation.time)}, after lock-in at block ${fmt(n.lock_in.height)}`)];
}

// ---- can this window lock in ----

/** The share the outlook goes by: this window's once it has a day of blocks, else the last day's. */
function todaysPace(d) {
  const w = d.window;
  if (d.state === "started" && w && w.mined > 0 && w.mined >= d.recent.blocks) return { signalled: w.count, blocks: w.mined, basis: "window" };
  return d.recent.blocks > 0 ? { signalled: d.recent.count, blocks: d.recent.blocks, basis: "recent" } : null;
}

/** Whether the window being counted can still lock in, and whether it does at `rate`. */
function windowOutlook(d, rate) {
  const w = d.window;
  if (d.state !== "started" || w?.threshold == null) return null;
  const size = d.params.window;
  const remaining = size - w.mined;
  const needed = Math.max(0, w.threshold - w.count);
  const projected = Math.min(size, Math.floor(w.count + rate * remaining));
  const verdict = needed === 0 ? "reached" : needed > remaining ? "impossible" : projected >= w.threshold ? "on-track" : "needs-more";
  return { verdict, count: w.count, mined: w.mined, remaining, threshold: w.threshold, needed, projected, end: w.start.height + size - 1 };
}

/** A height's time (Unix seconds) at the recent pace from the tip. */
const estimate = (doc, height) => doc.tip.time + (height - doc.tip.height) * doc.block_seconds;

/**
 * The first window that locks in if miners keep signalling at `rate` (the one being counted by where its
 * pace leads, then each to come against its falling threshold), or `{ never: "floor" | "timeout" }`.
 */
function projectLockIn(doc, d, rate) {
  const p = d.params;
  if (!d.schedule.length) return null;
  const last = d.schedule.length - 1;
  const at = (attempt, start) => ({
    attempt, threshold: d.schedule[Math.min(attempt, last)].threshold, start, end: start + p.window - 1,
    lockIn: start + p.window, activation: start + 2 * p.window,
  });
  const timedOut = (x) => p.timeout != null && estimate(doc, x.end) >= p.timeout;
  const now = windowOutlook(d, rate);
  if (!now || d.window.attempt == null) return null;
  const current = at(d.window.attempt, d.window.start.height);
  if (now.verdict === "reached" || now.verdict === "on-track") return current;
  if (timedOut(current)) return { never: "timeout" };
  for (let a = current.attempt + 1, start = current.start + p.window; ; a++, start += p.window) {
    const x = at(a, start);
    if (Math.floor(rate * p.window) >= x.threshold) return x;
    if (timedOut(x)) return { never: "timeout" };
    if (a >= last || x.threshold <= p.threshold_min) return { never: "floor" };
  }
}

const ofShare = (n, of) => (of > 0 ? share(n, of) : "0.0%");
const onDay = (doc, height) => day({ time: estimate(doc, height) });
/** A share of a bar's width, as CSS. */
const at = (ratio) => `${Math.min(1, Math.max(0, ratio)) * 100}%`;
/** Places a label over a bar at `ratio` of its width, shifted by the same share of its own width so it never leaves the bar. */
const pinAt = (ratio) => ({ left: at(ratio), transform: `translateX(-${at(ratio)})` });

const INFO_ICON = '<svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true"><circle cx="8" cy="8" r="6.5" fill="none" stroke="currentColor" stroke-width="1.4"/><path d="M8 7.3v3.9" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><circle cx="8" cy="4.9" r="1" fill="currentColor"/></svg>';
/** The open note and the controller of its scroll and resize listeners. */
let openNote = null;
/**
 * An info button and its note, for the fine print a reading doesn't need at a glance. The note is a popover: it opens
 * on click, tap or Enter, and Escape or a click elsewhere closes it. Returns [button, note]; the note goes anywhere.
 */
function infoTip(id, label, ...body) {
  const note = el("div", { id, popover: "auto", class: "v24-tip" }, ...body);
  const button = el("button", { type: "button", class: "v24-info", "aria-label": label, title: label, popovertarget: id, "data-key": id });
  button.innerHTML = INFO_ICON;
  const place = () => {
    const b = button.getBoundingClientRect(), t = note.getBoundingClientRect(), m = 12;
    const left = Math.max(m, Math.min(b.left + b.width / 2 - t.width / 2, document.documentElement.clientWidth - m - t.width));
    const below = b.bottom + 6, above = b.top - 6 - t.height;
    note.style.left = `${left}px`;
    note.style.top = `${below + t.height <= innerHeight - m || above < m ? below : above}px`;
  };
  // Placed in the frame it opens in. A redraw removes an open note without a toggle event, so the next note to open
  // lets go of its listeners; a note that closes lets go only of its own, as the next one may already be open.
  note.addEventListener("beforetoggle", (e) => {
    if (e.newState === "open") {
      openNote?.controller.abort();
      openNote = { note, controller: new AbortController() };
      requestAnimationFrame(place);
      for (const ev of ["scroll", "resize"]) addEventListener(ev, place, { capture: true, passive: true, signal: openNote.controller.signal });
    } else if (openNote?.note === note) {
      openNote.controller.abort();
      openNote = null;
    }
  });
  return [button, note];
}

const VERDICT = {
  reached: ["good", "Threshold reached"],
  "on-track": ["good", "On pace"],
  "needs-more": ["warning", "Not on pace"],
  impossible: ["critical", "Out of reach"],
};
const poolName = (q) => q.pool ?? "Untagged";
/** A legend entry: swatch, label, figure. */
const legendKey = (cls, label, value) => el("span", {}, el("i", { class: cls }), label, el("b", { text: value }));

function renderLockIn(doc, d) {
  const pace = todaysPace(d);
  const rate = pace ? pace.signalled / pace.blocks : 0;
  const ratePct = pace ? share(pace.signalled, pace.blocks) : "–";
  const w = pace ? windowOutlook(d, rate) : null;
  $("lock-in").hidden = !w;
  if (!w) return;
  const p = d.params;
  const [tone, chip] = VERDICT[w.verdict];
  const needsShare = `Needs ${ofShare(w.needed, w.remaining)} of the blocks left; signalling runs at ${ratePct}.`;
  const line = {
    reached: `v24 locks in at block ${fmt(w.end + 1)}.`,
    "on-track": needsShare,
    "needs-more": needsShare,
    impossible: `Needs ${fmt(w.needed)} more signalling blocks; only ${fmt(w.remaining)} are left.`,
  }[w.verdict];
  $("lock-in-answer").dataset.verdict = w.verdict;
  $("lock-in-answer").replaceChildren(el("span", { class: `v24-verdict ${tone}`, text: chip }), el("span", { text: line }));
  renderWindowBar(w, p.window);
  $("lock-in-stats").replaceChildren(
    stat("Signalling", w.mined > 0 ? ofShare(w.count, w.mined) : "–", w.mined > 0 ? `${fmt(w.count)} of ${fmt(w.mined)}` : null),
    stat("Blocks left", fmt(w.remaining), `ends about ${onDay(doc, w.end)}`),
    stat("Needed", fmt(w.threshold), `${share(w.threshold, p.window)} of ${fmt(p.window)}`),
    stat("Needed from the rest", w.needed > w.remaining ? "out of reach" : ofShare(w.needed, w.remaining),
      `${fmt(w.needed)} of ${fmt(w.remaining)}`, tone));
  renderProjection(doc, d, pace, rate, ratePct);
}

/**
 * The window as one bar of its blocks: those that signalled fill from the left, those that didn't from the right, and
 * the blocks still to come split between them at today's pace. The threshold is a marker; it locks in when green
 * reaches it.
 */
function renderWindowBar(w, size) {
  const x = (n) => at(n / size);
  const lost = w.mined - w.count;
  const projected = Math.max(w.count, Math.min(w.projected, size - lost));
  const paced = w.remaining > 0;
  const seg = (cls, style) => el("i", { class: cls, style });
  $("window-bar").replaceChildren(
    el("div", { class: `v24-bar${paced ? " paced" : ""}`, role: "img",
      "aria-label": `This window's ${fmt(size)} blocks: ${fmt(w.count)} signalled, ${fmt(lost)} did not and ${fmt(w.remaining)} are left. ` +
        `${fmt(w.threshold)} must signal; at today's pace about ${fmt(projected)} do.` },
    el("span", { class: "v24-mlabel", style: pinAt(w.threshold / size), "aria-hidden": "true", text: `${fmt(w.threshold)} needed` }),
    el("div", { class: "v24-track", "aria-hidden": "true" },
      seg("v24-yes", { left: "0", width: x(w.count) }),
      seg("v24-yes v24-paced", { left: x(w.count), width: x(projected - w.count) }),
      seg("v24-no v24-paced", { left: x(projected), width: x(size - lost - projected) }),
      seg("v24-no", { right: "0", width: x(lost) })),
    el("b", { class: "v24-mark", style: { left: x(w.threshold) }, "aria-hidden": "true" }),
    paced ? [
      el("b", { class: "v24-pace", style: { left: x(projected) }, "aria-hidden": "true" }),
      el("span", { class: "v24-plabel", style: pinAt(projected / size), "aria-hidden": "true", text: `about ${fmt(projected)} at today's pace` }),
    ] : null),
    el("div", { class: "legend", "aria-hidden": "true" },
      legendKey("v24-yes", "Signalled ", fmt(w.count)),
      legendKey("v24-no", "Not signalled ", fmt(lost)),
      paced ? legendKey("v24-sw-pace", "Left, at today's pace ", fmt(w.remaining)) : null));
}

/** Which window locks in at today's pace, in one line; how that's worked out sits behind an info button. */
function renderProjection(doc, d, pace, rate, ratePct) {
  const p = d.params;
  const x = projectLockIn(doc, d, rate);
  const box = $("projection");
  box.hidden = !x;
  box.dataset.lockIn = x ? (x.never ?? String(x.attempt)) : "";
  if (!x) return box.replaceChildren();
  const basis = pace.basis === "window"
    ? `Today's pace is this window's so far: ${fmt(pace.signalled)} of ${fmt(pace.blocks)} blocks signalled (${ratePct}).`
    : `Today's pace is the last ${fmt(pace.blocks)} blocks': ${fmt(pace.signalled)} signalled (${ratePct}).`;
  const floor = p.threshold_min / p.window;
  const gap = Math.max(0.1, Math.round((floor - rate) * 1000) / 10);
  const short = x.never === "timeout" ? "At today's pace no window locks in before the timeout."
    : x.never === "floor" ? `At today's pace no window locks in: even the lowest threshold is ${share(p.threshold_min, p.window)}.`
    : `At today's pace window #${x.attempt + 1} locks in, about ${onDay(doc, x.end)}.`;
  const outcome = x.never === "timeout" ? `At that pace no window locks in before the timeout, ${dateTime(p.timeout)}.`
    : x.never === "floor" ? `At that pace no window locks in: even the lowest threshold, ${fmt(p.threshold_min)} of ${fmt(p.window)} blocks (${share(p.threshold_min, p.window)}), is above it. Signalling has to rise by about ${gap.toFixed(1)} percentage points.`
    : `At that pace window #${x.attempt + 1} (blocks ${fmt(x.start)}–${fmt(x.end)}, threshold ${fmt(x.threshold)} of ${fmt(p.window)}) is the first to lock in, about ${onDay(doc, x.end)}. v24 would then activate at block ${fmt(x.activation)}, about ${onDay(doc, x.activation)}.`;
  const [button, note] = infoTip("projection-tip", "How the outlook is worked out",
    el("p", { text: `${basis} ${outcome}` }),
    el("p", { class: "muted", text: `An estimate: the share moves as pools switch, and dates assume ${Math.round(doc.block_seconds)} s a block.` }));
  box.replaceChildren(el("span", { text: short }), " ", button, note);
}

/** Each pool's signalling blocks, then its blocks that didn't signal: a pool that signals for some is in both. */
function segmentsOf(pools) {
  const yes = pools.filter((q) => q.signalling > 0).map((q) => ({ name: poolName(q), blocks: q.signalling, on: true }));
  const no = pools.filter((q) => q.blocks > q.signalling).map((q) => ({ name: poolName(q), blocks: q.blocks - q.signalling, on: false }))
    .sort((a, b) => b.blocks - a.blocks);
  return [...yes, ...no];
}

/** Blocks and signalling blocks per pool this window, by coinbase tag. Hidden until the collector has them. */
function renderPools(d) {
  const w = d.window;
  const pools = d.state === "started" ? w?.pools : null;
  $("pools").hidden = !pools?.length || !(w.mined > 0);
  if ($("pools").hidden) return;
  const [button, note] = infoTip("pools-tip", "About the pool tags",
    el("p", { text: `Who mined this window's ${fmt(w.mined)} blocks and how many of them signalled. Best effort: we go by the tag in each block's coinbase, which is whatever the miner wrote, and some blocks carry none.` }));
  $("pools-lede").replaceChildren(`This window's ${fmt(w.mined)} blocks by pool. `, button, note);
  const segments = segmentsOf(pools);
  const blocker = segments.find((s) => !s.on);
  const yes = segments.filter((s) => s.on).reduce((a, s) => a + s.blocks, 0);
  const needed = w.threshold == null ? null : w.threshold / d.params.window;
  const neededShare = w.threshold == null ? null : share(w.threshold, d.params.window);
  const say = (s) => `${s.name}: ${ofShare(s.blocks, w.mined)}, ${s.on ? "signalling" : "not signalling"}`;
  $("pools-bar").replaceChildren(
    el("div", { class: "v24-bar", role: "img", "aria-label": `Share of this window's blocks by pool: ${segments.map(say).join("; ")}${needed != null ? `. ${neededShare} must signal` : ""}` },
      needed != null ? el("span", { class: "v24-mlabel", style: pinAt(needed), "aria-hidden": "true", text: `${neededShare} needed` }) : null,
      el("div", { class: "v24-pools", "aria-hidden": "true" }, ...segments.map((s) =>
        el("span", { class: s.on ? "v24-yes" : "v24-no", style: { flexGrow: String(s.blocks) }, title: say(s), "data-pool": s.name, "data-blocker": s === blocker },
          el("span", { class: "v24-seg" }, el("span", { class: "v24-seg-name", text: s.name }), el("span", { class: "v24-seg-pct", text: ` ${ofShare(s.blocks, w.mined)}` }),
            s === blocker ? el("span", { class: "v24-seg-not", text: " · not signalling" }) : null)))),
      needed != null ? el("b", { class: "v24-mark tick", style: { left: at(needed) }, "aria-hidden": "true" }) : null),
    el("div", { class: "legend", "aria-hidden": "true" },
      legendKey("v24-yes", "Signalling ", ofShare(yes, w.mined)),
      legendKey("v24-no", "Not signalling ", ofShare(w.mined - yes, w.mined))));
  const heads = ["Pool", "Blocks", "Signalling"];
  $("pools-table").replaceChildren(
    el("thead", {}, el("tr", {}, ...heads.map((h, i) => el("th", { class: i === 1 ? "r" : null, text: h })))),
    el("tbody", {}, ...pools.map((q) => labelled(heads, el("tr", {},
      el("td", { text: poolName(q) }),
      el("td", { class: "r num" }, el("b", { text: fmt(q.blocks) }), " ", el("small", { class: "muted", text: ofShare(q.blocks, w.mined) })),
      el("td", {}, q.signalling === q.blocks ? badge("good", "yes") : q.signalling === 0 ? badge("warning", "no")
        : badge("none", `${fmt(q.signalling)} of ${fmt(q.blocks)}`)))))));
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
    case "signalling": {
      const signal = ["Masternodes' signal mined", d.ehf.block, true];
      const ends = `This window ends (attempt ${d.window.attempt + 1})`;
      if (o.possible) return [signal, [ends, o.window_end, false],
        ["Lock-in, if it reaches the threshold", o.lock_in_if_reached, false],
        ["Activation, if this window locks in", o.activation_if_reached, false]];
      const n = o.next_attempt;
      if (!n) return [signal, [`${ends}, out of reach and the last before the timeout`, o.window_end, false],
        ["v24 fails; it can no longer activate", o.next_window, false]];
      return [signal, [`${ends}, out of reach`, o.window_end, false],
        [`Next window ends (attempt ${n.attempt + 1})`, n.window_end, false],
        ["Lock-in, if it reaches the threshold", n.lock_in, false],
        ["Earliest activation", n.activation, false]];
    }
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
  if (o.phase === "timed_out") {
    $("signal-lede").textContent = `The timeout (${dateTime(o.timeout)}) passed without the signal: masternodes no longer sign it, and no block may carry it.`;
    $("signal-meter").replaceChildren();
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
    ? `Blocks that set bit ${p.bit} (with the top version bits 001) count as signalling. In the last ${fmt(d.recent.blocks)} blocks, ${fmt(d.recent.count)} did.`
    : `Miners can't vote yet. From the first window that counts, blocks that set bit ${p.bit} (with the top version bits 001) count as signalling. ` +
      `In the last ${fmt(d.recent.blocks)} blocks, ${fmt(d.recent.count)} already set it.`;
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
  else if (o.phase === "signalling" && o.next_attempt) next = { start: o.next_attempt.window_start, attempt: o.next_attempt.attempt };
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
  if (!slots.length) { sizedBoxes.get(box)?.observer.disconnect(); sizedBoxes.delete(box); box.replaceChildren(el("p", { class: "muted", text: "No window to show yet." })); return; }
  drawSized(box, (W) => {
    const H = 220, L = 46, B = 34, T = 10;
    const row = (v) => T + (H - T - B) * (1 - v);
    const slot = (W - L) / list.length;
    const center = (i) => L + slot * i + slot / 2;
    // A group of focusable windows, each an image with its own description; the table below has the numbers.
    const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H, class: "windows-chart", role: "group",
      "aria-label": `Signalling share and threshold for ${list.length} windows` });
    // Axes are for the eye; each window's own label says it all.
    const axis = svg("g", { "aria-hidden": "true" });
    root.append(axis);
    valueAxis(axis, { L, W, step: 0.2, max: 1, row, format: (v) => `${Math.round(v * 100)}%` });
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
    // Labels every few windows, counted from the last, and always the current one (the others give way to it).
    const every = Math.ceil(list.length / Math.max(2, Math.floor(W / 64)));
    const now = list.findIndex((s) => s.current);
    list.forEach((s, i) => {
      if (i !== now && ((list.length - 1 - i) % every || (now >= 0 && Math.abs(i - now) * slot < 56))) return;
      const t = svg("text", { x: center(i), y: H - 18, "text-anchor": "middle", class: "tick" });
      t.textContent = `#${s.attempt + 1}`;
      const t2 = svg("text", { x: center(i), y: H - 5, "text-anchor": "middle", class: "tick" });
      t2.textContent = s.start.time != null ? day(s.start) : fmt(s.start.height);
      axis.append(t, t2);
    });
    return root;
  });
}

// ---- thresholds ----

function renderThresholds(doc, d, o, pace) {
  const p = d.params;
  const heads = ["Window", "From block", "Starts", "Threshold", "Signalled"];
  const done = new Map(d.windows.filter((w) => w.attempt != null).map((w) => [w.attempt, w]));
  // Windows to come are dated from the next one (anchored where the tip is, like the milestones): the
  // earliest first window while waiting, the next window while signalling, none once it's decided.
  const anchor = o.phase === "signalling" ? (o.next_attempt ? [o.next_attempt.attempt, o.next_attempt.window_start] : null)
    : o.earliest_start ? [0, o.earliest_start] : null;
  const decided = ["locked_in", "active", "failed"].includes(d.state) || o.phase === "timed_out";
  // The schedule's windows, then (on a network where it took longer) the last ones that counted.
  const beyond = [...done.keys()].filter((a) => a >= d.schedule.length).sort((a, b) => a - b).slice(-3);
  const floor = d.schedule.at(-1).threshold;
  const pace2 = todaysPace(d);
  const projected = pace2 && !decided ? projectLockIn(doc, d, pace2.signalled / pace2.blocks) : null;
  const lockIn = projected && projected.never == null ? projected.attempt : null;
  const rows = [...d.schedule, ...beyond.map((a) => ({ attempt: a, threshold: floor, gap: a > d.schedule.length && a === beyond[0] }))].map((s) => {
    const w = done.get(s.attempt);
    const k = anchor ? s.attempt - anchor[0] : -1;
    const start = w ? w.start : !decided && k >= 0 ? { height: anchor[1].height + k * p.window,
      time: anchor[1].time != null ? Math.round(anchor[1].time + k * p.window * pace) : null, estimated: true } : null;
    const isNow = d.window?.attempt === s.attempt && d.state === "started";
    return labelled(heads, el("tr", { class: [isNow ? "now" : null, s.gap ? "gap" : null].filter(Boolean).join(" ") || null },
      el("td", {}, `#${s.attempt + 1}`, isNow ? [" ", el("span", { class: "pill", text: "now" })] : null,
        s.attempt === lockIn ? [" ", el("span", { class: "pill", text: "locks in at today's pace" })] : null),
      el("td", { class: "num", text: start ? fmt(start.height) : "–" }),
      el("td", {}, start?.time != null && start.estimated ? el("span", { class: "about", text: "about " }) : null, start?.time != null ? dateTime(start.time) : "–"),
      el("td", { class: "r num" }, fmt(s.threshold), el("small", { class: "muted", text: ` ${share(s.threshold, p.window)}` })),
      el("td", { class: w ? "r num" : "r num none", text: w ? `${fmt(w.count)}${w.mined < p.window ? ` of ${fmt(w.mined)} so far` : ""}` : "–" })));
  });
  $("thresholds-lede").textContent = `Each window needs this many of its ${fmt(p.window)} blocks to signal: ${fmt(p.threshold_start)} at first, ` +
    `falling after each window that misses, down to ${fmt(p.threshold_min)}. ` + (decided ? "" : done.size ? "Dates to come are estimates."
      : "Dates are estimates from the earliest start; if the signal comes later, they all move by whole windows.");
  $("thresholds-formula").textContent = `${fmt(p.threshold_start)} − attempt² × ${fmt(p.window)} ÷ 100 ÷ ${p.falloff_coeff}, rounded down at each step, ` +
    `never below ${fmt(p.threshold_min)}.`;
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
  $("how-summary").textContent = "Two gates. First the masternodes sign the v24 signal and a miner puts it in a block; then miners signal, " +
    "window by window. The first window that reaches its threshold locks v24 in, and it activates one window later.";
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

let last = null;
const load = () => api(`${net}/activation`).then((doc) => { last = doc; render(doc); });

async function main() {
  try {
    await load();
  } catch (e) {
    $("lede").textContent = "Where Dash Core 24's activation stands.";
    $("banner").replaceChildren(el("div", { class: "banner" }, el("strong", { text: "The activation data didn't load. " }),
      `${e.message}. It's read from our dashd every 30 seconds; try again in a minute.`));
  }
}

main();
// About once a block. If fetching fails, the last reading is drawn again, so it turns stale on the page as time passes.
everyVisible(() => load().catch(() => last && render(last)), 150_000);
