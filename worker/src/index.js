// DashHQ Radar (radar.dashhq.org, and nodes.dashhq.org as before): the static site (Workers static assets), the public API answered from R2, and an
// authenticated ingest speed-test vantages push their results to (the crawler's publisher writes to R2
// directly, see scripts/publish.mjs). The crawler's host takes no inbound traffic.
import { connect } from "cloudflare:sockets";
import { route, ApiError, KEY, NETWORKS, DOCS, PARAMS, VANTAGE, shardKey } from "./api.js";
import { checkNode, parseAddress } from "./check.js";

const API_HEADERS = {
  "content-type": "application/json; charset=utf-8",
  "access-control-allow-origin": "*",
  "x-content-type-options": "nosniff",
};
/** Largest accepted document, decompressed. The biggest today (mainnet masternode list) is about 2 MB. */
const MAX_INGEST = 24 << 20;

const json = (status, body, cache) =>
  new Response(JSON.stringify(body), { status, headers: { ...API_HEADERS, "cache-control": cache } });
const fail = (status, message) => json(status, { error: message }, "no-store");

/** Paths whose stored document is served as is (`releases`: Dash Core's, from the publisher). */
const PASSTHROUGH = new RegExp(`^(status|releases|(${NETWORKS.join("|")})/(${DOCS.join("|")}))$`);

const getJson = async (env, key) => {
  const obj = await env.DATA.get(key);
  return obj ? obj.json() : null;
};

/**
 * The request as the cache (and the router) should see it: path plus only the parameters that endpoint
 * reads, in a fixed order, so cache-busting or junk parameters can't force fresh R2 reads and parses.
 */
function canonical(url, path) {
  const u = new URL(`/api/v1/${path}`, url.origin);
  const kind = path.endsWith("/payments") ? "payments" : path.endsWith("/history") ? "history" : null;
  for (const k of PARAMS[kind] || []) if (url.searchParams.has(k)) u.searchParams.set(k, url.searchParams.get(k));
  return u;
}

async function answer(env, path, query) {
  // The big documents (the masternode list is ~2 MB) go out as stored: parsing them would cost more CPU
  // than a request gets.
  if (PASSTHROUGH.test(path)) {
    const obj = await env.DATA.get(`v1/${path}.json`);
    return obj
      ? new Response(obj.body, { headers: { ...API_HEADERS, "cache-control": "public, max-age=60", etag: obj.httpEtag } })
      : fail(503, "not published yet");
  }
  try {
    const vantages = (env.BENCH_VANTAGES || "home").split(",").map((v) => v.trim()).filter((v) => new RegExp(`^${VANTAGE}$`).test(v));
    const body = JSON.stringify(await route(path.split("/"), query, (key) => getJson(env, key), vantages));
    const etag = `"${hex(await digest(body)).slice(0, 32)}"`;
    return new Response(body, { headers: { ...API_HEADERS, "cache-control": "public, max-age=60", etag } });
  } catch (e) {
    if (e instanceof ApiError) return fail(e.status, e.message);
    console.error(e);
    return fail(500, "internal error");
  }
}

async function api(request, env, ctx, url) {
  if (request.method !== "GET" && request.method !== "HEAD") return fail(405, "method not allowed");
  const path = url.pathname.slice("/api/v1/".length).replace(/\/$/, "");
  const key = canonical(url, path);
  const cache = caches.default;
  let response = await cache.match(key.toString());
  if (!response) {
    response = await answer(env, path, key.searchParams);
    if (response.ok) ctx.waitUntil(cache.put(key.toString(), response.clone()));
  }
  return forBrowser(request, response);
}

