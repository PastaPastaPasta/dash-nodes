// Hand-rolled SVG charts: stacked area / line time series with a crosshair tooltip, and bar lists.
// Marks follow the dataviz reference: 2px lines, hairline solid grid, 2px surface gaps, colors by role.
import { el, compact, fmt } from "./common.js";

const SVG = "http://www.w3.org/2000/svg";
const SLOTS = 6;

function svg(tag, attrs = {}) {
  const e = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) if (v != null) e.setAttribute(k, v);
  return e;
}

/** Color follows the entity: a key keeps its slot for the life of the palette, survivors never repaint. */
export class Palette {
  constructor() { this.map = new Map(); }
  assign(keys) {
    const live = new Set(keys.filter((k) => k !== "Other" && k !== "Unknown"));
    for (const k of [...this.map.keys()]) if (!live.has(k)) this.map.delete(k);
    const used = new Set(this.map.values());
    for (const k of live) {
      if (this.map.has(k)) continue;
      for (let s = 1; s <= SLOTS; s++) if (!used.has(s)) { this.map.set(k, s); used.add(s); break; }
    }
  }
  color(key) {
    if (key === "Other") return "var(--other)";
    if (key === "Unknown") return "var(--axis)";
    const s = this.map.get(key);
    return s ? `var(--s${s})` : "var(--other)";
  }
}

/** Round gridline step for about four gridlines; whole numbers only (these are counts). */
function niceStep(max) {
  const raw = Math.max(1, max / 4);
  const p = 10 ** Math.floor(Math.log10(raw));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= raw && Number.isInteger(m * p)) return m * p;
  return 10 * p;
}

function timeTicks(t0, t1, n) {
  const span = t1 - t0;
  const steps = [3600, 3 * 3600, 6 * 3600, 12 * 3600, 86400, 2 * 86400, 7 * 86400, 14 * 86400, 30 * 86400, 91 * 86400, 182 * 86400, 365 * 86400];
  const step = steps.find((s) => span / s <= n) || steps.at(-1);
  const first = Math.ceil(t0 / step) * step;
  const out = [];
  for (let t = first; t <= t1; t += step) out.push(t);
  return { ticks: out, step };
}

function fmtTick(t, step) {
  const d = new Date(t * 1000);
  if (step < 86400) return d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", timeZone: "UTC" });
  if (step >= 182 * 86400) return d.toLocaleDateString("en-GB", { month: "short", year: "numeric", timeZone: "UTC" });
  return d.toLocaleDateString("en-GB", { day: "numeric", month: "short", timeZone: "UTC" });
}

