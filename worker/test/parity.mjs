// Compares Worker answers with the Rust API they were published from: node test/parity.mjs <api> <worker>
import assert from "node:assert/strict";
const [a, b] = process.argv.slice(2);
const both = async (path) => {
  const [x, y] = await Promise.all([a, b].map(async (base) => { const r = await fetch(`${base}/api/v1/${path}`); return [r.status, await r.json()]; }));
  return [x, y];
};
const pr = "d353cdcae53071401345c38ab482935cb62442be2174a1d3210ca356c9017eff";
const paths = [
  "status", "mainnet/summary", "testnet/summary", "mainnet/masternodes",
  "mainnet/history?scope=mn&dim=version&days=30&top=6", "mainnet/history?scope=all&dim=asn&days=7&top=8",
  "mainnet/history?scope=mn&dim=version&days=1", "mainnet/history?dim=totals&days=90", "mainnet/history?scope=nonmn&dim=country&days=365&top=3",
  `mainnet/masternodes/${pr}`, `mainnet/masternodes/${pr.toUpperCase()}`, `mainnet/masternodes/${pr}/payments`,
  `mainnet/masternodes/${pr}/payments?before=2550000&limit=5`, "mainnet/masternodes/zz", "mainnet/masternodes/" + "0".repeat(64),
  "devnet/summary", "mainnet/history?dim=nope",
];
let fail = 0;
for (const p of paths) {
  const [[s1, j1], [s2, j2]] = await both(p);
  try { assert.equal(s2, s1); if (s1 === 200) assert.deepEqual(j2, j1); console.log("ok  ", p); }
  catch (e) { fail++; console.log("FAIL", p, s1, s2, JSON.stringify(j1).slice(0, 120), "|", JSON.stringify(j2).slice(0, 120)); }
}
process.exit(fail ? 1 : 0);
