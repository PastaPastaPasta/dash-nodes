// The public API, answered from documents the crawler publishes to R2. Pure functions over a `get(key)`
// that returns parsed JSON (or null), so they can be tested without a Worker runtime.

export const NETWORKS = ["mainnet", "testnet"];
/** Per-network documents served as published. */
export const DOCS = ["summary", "geo", "masternodes"];
export const SCOPES = ["all", "mn", "evo", "nonmn"];
export const DIMS = ["totals", "ua", "version", "client", "protocol", "country", "asn", "reachable", "status", "type", "network"];
/** Ranges the crawler precomputes. Up to 14 days it's one point per crawl, beyond that daily averages. */
export const DAYS = [1, 7, 14, 30, 90, 365];
/** Series kept per published history document; `top` can only narrow it. */
export const MAX_TOP = 20;
export const SHARDS = 64;
/** Payments are stored in blocks of this many heights, so a new payment only rewrites the newest one. */
export const PAYMENT_BUCKET = 100_000;
/** Query parameters each endpoint reads; the Worker caches on these alone. */
export const PARAMS = { history: ["scope", "dim", "days", "top"], payments: ["before", "limit"] };

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

/** Where a masternode's detail lives: one of SHARDS documents, by the first byte of its ProTx hash. */
export const shardOf = (protx) => (parseInt(protx.slice(0, 2), 16) * SHARDS) >> 8;
export const shardKey = (net, protx) => `v1/${net}/mn/${shardOf(protx).toString(16).padStart(2, "0")}.json`;

/** Keys the crawler may write. Anything else is refused at ingest. */
export const KEY = new RegExp(
  `^v1/(status\\.json|(${NETWORKS.join("|")})/(` +
  `(${DOCS.join("|")})\\.json` +
  `|history/(totals|(${SCOPES.join("|")})/(${DIMS.filter((d) => d !== "totals").join("|")}))\\.json` +
  "|mn/[0-9a-f]{2}\\.json|payments/[0-9a-f]{64}(/\\d{1,4})?\\.json))$",
);

function parseProtx(s) {
  if (!/^[0-9a-fA-F]{64}$/.test(s)) throw new ApiError(400, "protx must be 64 hex characters");
  return s.toLowerCase();
}

function intParam(q, name, fallback) {
  const v = q.get(name);
  if (v == null || v === "") return fallback;
  if (!/^-?\d+$/.test(v)) throw new ApiError(400, `${name} must be an integer`);
  return Number(v);
}

const clamp = (v, lo, hi) => Math.min(Math.max(v, lo), hi);

async function required(get, key, what) {
  const doc = await get(key);
  if (doc == null) throw new ApiError(503, `${what} hasn't been published yet`);
  return doc;
}

/**
 * Narrows a published series set to `top`, the same way the crawler's API does: keep the first `top`
 * (already ranked by total over the window), fold the rest, and any existing "Other", into "Other".
 */
export function narrow(doc, top) {
  const named = doc.series.filter((s) => s.key !== "Other");
  const other = doc.series.find((s) => s.key === "Other");
  if (named.length <= top) return doc;
  const folded = doc.t.map((_, i) => named.slice(top).reduce((a, s) => a + s.values[i], other ? other.values[i] : 0));
  return { ...doc, series: [...named.slice(0, top), { key: "Other", values: folded }] };
}

async function history(get, net, query) {
  const dim = query.get("dim") || "";
  if (!DIMS.includes(dim)) throw new ApiError(400, `unknown dim ${dim}`);
  const totals = dim === "totals";
  const scope = query.get("scope") || "all";
  if (!totals && !SCOPES.includes(scope)) throw new ApiError(400, `unknown scope ${scope}`);
  const days = intParam(query, "days", 30);
  if (!DAYS.includes(days)) throw new ApiError(400, `days must be one of ${DAYS.join(", ")}`);
  const top = clamp(intParam(query, "top", 8), 1, MAX_TOP);
  const doc = await required(get, `v1/${net}/history/${totals ? "totals" : `${scope}/${dim}`}.json`, "History");
  const range = doc.ranges?.[days];
  if (!range) throw new ApiError(503, "History for this range hasn't been published yet");
  return totals ? range : narrow(range, top);
}

/** A masternode's payments, newest first: `payments/{protx}.json` lists its buckets, newest first. */
async function payments(get, net, protx, query) {
  const id = parseProtx(protx);
  const limit = clamp(intParam(query, "limit", 100), 1, 1000);
  const before = intParam(query, "before", null);
  const out = [];
  for (const b of (await get(`v1/${net}/payments/${id}.json`))?.buckets || []) {
    if (before != null && b * PAYMENT_BUCKET >= before) continue;
    for (const p of (await get(`v1/${net}/payments/${id}/${b}.json`)) || []) {
      if (before != null && p.height >= before) continue;
      out.push(p);
      if (out.length === limit) return { payments: out };
    }
  }
  return { payments: out };
}

/** Routes /api/v1/... (the path after it, split) to a JSON value. */
export async function route(parts, query, get) {
  if (parts.length === 1 && parts[0] === "status") return required(get, "v1/status.json", "Status");
  const [net, what, protx, sub] = parts;
  if (!NETWORKS.includes(net)) throw new ApiError(404, `unknown network ${net}`);
  if (parts.length === 2 && DOCS.includes(what)) return required(get, `v1/${net}/${what}.json`, `The ${what} document`);
  if (parts.length === 2 && what === "history") return history(get, net, query);
  if (what === "masternodes" && parts.length === 3) {
    const id = parseProtx(protx);
    const mn = (await required(get, shardKey(net, id), "Masternode details"))[id];
    if (!mn) throw new ApiError(404, "unknown protx");
    return mn;
  }
  if (what === "masternodes" && parts.length === 4 && sub === "payments") return payments(get, net, protx, query);
  throw new ApiError(404, "no such endpoint");
}
