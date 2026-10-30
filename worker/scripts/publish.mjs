// Publisher: copies what the crawler's API serves into the Worker's R2, so nodes.dashhq.org never
// talks to the crawler host. Runs next to the app (deploy/docker-compose.yml, service "publisher").
//
//   node scripts/publish.mjs            loop: status every minute, a network's documents after each of its
//                                       crawls (and hourly), retrying a failed network with backoff
//   node scripts/publish.mjs --once     one full pass, then exit (also: seeding a local `wrangler dev`)
//
// Env: SOURCE (the app, e.g. http://app:8080), TARGET (e.g. https://nodes.dashhq.org),
//      INGEST_TOKEN or INGEST_TOKEN_FILE, NETWORKS (default "mainnet testnet"),
//      STATE_FILE (optional: remembers what was uploaded across restarts).
// Documents whose content didn't change since the last upload are skipped. Within a network, documents
// go up in dependency order (payments, masternode details, then the list and summaries that link to them).
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { SCOPES, DIMS, DAYS, MAX_TOP, SHARDS, PAYMENT_BUCKET, shardKey } from "../src/api.js";

const SOURCE = process.env.SOURCE || "http://127.0.0.1:8080";
const TARGET = process.env.TARGET || "http://127.0.0.1:8788";
const NETS = (process.env.NETWORKS || "mainnet testnet").split(/\s+/).filter(Boolean);
const STATE_FILE = process.env.STATE_FILE || "";
/** A network is republished at least this often even without a new crawl (list and payments move). */
const REFRESH_EVERY = 60 * 60_000;
const once = process.argv.includes("--once");

function readToken() {
  if (process.env.INGEST_TOKEN) return process.env.INGEST_TOKEN;
  const file = process.env.INGEST_TOKEN_FILE || "/run/secrets/ingest_token";
  try {
    return readFileSync(file, "utf8").trim();
  } catch (e) {
    if (e.code !== "ENOENT") log(`can't read ${file}: ${e.code}`);
    return "";
  }
}
let TOKEN = "";

