// The map's bottom sheet on phones. The sheet keeps one height (the map less a strip at the top)
// whatever it shows, and moves only by transform, so a drag follows the finger without relayout. It rests at three heights: "collapsed" (handle and head), "half" (half the map) and
// "full"; a flick goes on to the next one in its direction. Only at "full" does the body scroll, and
// then natively; everywhere else a vertical drag on the sheet moves the sheet (touch-action: none in
// CSS), never the map or the page behind it.

const ENGAGE = 8; // px of movement before a press becomes a drag (less is a tap)
const FLICK = 0.4; // px/ms: faster than this, release moves on to the next rest in that direction
const TOP_GAP = 56; // px of map kept visible above a full sheet

/**
 * `panel` holds `handle`, `head` and `body` (a column). `active()` says whether it's a sheet right
 * now (phones). `onRest(name, byUser)` runs whenever it comes to rest.
 */
export function bottomSheet({ panel, handle, head, body, active, onRest }) {
  let rests = { collapsed: 0, half: 0, full: 0 };
  let height = 0; // the sheet's height; the part below the screen's edge is out of sight
  let at = "collapsed"; // the rest it's at or heading to
  let y = 0; // current translateY: height - visible
  let suppressClickUntil = 0;

  const visible = () => height - y;
  const place = (ty) => { y = ty; panel.style.transform = `translate3d(0, ${ty}px, 0)`; };
  const order = () => ["collapsed", "half", "full"].filter((n) => rests[n] != null);

  /** Recomputes the rests (visible heights) from the content; keeps the current rest. */
  function measure() {
    const box = panel.parentElement.clientHeight;
    height = Math.max(0, box - TOP_GAP);
    panel.style.height = `${height}px`;
    const pad = parseFloat(getComputedStyle(panel).paddingBottom) || 0;
    const headH = head.offsetParent ? head.offsetHeight : 0; // hidden while showing details
    const top = handle.offsetHeight + headH;
    // The body shrinks to its content (CSS), so scrollHeight is the content's height either way.
    const full = Math.round(Math.min(height, top + body.scrollHeight + pad));
    const collapsed = headH ? Math.min(full, top + pad) : null;
    const half = Math.min(full, Math.max(collapsed ?? 0, Math.round(box * 0.5)));
    rests = { collapsed, half: half < full - 8 ? half : null, full };
    if (rests[at] == null) at = at === "collapsed" ? "half" : "full";
    if (rests[at] == null) at = "full";
  }

  function settle(name, { animate = true, byUser = false } = {}) {
    if (!active()) return;
    measure();
    at = rests[name] != null ? name : at;
    panel.classList.toggle("dragging", !animate);
    place(height - rests[at]);
    panel.dataset.snap = at;
    // Hidden controls below the edge stay out of the tab order and away from screen readers.
    body.inert = at === "collapsed";
    document.documentElement.style.setProperty("--sheet-h", `${rests[at]}px`);
    if (!animate) requestAnimationFrame(() => panel.classList.remove("dragging"));
    onRest?.(at, byUser);
  }

  // A press anywhere on the sheet can become a drag, except in a body that's scrolling (at "full").
  panel.addEventListener("pointerdown", (e) => {
    if (!active() || !e.isPrimary || (e.pointerType === "mouse" && e.button !== 0)) return;
    if (at === "full" && body.contains(e.target) && body.scrollHeight > body.clientHeight + 1) return;
    const id = e.pointerId, y0 = e.clientY, x0 = e.clientX, start = y;
    let engaged = false, frame = 0, samples = [];
    const ac = new AbortController();
    const end = (ev) => {
      ac.abort();
      cancelAnimationFrame(frame);
      if (!engaged) return;
      suppressClickUntil = performance.now() + 400;
      // From the finger, not from what was painted: moves can come faster than frames. Velocity over
      // the last ~100 ms (none if the finger stopped before lifting); positive is downward.
      const last = samples.at(-1);
      place(last.at);
      const recent = samples.filter((s) => ev.timeStamp - s.t < 100);
      const v = ev.type === "pointercancel" || recent.length < 2 ? 0
        : (recent.at(-1).finger - recent[0].finger) / Math.max(1, recent.at(-1).t - recent[0].t);
      const names = order(), cur = visible();
      let pick;
      if (Math.abs(v) > FLICK) {
        // On to the next rest in the flick's direction (or the last one).
        const ahead = v > 0 ? names.filter((n) => rests[n] < cur - 1).reverse() : names.filter((n) => rests[n] > cur + 1);
        pick = ahead[0] || (v > 0 ? names[0] : names.at(-1));
      } else {
        // Where the motion would carry it in another ~150 ms; the rest nearest that.
        const aim = cur - v * 150;
        pick = names.reduce((a, n) => (Math.abs(rests[n] - aim) < Math.abs(rests[a] - aim) ? n : a));
      }
      panel.classList.remove("dragging");
      settle(pick, { byUser: true });
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
        document.activeElement?.blur?.(); // a keyboard open over a moving sheet is jarring
      }
      // Past the highest or lowest rest it gives a little, with resistance.
      const lo = height - rests.full, hi = height - rests[order()[0]];
      const finger = start + dy;
      let ty = finger;
      if (ty < lo) ty = lo - Math.sqrt(lo - ty) * 4;
      if (ty > hi) ty = hi + Math.sqrt(ty - hi) * 4;
      samples.push({ t: ev.timeStamp, finger, at: ty });
      if (samples.length > 16) samples.shift();
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => place(ty));
    }, { signal: ac.signal, passive: true });
    panel.addEventListener("pointerup", end, { signal: ac.signal, passive: true });
    panel.addEventListener("pointercancel", end, { signal: ac.signal, passive: true });
  }, { passive: true });

  // The click that ends a drag (a button under the finger) is not a tap.
  panel.addEventListener("click", (e) => {
    if (performance.now() < suppressClickUntil) { e.preventDefault(); e.stopPropagation(); }
  }, true);

  // Content that grows or shrinks moves the rests; the sheet stays at the same one.
  let queued = 0;
  const refresh = () => {
    if (queued) return;
    queued = requestAnimationFrame(() => { queued = 0; if (active() && !panel.classList.contains("dragging")) settle(at); });
  };
  new MutationObserver(refresh).observe(panel, { childList: true, subtree: true, attributes: true, attributeFilter: ["hidden", "data-mode", "data-layer"], characterData: true });
  new ResizeObserver(refresh).observe(panel.parentElement);

  // Wider than a phone: the floating panel, without any of this.
  const reset = () => {
    if (active()) { settle(at, { animate: false }); return; }
    panel.style.height = panel.style.transform = "";
    delete panel.dataset.snap;
    body.inert = false;
    document.documentElement.style.removeProperty("--sheet-h");
  };
  addEventListener("resize", reset);
  reset();

  return {
    /** Moves to a rest ("collapsed", "half" or "full"). */
    snap: (name) => settle(name),
    /** How much of the sheet shows, in px (0 when it isn't a sheet). */
    visible: () => (active() ? visible() : 0),
    get rest() { return at; },
  };
}
