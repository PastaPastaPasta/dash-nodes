// Publisher: copies what the crawler's API serves into the Worker's R2, so nodes.dashhq.org never
// talks to the crawler host. Runs next to the app (deploy/docker-compose.yml, service "publisher").
//
//   node scripts/publish.mjs            loop: status every minute, everything else after each crawl
//   node scripts/publish.mjs --once     one full pass, then exit (also: seeding a local `wrangler dev`)
//
// Env: SOURCE (the app, e.g. http://app:8080), TARGET (e.g. https://nodes.dashhq.org),
//      INGEST_TOKEN or INGEST_TOKEN_FILE, NETWORKS (default "mainnet testnet").
// Documents whose content didn't change since the last upload are skipped.
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { SCOPES, DIMS, DAYS, MAX_TOP, shardKey } from "../src/api.js";

const SOURCE = process.env.SOURCE || "http://127.0.0.1:8080";
const TARGET = process.env.TARGET || "http://127.0.0.1:8788";
const TOKEN = process.env.INGEST_TOKEN ||
  (() => { try { return readFileSync(process.env.INGEST_TOKEN_FILE || "/run/secrets/ingest_token", "utf8").trim(); } catch { return ""; } })();
const NETS = (process.env.NETWORKS || "mainnet testnet").split(/\s+/).filter(Boolean);
const once = process.argv.includes("--once");

const log = (...a) => console.log(new Date().toISOString(), ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function get(path) {
  for (let attempt = 0; ; attempt++) {
    const r = await fetch(`${SOURCE}/api/v1/${path}`).catch((e) => ({ ok: false, status: e.message }));
    if (r.ok) return r.json();
    // 404 for an endpoint this build doesn't have; anything else is worth a retry.
    if (r.status === 404 || r.status === 400 || attempt >= 2) throw new Error(`${path}: ${r.status}`);
    await sleep(1000 * (attempt + 1));
  }
}

const sent = new Map(); // key -> sha256 of the last upload
let puts = 0;

async function put(key, doc) {
  const body = JSON.stringify(doc);
  const hash = createHash("sha256").update(body).digest("hex");
  if (sent.get(key) === hash) return;
  for (let attempt = 0; ; attempt++) {
    const r = await fetch(`${TARGET}/_ingest/${key}`, {
      method: "PUT", body: gzipSync(body),
      headers: { authorization: `Bearer ${TOKEN}`, "x-body-encoding": "gzip", "content-type": "application/json" },
    }).catch((e) => ({ status: e.message, text: async () => "" }));
    if (r.status === 204) break;
    if (attempt >= 3) throw new Error(`${key}: ${r.status} ${await r.text()}`);
    await sleep(2000 * (attempt + 1));
  }
  sent.set(key, hash);
  puts++;
}

/** Runs `fn` over `items`, `width` at a time. */
async function each(items, width, fn) {
  for (let i = 0; i < items.length; i += width) await Promise.all(items.slice(i, i + width).map(fn));
}

const paid = new Map(); // `${net}/${protx}` -> payment count last published

async function publishNetwork(net) {
  for (const what of ["summary", "masternodes"]) await put(`v1/${net}/${what}.json`, await get(`${net}/${what}`));
  await get(`${net}/geo`).then((d) => put(`v1/${net}/geo.json`, d), () => {});
  const ranges = async (scope, dim) => Object.fromEntries(await Promise.all(DAYS.map(async (d) =>
    [String(d), await get(`${net}/history?scope=${scope}&dim=${dim}&days=${d}&top=${MAX_TOP}`)])));
  await put(`v1/${net}/history/totals.json`, { ranges: await ranges("all", "totals") });
  const pairs = SCOPES.flatMap((s) => DIMS.filter((d) => d !== "totals").map((d) => [s, d]));
  await each(pairs, 4, async ([scope, dim]) => {
    // Dimensions an older crawler doesn't have yet answer 400; skip them.
    await ranges(scope, dim).then((r) => put(`v1/${net}/history/${scope}/${dim}.json`, { ranges: r }), () => {});
  });
  const { masternodes } = await get(`${net}/masternodes`);
  const shards = new Map();
  await each(masternodes, 24, async ({ protx }) => {
    const detail = await get(`${net}/masternodes/${protx}`);
    const key = shardKey(net, protx);
    if (!shards.has(key)) shards.set(key, {});
    shards.get(key)[protx] = detail;
    // The full payment list only changes when the masternode is paid again.
    const count = detail.payments_summary?.count ?? 0;
    const id = `${net}/${protx}`;
    if (!count || paid.get(id) === count) return;
    const list = [];
    for (let before = null; ;) {
      const { payments } = await get(`${net}/masternodes/${protx}/payments?limit=1000${before != null ? `&before=${before}` : ""}`);
      list.push(...payments);
      if (payments.length < 1000) break;
      before = payments.at(-1).height;
    }
    await put(`v1/${net}/payments/${protx}.json`, list);
    paid.set(id, count);
  });
  for (const [key, doc] of shards) await put(key, doc);
}

async function main() {
  if (!TOKEN) {
    // Not configured yet: stay up quietly rather than crash-loop.
    log("no ingest token (INGEST_TOKEN / INGEST_TOKEN_FILE); not publishing");
    for (;;) await sleep(3_600_000);
  }
  const lastCrawl = new Map();
  let lastFull = 0;
  for (;;) {
    const started = Date.now();
    try {
      const status = await get("status");
      await put("v1/status.json", status);
      for (const net of NETS) {
        const id = status.networks?.[net]?.crawl?.last_id ?? null;
        // A new crawl, or at least every 15 minutes (masternode list and payments move between crawls).
        if (once || id !== lastCrawl.get(net) || started - lastFull > 15 * 60_000) {
          const before = puts;
          await publishNetwork(net);
          lastCrawl.set(net, id);
          log(`${net}: crawl ${id}, ${puts - before} documents uploaded in ${((Date.now() - started) / 1000).toFixed(0)} s`);
        }
      }
      if (started - lastFull > 15 * 60_000) lastFull = started;
    } catch (e) {
      log("publish failed:", e.message);
    }
    if (once) break;
    await sleep(Math.max(5_000, 60_000 - (Date.now() - started)));
  }
}

main();