const log = (...a) => console.log(new Date().toISOString(), ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** fetch, retried up to `tries` times with a growing pause unless `permanent(status)`; returns the last response. */
async function fetchRetry(url, init, tries, pause, permanent = () => false) {
  for (let n = 1; ; n++) {
    const r = await fetch(url, init).catch((e) => ({ ok: false, status: e.message, text: async () => "" }));
    if (r.ok || n >= tries || permanent(r.status)) return r;
    await sleep(pause * n);
  }
}

async function get(path) {
  // 404 for an endpoint this build doesn't have, 400 for a parameter it doesn't know; anything else is worth a retry.
  const r = await fetchRetry(`${SOURCE}/api/v1/${path}`, undefined, 3, 1000, (status) => status === 404 || status === 400);
  if (!r.ok) throw Object.assign(new Error(`${path}: ${r.status}`), { status: r.status });
  return r.json();
}

// What's already in R2, so unchanged documents aren't uploaded again (kept in STATE_FILE across restarts).
let sentHash = new Map(); // key -> sha256 of the last upload
let paymentCounts = new Map(); // `${net}/${protx}` -> payment count last published
let known = new Map(); // net -> protx hashes ever published, so removed masternodes keep their page
let puts = 0;

function loadState() {
  if (!STATE_FILE) return;
  try {
    const st = JSON.parse(readFileSync(STATE_FILE, "utf8"));
    sentHash = new Map(Object.entries(st.sentHash || {}));
    paymentCounts = new Map(Object.entries(st.paymentCounts || {}));
    known = new Map(Object.entries(st.known || {}).map(([net, list]) => [net, new Set(list)]));
    log(`state: ${sentHash.size} documents already published`);
  } catch (e) {
    if (e.code !== "ENOENT") log(`state file unreadable (${e.message}); republishing everything`);
  }
}

function saveState() {
  if (!STATE_FILE) return;
  const st = { sentHash: Object.fromEntries(sentHash), paymentCounts: Object.fromEntries(paymentCounts),
    known: Object.fromEntries([...known].map(([net, set]) => [net, [...set]])) };
  writeFileSync(`${STATE_FILE}.tmp`, JSON.stringify(st));
  renameSync(`${STATE_FILE}.tmp`, STATE_FILE);
}

async function put(key, doc) {
  const body = JSON.stringify(doc);
  const hash = createHash("sha256").update(body).digest("hex");
  if (sentHash.get(key) === hash) return;
  const r = await fetchRetry(`${TARGET}/_ingest/${key}`, {
    method: "PUT", body: gzipSync(body),
    headers: { authorization: `Bearer ${TOKEN}`, "x-body-encoding": "gzip", "content-type": "application/json" },
  }, 4, 2000);
  if (!r.ok) throw new Error(`${key}: ${r.status} ${await r.text()}`);
  sentHash.set(key, hash);
  puts++;
}

/** Uploads a document an older crawler may not have yet (its endpoint answers 400/404); skips it if so. */
async function putIfServed(key, load) {
  // Only "this build doesn't have it" is skipped; any other failure fails the pass, which is retried.
  const doc = await load().catch((e) => { if (e.status === 404 || e.status === 400) return undefined; throw e; });
  if (doc !== undefined) await put(key, doc);
}

/** Runs `fn` over `items`, `width` at a time. */
async function inBatches(items, width, fn) {
  for (let i = 0; i < items.length; i += width) await Promise.all(items.slice(i, i + width).map(fn));
}

/** A history document: every precomputed range of one series set. */
async function historyDoc(net, scope, dim) {
  const ranges = await Promise.all(DAYS.map(async (d) =>
    [String(d), await get(`${net}/history?scope=${scope}&dim=${dim}&days=${d}&top=${MAX_TOP}`)]));
  return { ranges: Object.fromEntries(ranges) };
}

/** Every payment of one masternode, newest first, paged by height. Only the newest bucket usually changes. */
async function allPayments(net, protx) {
  const list = [];
  for (let before = null; ;) {
    const { payments } = await get(`${net}/masternodes/${protx}/payments?limit=1000${before != null ? `&before=${before}` : ""}`);
    list.push(...payments);
    if (payments.length < 1000) return list;
    before = payments.at(-1).height;
  }
}

async function publishPayments(net, protx) {
  const buckets = new Map();
  for (const p of await allPayments(net, protx)) {
    const b = Math.floor(p.height / PAYMENT_BUCKET);
    if (!buckets.has(b)) buckets.set(b, []);
    buckets.get(b).push(p);
  }
  for (const [b, list] of buckets) await put(`v1/${net}/payments/${protx}/${b}.json`, list);
  await put(`v1/${net}/payments/${protx}.json`, { buckets: [...buckets.keys()].sort((a, b) => b - a) });
}

async function publishNetwork(net) {
  const list = await get(`${net}/masternodes`);
  // Removed masternodes drop off the list but keep their page, so their details stay published.
  const seen = known.get(net) || new Set();
  // Masternodes named in the changes feed keep their page too, even across a publisher restart.
  const changes = await get(`${net}/changes`).catch((e) => { if (e.status === 404 || e.status === 400) return null; throw e; });
  const ids = [...new Set([...list.masternodes.map((m) => m.protx), ...seen, ...(changes?.changes || []).map((c) => c.protx)])];
  // Every shard goes up, empty ones too, so an unknown ProTx is a 404 rather than "not published".
  const shards = new Map(Array.from({ length: SHARDS }, (_, i) =>
    [shardKey(net, ((i * 256) / SHARDS).toString(16).padStart(2, "0")), {}]));
  await inBatches(ids, 24, async (protx) => {
    const detail = await get(`${net}/masternodes/${protx}`).catch(() => null);
    if (!detail) return;
    shards.get(shardKey(net, protx))[protx] = detail;
    // The payment list only changes when the masternode is paid again.
    const count = detail.payments_summary?.count ?? 0;
    const id = `${net}/${protx}`;
    if (!count || paymentCounts.get(id) === count) return;
    await publishPayments(net, protx);
    paymentCounts.set(id, count);
  });
  for (const [key, doc] of shards) await put(key, doc);
  known.set(net, new Set(ids));
  await put(`v1/${net}/masternodes.json`, list);
  await put(`v1/${net}/summary.json`, await get(`${net}/summary`));
  await putIfServed(`v1/${net}/geo.json`, () => get(`${net}/geo`));
  // The latest registration changes (see the crawler's collector.rs).
  if (changes) await put(`v1/${net}/changes.json`, changes);
  // Block hashes speed-test vantages check downloaded headers against.
  await putIfServed(`v1/${net}/reference.json`, () => get(`${net}/reference`));
  await put(`v1/${net}/history/totals.json`, await historyDoc(net, "all", "totals"));
  const pairs = SCOPES.flatMap((s) => DIMS.filter((d) => d !== "totals").map((d) => [s, d]));
  await inBatches(pairs, 4, ([scope, dim]) =>
    putIfServed(`v1/${net}/history/${scope}/${dim}.json`, () => historyDoc(net, scope, dim)));
}

async function main() {
  TOKEN = readToken();
  if (!TOKEN && once) { log("no ingest token (INGEST_TOKEN / INGEST_TOKEN_FILE)"); process.exit(1); }
  // Not configured yet: stay up quietly rather than crash-loop, and start once the token appears.
  if (!TOKEN) log("no ingest token yet (INGEST_TOKEN / INGEST_TOKEN_FILE); waiting for one");
  while (!TOKEN) { await sleep(60_000); TOKEN = readToken(); }
  loadState();
  const lastCrawl = new Map(); // net -> crawl id last published
  const lastPass = new Map(); // net -> time of the last successful pass
  const failures = new Map(); // net -> consecutive failed passes
  const retryAt = new Map(); // net -> earliest next attempt after a failure
  for (;;) {
    const started = Date.now();
    let status = null;
    try {
      status = await get("status");
      await put("v1/status.json", status);
    } catch (e) {
      log("status failed:", e.message);
    }
    for (const net of NETS) {
      const id = status?.networks?.[net]?.crawl?.last_id ?? null;
      const due = once || id !== lastCrawl.get(net) || started - (lastPass.get(net) || 0) > REFRESH_EVERY;
      if (!status || !due || started < (retryAt.get(net) || 0)) continue;
      const before = puts;
      try {
        await publishNetwork(net);
        lastCrawl.set(net, id);
        lastPass.set(net, started);
        failures.delete(net);
        log(`${net}: crawl ${id}, ${puts - before} documents uploaded in ${((Date.now() - started) / 1000).toFixed(0)} s`);
      } catch (e) {
        // Back off 1, 2, 4 … 15 minutes; what did go up is remembered, so a retry only sends the rest.
        const n = (failures.get(net) || 0) + 1;
        failures.set(net, n);
        retryAt.set(net, Date.now() + Math.min(15, 2 ** (n - 1)) * 60_000);
        log(`${net}: publish failed (${n} in a row, ${puts - before} documents went up):`, e.message);
      }
      saveState();
    }
    if (once) break;
    await sleep(Math.max(5_000, 60_000 - (Date.now() - started)));
  }
}

main();
