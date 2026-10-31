// The map's bottom sheet on phones. It keeps one height (the map less a strip at the top) whatever it
// shows, and moves only by transform, so a drag follows the finger without relayout. It rests
// "collapsed" (handle and head), "half" (half the map) or "full" (all its content, as far as it fits);
// a flick goes on to the next rest in its direction. A vertical drag anywhere on it moves it
// (touch-action: none in CSS), never the map or the page behind it; the body scrolls, natively, only
// at full and only when its content doesn't fit.

const ENGAGE = 8; // px of movement before a press becomes a drag (less is a tap)
const FLICK = 0.4; // px/ms: faster than this, a release goes on to the next rest in that direction
const COAST_MS = 150; // slower releases rest nearest where the motion would carry them in this long
const GIVE = 4; // past the first or last rest, the sheet moves sqrt(overshoot) * GIVE
const TOP_GAP = 56; // px of map kept visible above the sheet

/** Past the lowest or highest rest it gives a little, with resistance (`lo` < `hi`, in translateY). */
function resist(ty, lo, hi) {
  if (ty < lo) return lo - Math.sqrt(lo - ty) * GIVE;
  if (ty > hi) return hi + Math.sqrt(ty - hi) * GIVE;
  return ty;
}

/** The rest a release `cur` px up, moving at `v` px/ms (downward positive), comes to. */
function pickRest(names, rests, cur, v) {
  if (Math.abs(v) > FLICK) {
    const next = v > 0 ? names.findLast((n) => rests[n] < cur - 1) : names.find((n) => rests[n] > cur + 1);
    return next ?? (v > 0 ? names[0] : names.at(-1));
  }
  const aim = cur - v * COAST_MS;
  return names.reduce((a, n) => (Math.abs(rests[n] - aim) < Math.abs(rests[a] - aim) ? n : a));
}

/**
 * `panel` holds `handle`, `head` and `body` (a column). `active()` says whether it's a sheet right now
 * (phones). `restFor()` is the rest for what the panel shows, used when it becomes a sheet.
 * `onDragEnd(rest)` runs when a drag comes to rest.
 */
