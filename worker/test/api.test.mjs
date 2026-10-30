import { test } from "node:test";
import assert from "node:assert/strict";
import { narrow, route, KEY, shardOf, ApiError } from "../src/api.js";

const doc = { resolution: "crawl", t: [1, 2], series: [
  { key: "a", values: [5, 4] }, { key: "b", values: [3, 0] }, { key: "c", values: [0, 1] }, { key: "Other", values: [1, 1] }] };

test("narrow folds the tail and the existing Other into Other", () => {
  assert.deepEqual(narrow(doc, 1).series, [{ key: "a", values: [5, 4] }, { key: "Other", values: [4, 2] }]);
  assert.equal(narrow(doc, 3), doc);
  const plain = { ...doc, series: doc.series.slice(0, 2) };
  assert.equal(narrow(plain, 5), plain);
});

test("shards spread the first byte over 64 documents", () => {
  assert.equal(shardOf("00" + "0".repeat(62)), 0);
  assert.equal(shardOf("ff" + "0".repeat(62)), 63);
  assert.equal(shardOf("80" + "0".repeat(62)), 32);
});

test("ingest keys are an allow-list", () => {
  for (const k of ["v1/status.json", "v1/mainnet/summary.json", "v1/testnet/mn/3f.json", "v1/mainnet/history/totals.json",
    "v1/mainnet/history/mn/version.json", `v1/mainnet/payments/${"a".repeat(64)}.json`,
    `v1/mainnet/payments/${"a".repeat(64)}/25.json`]) assert.ok(KEY.test(k), k);
  for (const k of ["v1/devnet/summary.json", "v1/mainnet/../x.json", "v1/mainnet/history/mn/totals.json", "v1/mainnet/mn/3F.json",
    "status.json", "v1/mainnet/history/x/version.json"]) assert.ok(!KEY.test(k), k);
});

test("route validates parameters before reading", async () => {
  const get = async () => { throw new Error("should not read"); };
  const q = (s) => new URLSearchParams(s);
  await assert.rejects(route(["devnet", "summary"], q(""), get), (e) => e instanceof ApiError && e.status === 404);
  await assert.rejects(route(["mainnet", "history"], q("dim=version&days=10"), get), (e) => e.status === 400);
  await assert.rejects(route(["mainnet", "history"], q("dim=version&days=x"), get), (e) => e.status === 400);
  await assert.rejects(route(["mainnet", "masternodes", "zz"], q(""), get), (e) => e.status === 400);
});

test("history picks the range and narrows; payments page by height", async () => {
  const store = { "v1/mainnet/history/mn/version.json": { ranges: { 30: doc } },
    [`v1/mainnet/payments/${"b".repeat(64)}.json`]: { buckets: [2, 0] },
    [`v1/mainnet/payments/${"b".repeat(64)}/2.json`]: [{ height: 200_030 }, { height: 200_020 }],
    [`v1/mainnet/payments/${"b".repeat(64)}/0.json`]: [{ height: 30 }, { height: 20 }] };
  const get = async (k) => store[k] ?? null;
  const h = await route(["mainnet", "history"], new URLSearchParams("scope=mn&dim=version&top=2"), get);
  assert.deepEqual(h.series.map((s) => s.key), ["a", "b", "Other"]);
  const pay = (q) => route(["mainnet", "masternodes", "b".repeat(64), "payments"], new URLSearchParams(q), get);
  assert.deepEqual(await pay("before=30&limit=1"), { payments: [{ height: 20 }] });
  assert.deepEqual(await pay("limit=3"), { payments: [{ height: 200_030 }, { height: 200_020 }, { height: 30 }] });
  assert.deepEqual(await pay("before=200021"), { payments: [{ height: 200_020 }, { height: 30 }, { height: 20 }] });
  assert.deepEqual((await route(["mainnet", "masternodes", "c".repeat(64), "payments"], new URLSearchParams(""), get)), { payments: [] });
  await assert.rejects(route(["mainnet", "summary"], new URLSearchParams(""), get), (e) => e.status === 503);
});