const bareTag = (t) => t.trim().replace(/^W\//, "");

/**
 * The edge copy (kept here for 60 s) as browsers get it: `no-cache`, so they revalidate on every load
 * and a reload always shows the latest crawl, with a 304 when nothing changed. A max-age would be
 * raised to hours by the zone's Browser Cache TTL, and normal reloads then reused hours-old data.
 */
function forBrowser(request, response) {
  if (!response.ok) return request.method === "HEAD" ? new Response(null, response) : response;
  const headers = new Headers(response.headers);
  headers.set("cache-control", "no-cache");
  const etag = headers.get("etag");
  const asked = request.headers.get("if-none-match");
  if (etag && asked && asked.split(",").some((t) => bareTag(t) === bareTag(etag))) {
    return new Response(null, { status: 304, headers });
  }
  return new Response(request.method === "HEAD" ? null : response.body, { status: response.status, headers });
}

/** Reads a body, failing once it passes `limit` bytes instead of buffering whatever arrives. */
async function readLimited(stream, limit) {
  const chunks = [];
  let size = 0;
  for await (const chunk of stream) {
    size += chunk.byteLength;
    if (size > limit) return null;
    chunks.push(chunk);
  }
  const out = new Uint8Array(size);
  let at = 0;
  for (const c of chunks) { out.set(c, at); at += c.byteLength; }
  return out;
}

const digest = async (text) => new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)));
const hex = (bytes) => [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");

/**
 * Who may write `key`: the crawler's INGEST_TOKEN anything, a speed-test vantage only its own results.
 * BENCH_TOKENS (a secret) maps vantage ids to the SHA-256 hex of their tokens, e.g. {"nyc": "9f…"}.
 * Comparisons are on digests, in constant time.
 */
async function authorized(request, env, key) {
  return authorizedFor(request, env, /^v1\/[a-z]+\/bench\/([^/]+)\//.exec(key)?.[1]);
}

/** The INGEST_TOKEN, `extra` (another secret's value) if given, or the token of `vantage` (if given). */
async function authorizedFor(request, env, vantage, extra) {
  const got = (request.headers.get("authorization") || "").replace(/^Bearer /, "");
  if (!got) return false;
  const mine = await digest(got);
  for (const t of [env.INGEST_TOKEN, extra]) if (t && crypto.subtle.timingSafeEqual(mine, await digest(t))) return true;
  let tokens;
  try { tokens = JSON.parse(env.BENCH_TOKENS || "{}"); } catch { return false; }
  if (!tokens || typeof tokens !== "object" || !vantage || !Object.hasOwn(tokens, vantage)) return false;
  const want = String(tokens[vantage]).toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(want)) return false;
  const a = new TextEncoder().encode(hex(mine)), b = new TextEncoder().encode(want);
  return crypto.subtle.timingSafeEqual(a, b);
}

/** PUT /_ingest/<key>: the crawler publishing one document, gzip-compressed (X-Body-Encoding: gzip). */
async function ingest(request, env, url) {
  if (request.method !== "PUT") return fail(405, "method not allowed");
  const key = url.pathname.slice("/_ingest/".length);
  if (!(await authorized(request, env, key))) return fail(401, "unauthorized");
  if (!KEY.test(key)) return fail(400, "key not allowed");
  if (!request.body || Number(request.headers.get("content-length") || 0) > MAX_INGEST) return fail(413, "too large");
  let body = request.body;
  if (request.headers.get("x-body-encoding") === "gzip") body = body.pipeThrough(new DecompressionStream("gzip"));
  const bytes = await readLimited(body, MAX_INGEST).catch(() => null);
  if (!bytes) return fail(413, "too large, or not valid gzip");
  // The sender is authenticated; a full parse of a multi-megabyte list would cost more CPU than the
  // request is allowed, so only check it looks like a JSON document.
  const first = bytes.subarray(0, 64).find((c) => c > 32);
  if (first !== 0x7b && first !== 0x5b) return fail(400, "body is not JSON");
  await env.DATA.put(key, bytes, { httpMetadata: { contentType: "application/json; charset=utf-8" } });
  return new Response(null, { status: 204 });
}

/**
 * Snapshot keys in the private STATE bucket: `<name>/latest.json` (the manifest), `<name>/previous.json`
 * (the one before, read-only) and `<name>/<stamp>/<nnn>` (parts). `app-<network>` is the crawler's
 * database (STATE_TOKEN), `bench-<vantage>` a vantage's results (its own token); INGEST_TOKEN both.
 */
const STATE_KEY = new RegExp(`^(app-(?:${NETWORKS.join("|")})|bench-(${VANTAGE}))/(latest\\.json|previous\\.json|\\d{10}/\\d{3})$`);
/** Largest snapshot part (the client sends 64 MiB; Workers take request bodies up to 100 MB). */
const MAX_PART = 96 << 20;

/** GET/PUT /_state/<key>: the hosts' snapshots of state they can't rebuild (see src/snapshot.rs). */
async function state(request, env, url) {
  const key = url.pathname.slice("/_state/".length);
  const m = STATE_KEY.exec(key);
  if (!m) return fail(400, "key not allowed");
  const [, name, vantage, file] = m;
  if (!(await authorizedFor(request, env, vantage, vantage ? null : env.STATE_TOKEN))) return fail(401, "unauthorized");
  if (request.method === "GET") {
    const obj = await env.STATE.get(key);
    if (obj) return new Response(obj.body, { headers: { "cache-control": "no-store" } });
    // Marked, so a client can tell "there is no snapshot" from a 404 by anything else on the way.
    const r = fail(404, "no such snapshot");
    r.headers.set("x-state", "none");
    return r;
  }
  if (request.method !== "PUT") return fail(405, "method not allowed");
  const length = Number(request.headers.get("content-length") || 0);
  if (!request.body || !length || length > MAX_PART) return fail(413, "missing length, or too large");
  if (file === "previous.json") return fail(400, "key not allowed");
  if (file !== "latest.json") {
    await env.STATE.put(key, request.body);
    return new Response(null, { status: 204 });
  }
  // A manifest. It becomes the latest only if every part it names is stored and it descends from
  // the current latest (`parent`), so a host that started from an empty database, or a second
  // host, can't replace the history; the swap itself is conditional on the latest not changing.
  // The latest it replaces becomes the previous, and anything older (or left over from an upload
  // that never finished) goes.
  let manifest;
  try { manifest = JSON.parse(await request.text()); } catch { return fail(400, "manifest is not JSON"); }
  const { stamp, parts, parent = null } = manifest || {};
  if (!Number.isInteger(stamp) || !Number.isInteger(parts) || parts < 1 || parts > 999) return fail(400, "bad manifest");
  const partKey = (s, i) => `${name}/${s}/${String(i).padStart(3, "0")}`;
  for (let i = 0; i < parts; i++) {
    if (!(await env.STATE.head(partKey(stamp, i)))) return fail(409, `part ${i} is missing`);
  }
  const current = await env.STATE.get(`${name}/latest.json`);
  const old = current ? await current.json().catch(() => null) : null;
  if (old?.stamp === stamp) return new Response(null, { status: 204 }); // a retry of the same upload
  if (old && old.stamp !== parent) return fail(409, `snapshot ${stamp} descends from ${parent}, but the latest is ${old.stamp}`);
  const swapped = await env.STATE.put(`${name}/latest.json`, JSON.stringify(manifest),
    current ? { onlyIf: { etagMatches: current.etag } } : undefined);
  if (!swapped) return fail(409, "the latest snapshot changed meanwhile");
  if (old) await env.STATE.put(`${name}/previous.json`, JSON.stringify(old));
  const keep = new Set([stamp, old?.stamp].filter(Boolean).map(String));
  const listed = await env.STATE.list({ prefix: `${name}/` });
  const stale = listed.objects.map((o) => o.key).filter((k) => /\/\d{10}\//.test(k) && !keep.has(k.split("/")[1]));
  if (stale.length) await env.STATE.delete(stale);
  return new Response(null, { status: 204 });
}

const CHECK_PATH = new RegExp(`^/api/v1/(${NETWORKS.join("|")})/masternodes/([0-9a-fA-F]{64})/check$`);
/** One check of a masternode serves everyone who asks within this many seconds. */
const CHECK_REUSE = 60;

/**
 * POST /api/v1/<net>/masternodes/<protx>/check: a P2P handshake with the masternode's registered core
 * address, from this Cloudflare location (see check.js). Only registered masternodes, never an address
 * the caller picks, so this isn't a port scanner. CHECK_LIMIT (a rate limit binding) caps each client;
 * a masternode checked in the last minute gets that result again.
 */
async function check(request, env, ctx, url, net, protx) {
  if (request.method !== "POST") return fail(405, "use POST");
  // Limited per client (an IPv6 client by its /64, the block it can pick addresses from), and every check
  // that opens a connection per masternode too; a recent result is reused first.
  const limited = async (key) => env.CHECK_LIMIT && !(await env.CHECK_LIMIT.limit({ key })).success;
  const ip = request.headers.get("cf-connecting-ip") || "unknown";
  const v6 = ip.includes(":") ? parseAddress(`[${ip}]:0`)?.ip16 : null;
  const client = v6 ? [...v6.subarray(0, 8)].map((b) => b.toString(16).padStart(2, "0")).join("") : ip;
  if (await limited(client)) return fail(429, "too many checks from here; try again in a minute");
  const cacheKey = `${url.origin}/_check/${net}/${protx}`;
  const recent = await caches.default.match(cacheKey);
  if (recent) return json(200, { ...(await recent.json()), cached: true }, "no-store");
  if (await limited(`${net}:${protx}`)) return fail(429, "this masternode was just checked; try again in a minute");
  const mn = (await getJson(env, shardKey(net, protx)))?.[protx];
  if (!mn || mn.removed) return fail(404, "not a registered masternode");
  if (!mn.service) return fail(400, "this masternode has no core P2P address registered");
  // Mainnet's consensus rules pin masternodes to port 9999; testnet's don't, so only its default port is
  // checked (otherwise a free testnet registration could point the check at any host and port).
  if (net === "testnet" && !mn.service.endsWith(":19999")) return fail(400, "on testnet only the default port (19999) is checked");
  const result = await checkNode(connect, net, mn.service);
  const body = JSON.stringify({ ...result, address: mn.service, from: request.cf?.colo ? `Cloudflare ${request.cf.colo}` : "Cloudflare",
    checked: Math.floor(Date.now() / 1000) });
  ctx.waitUntil(caches.default.put(cacheKey, new Response(body, { headers: { "cache-control": `public, max-age=${CHECK_REUSE}` } })));
  return new Response(body, { headers: { ...API_HEADERS, "cache-control": "no-store" } });
}

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    const checking = CHECK_PATH.exec(url.pathname);
    if (checking) return check(request, env, ctx, url, checking[1], checking[2].toLowerCase());
    if (url.pathname.startsWith("/api/v1/")) return api(request, env, ctx, url);
    if (url.pathname.startsWith("/_ingest/")) return ingest(request, env, url);
    if (url.pathname.startsWith("/_state/")) return state(request, env, url);
    // Masternode pages share one HTML file; the script reads the ProTx from the path.
    if (/^\/mn\/[0-9a-fA-F]{64}\/?$/.test(url.pathname)) return env.ASSETS.fetch(new Request(new URL("/mn", url), request));
    return env.ASSETS.fetch(request);
  },
};