export function bottomSheet({ panel, handle, head, body, active, restFor, onDragEnd }) {
  let rests = { collapsed: 0, half: null, full: 0 };
  let height = 0; // the sheet's height; whatever is below the screen's edge is out of sight
  let at = "collapsed"; // the rest it's at or heading to
  let y = 0; // translateY: height less what shows
  let sheet = false; // whether it was a sheet at the last refresh
  let gesture = null; // the press in progress
  // The click that ends a drag (on whatever was under the finger): only that one, so it's dropped by
  // the next press and soon after the drag (a keyboard or assistive click comes without a press).
  let swallowUntil = 0;

  const place = (ty) => { y = ty; panel.style.transform = `translate3d(0, ${ty}px, 0)`; };
  const order = () => ["collapsed", "half", "full"].filter((n) => rests[n] != null);

  /** Recomputes the rests (px showing) from the content. */
  function measure() {
    const box = panel.parentElement.clientHeight;
    height = Math.max(0, box - TOP_GAP);
    panel.style.height = `${height}px`;
    const pad = parseFloat(getComputedStyle(panel).paddingBottom) || 0;
    const top = handle.offsetHeight + (head.offsetParent ? head.offsetHeight : 0); // no head in details
    // The body shrinks to its content (CSS), so scrollHeight is the content's height either way.
    const full = Math.round(Math.min(height, top + body.scrollHeight + pad));
    const collapsed = Math.min(full, top + pad);
    const half = Math.min(full, Math.max(collapsed, Math.round(box * 0.5)));
    // A half that's barely short of full is just a second full.
    rests = { collapsed, half: full - half > box * 0.2 ? half : null, full };
    if (rests[at] == null) at = "full";
  }

  function settle(name, animate = true) {
    measure();
    at = rests[name] != null ? name : "full"; // no half when it's barely short of full
    if (at !== "full") body.scrollTop = 0; // what scrolled away at full can't be reached from here
    if (!animate) panel.classList.add("dragging");
    place(height - rests[at]);
    if (!animate) { void panel.offsetHeight; panel.classList.remove("dragging"); } // instant: no transition
    panel.dataset.snap = at;
    panel.dataset.scrolls = String(at === "full" && body.scrollHeight > body.clientHeight + 1);
    // Out of sight below the edge: out of the tab order and away from screen readers too.
    body.inert = at === "collapsed";
    // The map's controls sit above the sheet, but no higher than half.
    document.documentElement.style.setProperty("--sheet-h", `${Math.min(rests[at], rests.half ?? rests.full)}px`);
  }

  // A press anywhere on the sheet can become a drag, except in a body that scrolls.
  panel.addEventListener("pointerdown", (e) => {
    gesture?.abort();
    swallowUntil = 0;
    if (!sheet || !e.isPrimary || (e.pointerType === "mouse" && e.button !== 0)) return;
    if (panel.dataset.scrolls === "true" && body.contains(e.target)) return;
    const id = e.pointerId, y0 = e.clientY, x0 = e.clientX;
    const names = order(), lo = height - rests.full, hi = height - rests[names[0]];
    // Touching it mid-slide stops it where it is on screen, as a native sheet does.
    const shown = new DOMMatrixReadOnly(getComputedStyle(panel).transform).m42;
    const caught = Math.abs(shown - y) > 0.5;
    if (caught) { panel.classList.add("dragging"); place(shown); }
    const start = y;
    let ty = y, engaged = false, frame = 0;
    const samples = [];
    const ac = gesture = new AbortController();
    const end = (ev) => {
      ac.abort();
      cancelAnimationFrame(frame);
      if (!engaged) {
        if (caught) { panel.classList.remove("dragging"); settle(at); } // a tap: carry on to the rest
        return;
      }
      swallowUntil = performance.now() + 300;
      panel.classList.remove("dragging");
      if (!sheet) return; // turned into the wide layout mid-drag
      // From the finger, not from what was painted (moves can come faster than frames). Velocity over
      // the last ~100 ms, none if the finger stopped before lifting.
      const recent = samples.filter((s) => ev.timeStamp - s.t < 100);
      const v = ev.type === "pointercancel" || recent.length < 2 ? 0
        : (recent.at(-1).finger - recent[0].finger) / Math.max(1, recent.at(-1).t - recent[0].t);
      y = ty;
      settle(pickRest(names, rests, height - ty, v));
      onDragEnd?.(at);
    };
    panel.addEventListener("pointermove", (ev) => {
      if (ev.pointerId !== id) return;
      const dy = ev.clientY - y0;
      if (!engaged) {
        if (Math.abs(dy) < ENGAGE) return;
        if (Math.abs(ev.clientX - x0) > Math.abs(dy)) { ac.abort(); return; } // sideways: not ours
        engaged = true;
        try { panel.setPointerCapture(id); } catch {} // the pointer may already be gone
        panel.classList.add("dragging");
        // A keyboard open over a moving sheet is jarring.
        if (panel.contains(document.activeElement)) document.activeElement.blur();
      }
      const finger = start + dy;
      ty = resist(finger, lo, hi);
      samples.push({ t: ev.timeStamp, finger });
      if (samples.length > 16) samples.shift();
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => place(ty));
    }, { signal: ac.signal });
    panel.addEventListener("pointerup", end, { signal: ac.signal });
    panel.addEventListener("pointercancel", end, { signal: ac.signal });
  });

  panel.addEventListener("click", (e) => {
    if (performance.now() < swallowUntil) { swallowUntil = 0; e.preventDefault(); e.stopPropagation(); }
  }, true);

  // A control in the body getting focus (Tab, iOS's next-field arrows) is shown whole.
  body.addEventListener("focusin", () => { if (sheet && at !== "full") settle("full"); });

  // Content that grows or shrinks moves the rests (the sheet stays at the same one); a new size, or a
  // switch between phone and wider layouts, too. Never mid-drag.
  let queued = 0;
  const unset = () => {
    panel.style.height = "";
    panel.style.transform = "";
    delete panel.dataset.snap;
    delete panel.dataset.scrolls;
    body.inert = false;
    document.documentElement.style.removeProperty("--sheet-h");
  };
  const refresh = () => {
    if (queued) return;
    queued = requestAnimationFrame(() => {
      queued = 0;
      const was = sheet;
      sheet = active();
      if (!sheet) {
        if (was) unset();
        return;
      }
      if (panel.classList.contains("dragging")) return;
      settle(was ? at : restFor(), was);
    });
  };
  new MutationObserver(refresh).observe(panel, {
    childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["hidden", "data-mode", "data-layer"],
  });
  new ResizeObserver(refresh).observe(panel.parentElement);

  return {
    /** Moves to a rest ("collapsed", "half" or "full"); nothing wider than a phone. */
    snap: (name) => { if (sheet) settle(name); },
    /** How much of the sheet shows, in px (0 when it isn't a sheet). */
    visible: () => (sheet ? height - y : 0),
  };
}
