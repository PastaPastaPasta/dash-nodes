// nodes.dashhq.org: the static site (Workers static assets), the public API answered from R2, and an
// authenticated ingest the crawler pushes documents to. The crawler's host takes no inbound traffic.
import { route, ApiError, KEY } from "./api.js";

const API_HEADERS = {
  "content-type": "application/json; charset=utf-8",
  "access-control-allow-origin": "*",
  "x-content-type-options": "nosniff",
};
const MAX_INGEST = 32 << 20;

const json = (status, body, cache) =>
  new Response(JSON.stringify(body), { status, headers: { ...API_HEADERS, "cache-control": cache } });

/** Passes a stored document through without parsing it (the big ones: masternode list, summary). */
const PASSTHROUGH = /^(status|(mainnet|testnet)\/(summary|geo|masternodes))$/;

async function api(request, env, ctx, url) {
  if (request.method !== "GET" && request.method !== "HEAD") return json(405, { error: "method not allowed" }, "no-store");
  const cache = caches.default;
  const hit = await cache.match(request);
  if (hit) return hit;
  const path = url.pathname.slice("/api/v1/".length).replace(/\/$/, "");
  let response;
  if (PASSTHROUGH.test(path) && !url.search) {
    const key = path === "status" ? "v1/status.json" : `v1/${path}.json`;
    const obj = await env.DATA.get(key);
    response = obj
      ? new Response(obj.body, { headers: { ...API_HEADERS, "cache-control": "public, max-age=60", etag: obj.httpEtag } })
      : json(503, { error: "not published yet" }, "no-store");
  } else {
    try {
      const get = async (key) => {
        const obj = await env.DATA.get(key);
        return obj ? obj.json() : null;
      };
      response = json(200, await route(path.split("/"), url.searchParams, get), "public, max-age=60");
    } catch (e) {
      if (!(e instanceof ApiError)) throw e;
      response = json(e.status, { error: e.message }, "no-store");
    }
  }
  if (response.ok) ctx.waitUntil(cache.put(request, response.clone()));
  return response;
}

/** Constant-time bearer check against the INGEST_TOKEN secret. */
async function authorized(request, env) {
  const got = (request.headers.get("authorization") || "").replace(/^Bearer /, "");
  if (!env.INGEST_TOKEN || !got) return false;
  const enc = new TextEncoder();
  const [a, b] = await Promise.all([enc.encode(got), enc.encode(env.INGEST_TOKEN)].map((x) => crypto.subtle.digest("SHA-256", x)));
  return crypto.subtle.timingSafeEqual(a, b);
}

/** PUT /_ingest/<key>: the crawler publishing one document, gzip-compressed (X-Body-Encoding: gzip). */
async function ingest(request, env, url) {
  if (request.method !== "PUT") return json(405, { error: "method not allowed" }, "no-store");
  if (!(await authorized(request, env))) return json(401, { error: "unauthorized" }, "no-store");
  const key = url.pathname.slice("/_ingest/".length);
  if (!KEY.test(key)) return json(400, { error: "key not allowed" }, "no-store");
  if (Number(request.headers.get("content-length") || 0) > MAX_INGEST) return json(413, { error: "too large" }, "no-store");
  let body = request.body;
  if (request.headers.get("x-body-encoding") === "gzip") body = body.pipeThrough(new DecompressionStream("gzip"));
  const bytes = await new Response(body).arrayBuffer();
  if (bytes.byteLength > 4 * MAX_INGEST) return json(413, { error: "too large" }, "no-store");
  // The sender is authenticated; a full parse of a multi-megabyte list would cost more CPU than the
  // request is allowed, so only check it looks like a JSON document.
  const first = new Uint8Array(bytes, 0, Math.min(bytes.byteLength, 64)).find((c) => c > 32);
  if (first !== 0x7b && first !== 0x5b) return json(400, { error: "body is not JSON" }, "no-store");
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
