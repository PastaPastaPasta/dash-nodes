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
      // Antarctica has no nodes and would take a fifth of the height.
      for (const p of polys) if (p[0].some(([, lat]) => lat > -58)) d += ringPath(p[0]);
    }
    return d;
  });
  return land;
}

/**
 * Draws the map into `root` (an <svg>). points: [{lat, lon, nodes}]. Dots fade in west to east once,
 * like a crawl sweeping the globe; skipped when the reader prefers reduced motion.
 */
export async function worldMap(root, points) {
  // The whole land outline: Greenland and northern Scandinavia reach the top of the projection.
  root.setAttribute("viewBox", "0 0 1000 440");
  root.setAttribute("preserveAspectRatio", "xMidYMid meet");
  root.replaceChildren(svg("path", { class: "wm-land", d: await landPath() }));
  const dots = svg("g", { class: "wm-dots" });
  const max = Math.max(1, ...points.map((p) => p.nodes));
  for (const p of [...points].sort((a, b) => b.nodes - a.nodes)) {
    const [x, y] = project(p.lon, p.lat);
    const r = 1.6 + Math.sqrt(p.nodes / max) * 13;
    const c = svg("circle", { cx: x.toFixed(1), cy: y.toFixed(1), r: r.toFixed(1) });
    c.style.setProperty("--d", `${Math.round((x / 1000) * 900)}ms`);
    dots.append(c);
  }
  root.append(dots);
}
