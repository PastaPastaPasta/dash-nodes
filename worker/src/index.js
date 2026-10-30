// nodes.dashhq.org: the static site (Workers static assets), the public API answered from R2, and an
// authenticated ingest the crawler pushes documents to. The crawler's host takes no inbound traffic.
import { route, ApiError, KEY, NETWORKS, DOCS, PARAMS } from "./api.js";

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

/** Paths whose stored document is served as is. */
const PASSTHROUGH = new RegExp(`^(status|(${NETWORKS.join("|")})/(${DOCS.join("|")}))$`);

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
    return json(200, await route(path.split("/"), query, (key) => getJson(env, key)), "public, max-age=60");
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
  return request.method === "HEAD" ? new Response(null, response) : response;
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

/** Constant-time bearer check against the INGEST_TOKEN secret. */
async function authorized(request, env) {
  const got = (request.headers.get("authorization") || "").replace(/^Bearer /, "");
  if (!env.INGEST_TOKEN || !got) return false;
  const digest = (text) => crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  const [a, b] = await Promise.all([digest(got), digest(env.INGEST_TOKEN)]);
  return crypto.subtle.timingSafeEqual(a, b);
}

/** PUT /_ingest/<key>: the crawler publishing one document, gzip-compressed (X-Body-Encoding: gzip). */
async function ingest(request, env, url) {
  if (request.method !== "PUT") return fail(405, "method not allowed");
  if (!(await authorized(request, env))) return fail(401, "unauthorized");
  const key = url.pathname.slice("/_ingest/".length);
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

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    if (url.pathname.startsWith("/api/v1/")) return api(request, env, ctx, url);
    if (url.pathname.startsWith("/_ingest/")) return ingest(request, env, url);
    // Masternode pages share one HTML file; the script reads the ProTx from the path.
    if (/^\/mn\/[0-9a-fA-F]{64}\/?$/.test(url.pathname)) return env.ASSETS.fetch(new Request(new URL("/mn", url), request));
    return env.ASSETS.fetch(request);
  },
};