function fmtWhen(t, resolution) {
  const d = new Date(t * 1000);
  return resolution === "day"
    ? d.toLocaleDateString("en-GB", { dateStyle: "medium", timeZone: "UTC" }) + " (daily avg)"
    : d.toLocaleString("en-GB", { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" }) + " UTC";
}

/**
 * Time-series chart.
 * data: {t: [unix...], series: [{key, values}], resolution}
 * opts: {stacked, share, palette, label(key), height, valueFmt}
 * Returns {update(data, opts)}.
 */
export function timeChart(container, data, opts) {
  container.classList.add("chart");
  const legend = el("div", { class: "legend" });
  const host = el("div", { style: { position: "relative" } });
  container.replaceChildren(host, legend);
  const tip = el("div", { class: "tooltip", hidden: true });
  host.append(tip);
  let state = { data, opts };
  let width = container.clientWidth || 600;

  function draw() {
    const { data, opts } = state;
    const label = opts.label || ((k) => k);
    const H = opts.height || 240, X = 22, M = { l: 44, r: 10, t: 8 };
    const W = Math.max(280, width);
    const pw = W - M.l - M.r, ph = H - M.t;
    host.querySelector("svg")?.remove();
    const root = svg("svg", { viewBox: `0 0 ${W} ${H + X}`, height: H + X, role: "img", "aria-label": opts.title || "chart" });
    host.prepend(root);
    const t = data.t;
    if (t.length < 2) {
      const msg = svg("text", { x: W / 2, y: H / 2, "text-anchor": "middle", class: "tick" });
      msg.textContent = t.length ? "History builds up as crawls run; check back after the next one." : "No data for this range yet.";
      root.append(msg);
      legend.replaceChildren();
      return;
    }
    const series = data.series.map((s) => ({ ...s, values: s.values.slice() }));
    if (opts.share) {
      for (let i = 0; i < t.length; i++) {
        const tot = series.reduce((a, s) => a + s.values[i], 0) || 1;
        for (const s of series) s.values[i] = (s.values[i] / tot) * 100;
      }
    }
    opts.palette.assign(series.map((s) => s.key));
    // Stacks: lower/upper bounds per series per point.
    const lo = [], hi = [];
    let max = 0;
    for (let i = 0; i < t.length; i++) {
      let acc = 0;
      series.forEach((s, j) => {
        (lo[j] ||= [])[i] = opts.stacked ? acc : 0;
        acc = opts.stacked ? acc + s.values[i] : s.values[i];
        (hi[j] ||= [])[i] = acc;
        max = Math.max(max, acc);
      });
    }
    const yStep = opts.share ? 25 : niceStep(max);
    const yMax = opts.share ? 100 : yStep * Math.max(1, Math.ceil((max * 1.02) / yStep));
    const t0 = t[0], t1 = t.length > 1 ? t.at(-1) : t[0] + 1;
    const x = (v) => M.l + ((v - t0) / (t1 - t0)) * pw;
    const y = (v) => M.t + ph - (v / yMax) * ph;

    for (let v = 0; v <= yMax + 1e-9; v += yStep) {
      const yy = Math.round(y(v)) + 0.5;
      root.append(svg("line", { x1: M.l, x2: W - M.r, y1: yy, y2: yy, class: v === 0 ? "baseline" : "gridline" }));
      const tx = svg("text", { x: M.l - 8, y: yy + 4, "text-anchor": "end", class: "tick" });
      tx.textContent = opts.share ? `${v}%` : compact(v);
      root.append(tx);
    }
    const { ticks, step } = timeTicks(t0, t1, Math.max(2, Math.floor(pw / 110)));
    for (const tv of ticks) {
      const tx = svg("text", { x: x(tv), y: H + 16, "text-anchor": "middle", class: "tick" });
      tx.textContent = fmtTick(tv, step);
      root.append(tx);
    }

    const pts = (arr) => arr.map((v, i) => `${x(t[i]).toFixed(1)},${y(v).toFixed(1)}`);
    series.forEach((s, j) => {
      const color = opts.palette.color(s.key);
      if (opts.stacked) {
        const d = `M${pts(hi[j]).join("L")}L${pts(lo[j]).reverse().join("L")}Z`;
        root.append(svg("path", { d, fill: color, "fill-opacity": 0.82 }));
        // 2px surface gap between stacked bands.
        root.append(svg("path", { d: `M${pts(hi[j]).join("L")}`, fill: "none", stroke: "var(--surface)", "stroke-width": 2, "stroke-linejoin": "round" }));
      } else {
        root.append(svg("path", { d: `M${pts(hi[j]).join("L")}`, fill: "none", stroke: color, "stroke-width": 2, "stroke-linejoin": "round", "stroke-linecap": "round" }));
        const last = t.length - 1;
        root.append(svg("circle", { cx: x(t[last]), cy: y(hi[j][last]), r: 4, fill: color, stroke: "var(--surface)", "stroke-width": 2 }));
      }
    });

    const xhair = svg("line", { y1: M.t, y2: M.t + ph, class: "xhair", visibility: "hidden" });
    root.append(xhair);
    const hit = svg("rect", { x: M.l, y: 0, width: pw, height: H, fill: "transparent", tabindex: 0 });
    root.append(hit);
    const valueFmt = opts.valueFmt || ((v) => (opts.share ? `${v.toFixed(1)}%` : fmt(v)));
    const show = (i) => {
      const xx = x(t[i]);
      xhair.setAttribute("x1", xx); xhair.setAttribute("x2", xx); xhair.setAttribute("visibility", "visible");
      const rows = series.map((s) => el("div", { class: "tt-row" },
        el("i", { style: { background: opts.palette.color(s.key) } }), el("b", { text: valueFmt(s.values[i]) }), el("span", { text: label(s.key) })));
      if (opts.stacked) rows.reverse();
      if (opts.stacked && !opts.share) {
        rows.push(el("div", { class: "tt-row" }, el("i", { style: { background: "transparent" } }),
          el("b", { text: fmt(series.reduce((a, s) => a + s.values[i], 0)) }), el("span", { text: "Total" })));
      }
      tip.replaceChildren(el("div", { class: "tt-head", text: fmtWhen(t[i], data.resolution) }), ...rows);
      tip.hidden = false;
      const left = xx + 14 + tip.offsetWidth > W ? xx - 14 - tip.offsetWidth : xx + 14;
      tip.style.left = `${(left / W) * 100}%`;
      tip.style.top = "6px";
    };
    const nearest = (px) => {
      let best = 0;
      for (let i = 1; i < t.length; i++) if (Math.abs(x(t[i]) - px) < Math.abs(x(t[best]) - px)) best = i;
      return best;
    };
    let focusIdx = t.length - 1;
    hit.addEventListener("pointermove", (ev) => {
      const r = root.getBoundingClientRect();
      focusIdx = nearest(((ev.clientX - r.left) / r.width) * W);
      show(focusIdx);
    });
    const hide = () => { tip.hidden = true; xhair.setAttribute("visibility", "hidden"); };
    hit.addEventListener("pointerleave", hide);
    hit.addEventListener("blur", hide);
    hit.addEventListener("focus", () => show(focusIdx));
    hit.addEventListener("keydown", (ev) => {
      if (ev.key === "ArrowLeft") focusIdx = Math.max(0, focusIdx - 1);
      else if (ev.key === "ArrowRight") focusIdx = Math.min(t.length - 1, focusIdx + 1);
      else return;
      ev.preventDefault();
      show(focusIdx);
    });

    const keys = opts.stacked ? series.slice().reverse() : series;
    legend.replaceChildren(...keys.map((s) => el("span", {},
      el("i", { class: opts.stacked ? null : "line", style: { background: opts.palette.color(s.key) } }), label(s.key))));
  }

  new ResizeObserver((entries) => {
    const w = Math.round(entries[0].contentRect.width);
    if (w && Math.abs(w - width) > 4) { width = w; draw(); }
  }).observe(container);
  window.addEventListener("themechange", draw);
  draw();
  return {
    update(data, opts) { state = { data, opts: { ...state.opts, ...opts } }; container.classList.remove("loading"); draw(); },
    loading() { container.classList.add("loading"); },
  };
}

/** Ranked horizontal bars. rows: [[key, count], ...] sorted desc. Single series: one color. */
export function barList(container, rows, { label = (k) => k, limit = 8, total } = {}) {
  const sum = total ?? rows.reduce((a, r) => a + r[1], 0);
  let expanded = false;
  const render = () => {
    const shown = expanded ? rows : rows.slice(0, limit);
    const max = Math.max(1, ...shown.map((r) => r[1]));
    const grid = el("div", { class: "bars" });
    for (const [k, v] of shown) {
      const name = label(k);
      grid.append(
        el("div", { class: "k", title: typeof name === "string" ? name : k }, name),
        el("div", { class: "track" }, el("div", { class: `fill${k === "Unknown" ? " other" : ""}`, style: { width: `${(v / max) * 100}%` } })),
        el("div", { class: "v" }, fmt(v), el("small", { text: sum ? `${((v / sum) * 100).toFixed(1)}%` : "" })),
      );
    }
    const parts = [rows.length ? grid : el("div", { class: "empty", text: "No data yet" })];
    if (rows.length > limit) {
      parts.push(el("button", { class: "more", type: "button", onclick: () => { expanded = !expanded; render(); },
        text: expanded ? "Show fewer" : `Show all ${rows.length}` }));
    }
    container.replaceChildren(...parts);
  };
  render();
}
