// Copies every document the Worker serves from a running dash-nodes API into a Worker's ingest, the way
// the crawler publishes them. For local testing: node scripts/seed.mjs <api base> <worker base> <token> [net...]
import { gzipSync } from "node:zlib";
import { SCOPES, DIMS, DAYS, MAX_TOP, shardKey } from "../src/api.js";

const [src, dst, token, ...nets] = process.argv.slice(2);
const get = async (path) => {
  const r = await fetch(`${src}/api/v1/${path}`);
  if (!r.ok) throw new Error(`${path}: HTTP ${r.status}`);
  return r.json();
};
let puts = 0;
const put = async (key, doc) => {
  const r = await fetch(`${dst}/_ingest/${key}`, {
    method: "PUT", body: gzipSync(JSON.stringify(doc)),
    headers: { authorization: `Bearer ${token}`, "x-body-encoding": "gzip", "content-type": "application/json" },
  });
  if (r.status !== 204) throw new Error(`${key}: HTTP ${r.status} ${await r.text()}`);
  puts++;
};
await put("v1/status.json", await get("status"));
for (const net of nets.length ? nets : ["mainnet", "testnet"]) {
  for (const what of ["summary", "masternodes"]) await put(`v1/${net}/${what}.json`, await get(`${net}/${what}`));
  try { await put(`v1/${net}/geo.json`, await get(`${net}/geo`)); } catch (e) { console.log("skip geo:", e.message); }
  const history = async (scope, dim) => Object.fromEntries(await Promise.all(DAYS.map(async (d) =>
    [String(d), await get(`${net}/history?scope=${scope}&dim=${dim}&days=${d}&top=${MAX_TOP}`)])));
  await put(`v1/${net}/history/totals.json`, { ranges: await history("all", "totals") });
  for (const scope of SCOPES) for (const dim of DIMS.filter((d) => d !== "totals")) {
    try { await put(`v1/${net}/history/${scope}/${dim}.json`, { ranges: await history(scope, dim) }); } catch (e) { console.log(`skip ${scope}/${dim}:`, e.message); }
  }
  const { masternodes } = await get(`${net}/masternodes`);
  const shards = new Map();
  for (let i = 0; i < masternodes.length; i += 32) {
    await Promise.all(masternodes.slice(i, i + 32).map(async ({ protx }) => {
      const key = shardKey(net, protx);
      if (!shards.has(key)) shards.set(key, {});
      shards.get(key)[protx] = await get(`${net}/masternodes/${protx}`);
      const pays = [];
      for (let before = null; ;) {
        const { payments } = await get(`${net}/masternodes/${protx}/payments?limit=1000${before ? `&before=${before}` : ""}`);
        pays.push(...payments);
        if (payments.length < 1000) break;
        before = payments.at(-1).height;
      }
      if (pays.length) await put(`v1/${net}/payments/${protx}.json`, pays);
    }));
  }
  for (const [key, doc] of shards) await put(key, doc);
  console.log(net, "done");
}
console.log("documents published:", puts);
