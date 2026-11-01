// A small static world map in the Equal Earth projection: land from the self-hosted Natural Earth file,
// one dot per city with reachable nodes, sized by how many. Used as the overview's hero.

import { svg } from "./charts.js";

const A1 = 1.340264, A2 = -0.081106, A3 = 0.000893, A4 = 0.003796, M = Math.sqrt(3) / 2;
const X_MAX = 2.7064, Y_MAX = 1.3173; // projected extent of the whole globe

/** Equal Earth (Šavrič, Patterson, Jenny 2018), scaled to a 1000 × 487 view box. */
function project(lon, lat) {
  const l = (lon * Math.PI) / 180;
  const t = Math.asin(M * Math.sin((lat * Math.PI) / 180));
  const t2 = t * t, t6 = t2 * t2 * t2;
  const x = (l * Math.cos(t)) / (M * (A1 + 3 * A2 * t2 + t6 * (7 * A3 + 9 * A4 * t2)));
  const y = t * (A1 + A2 * t2 + t6 * (A3 + A4 * t2));
  return [((x + X_MAX) / (2 * X_MAX)) * 1000, ((Y_MAX - y) / (2 * Y_MAX)) * 487];
}

function ringPath(ring) {
  let d = "";
  for (let i = 0; i < ring.length; i++) {
    const [x, y] = project(ring[i][0], ring[i][1]);
    d += `${i ? "L" : "M"}${x.toFixed(1)} ${y.toFixed(1)}`;
  }
  return `${d}Z`;
}

let land = null;
async function landPath() {
  land ??= fetch("/assets/map/world-low.json").then((r) => r.json()).then((fc) => {
    let d = "";
    for (const f of fc.features) {
      if (f.properties.k !== "land") continue;
      const polys = f.geometry.type === "Polygon" ? [f.geometry.coordinates] : f.geometry.coordinates;
      // Antarctica has no nodes and would take a fifth of the height. The islands in the Pacific west of
      // the Americas (Hawaii, and Fiji's sliver across the date line) would float alone under the text.
      for (const p of polys) {
        if (p[0].some(([, lat]) => lat > -58) && !p[0].every(([lon, lat]) => lon < -150 && lat < 30)) d += ringPath(p[0]);
      }
    }
    return d;
  });
  return land;
}

const PINGS = 5; // radar pings, from the biggest clusters at least PING_GAP viewBox units apart
const PING_GAP = 70;

/**
 * Radar pings into `box` (the .wm-pings overlay): a ring per chosen cluster, taking turns for half a
 * minute. The rings are CSS animations; script only places them once, and pauses them while the map
 * is off screen.
 */
function radarPings(box, dots) {
  const picked = [];
  for (const d of dots) {
    if (picked.length === PINGS) break;
    if (picked.every((p) => Math.hypot(p.x - d.x, p.y - d.y) >= PING_GAP)) picked.push(d);
  }
  const frame = document.createElement("div");
  picked.forEach((d, i) => {
    const ring = document.createElement("span");
    ring.className = "wm-ping";
    ring.style.cssText = `--x:${d.x / 10}%;--y:${d.y / 4.4}%;--r:${(d.r + 22).toFixed(1)};--d:${1200 + i * 1000}ms`;
    frame.append(ring);
  });
  box.replaceChildren(frame);
  new IntersectionObserver((es) => box.classList.toggle("paused", !es.at(-1).isIntersecting)).observe(box);
}

/**
 * Cities as bubbles, biggest first, merged wherever two would overlap: a merge sits at the node-weighted
 * centre and is sized by the total, so a dense region reads as one bubble rather than a pile. Repeats
 * until nothing overlaps, since a merge grows its bubble.
 */
function bubbles(points) {
  const radius = (n, max) => 1.6 + Math.sqrt(n / max) * 13;
  let cs = points.map((p) => { const [x, y] = project(p.lon, p.lat); return { x, y, nodes: p.nodes }; });
  for (let pass = 0; pass < 8; pass++) {
    const max = Math.max(1, ...cs.map((c) => c.nodes));
    const out = [];
    for (const c of cs.sort((a, b) => b.nodes - a.nodes)) {
      const r = radius(c.nodes, max);
      const into = out.find((o) => Math.hypot(o.x - c.x, o.y - c.y) < o.r + r);
      if (!into) { out.push({ ...c, r }); continue; }
      const n = into.nodes + c.nodes;
      Object.assign(into, { x: (into.x * into.nodes + c.x * c.nodes) / n, y: (into.y * into.nodes + c.y * c.nodes) / n, nodes: n, r: radius(n, max) });
    }
    const done = out.length === cs.length;
    cs = out;
    if (done) break;
  }
  const max = Math.max(1, ...cs.map((c) => c.nodes));
  return cs.sort((a, b) => b.nodes - a.nodes).map((c) => ({ ...c, r: radius(c.nodes, max) }));
}

/**
 * Draws the map into `root` (an <svg>). points: [{lat, lon, nodes}]. Dots fade in west to east once,
 * like a crawl sweeping the globe; skipped when the reader prefers reduced motion. Then, if `pings`
 * (the overlay over `root`) is given, the biggest clusters send out radar pings (also off then, in CSS).
 */
export async function worldMap(root, points, pings) {
  // The whole land outline: Greenland and northern Scandinavia reach the top of the projection.
  root.setAttribute("viewBox", "0 0 1000 440");
  root.setAttribute("preserveAspectRatio", "xMidYMid meet");
  root.replaceChildren(svg("path", { class: "wm-land", d: await landPath() }));
  const dots = svg("g", { class: "wm-dots" });
  const placed = bubbles(points);
  for (const { x, y, r } of placed) {
    const c = svg("circle", { cx: x.toFixed(1), cy: y.toFixed(1), r: r.toFixed(1) });
    c.style.setProperty("--d", `${Math.round((x / 1000) * 900)}ms`);
    dots.append(c);
  }
  root.append(dots);
  if (pings) radarPings(pings, placed);
}
