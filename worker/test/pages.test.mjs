import { test } from "node:test";
import assert from "node:assert/strict";
import { CSP, MN_CSP, MN_PAGE, PAGE_HEADERS, mnPage } from "../src/pages.js";

const directives = (p) => Object.fromEntries(p.split("; ").map((d) => { const [k, ...v] = d.split(" "); return [k, v]; }));

test("masternode pages' policy is the default plus what the Platform SDK needs, nothing else", () => {
  const base = directives(CSP), mn = directives(MN_CSP);
  assert.deepEqual(Object.keys(mn), Object.keys(base));
  const added = Object.fromEntries(Object.keys(mn).map((k) => [k, mn[k].filter((v) => !base[k].includes(v))]).filter(([, v]) => v.length));
  assert.deepEqual(added, { "script-src": ["'wasm-unsafe-eval'"], "connect-src": ["https:"], "worker-src": ["blob:"] });
  for (const k of Object.keys(base)) for (const v of base[k]) assert.ok(mn[k].includes(v), `${k} keeps ${v}`);
  assert.doesNotMatch(MN_CSP, /'unsafe-eval'|'unsafe-inline'/);
  assert.match(CSP, /^default-src 'self'; script-src 'self' https:\/\/static\.cloudflareinsights\.com; /);
});

test("a masternode page comes from /mn with the masternode policy, whatever the asset store set", async () => {
  const asked = [];
  const env = { ASSETS: { fetch: async (req) => {
    asked.push(new URL(req.url).pathname);
    return new Response("<html>", { status: 200, headers: { "content-type": "text/html", "content-security-policy": CSP, etag: '"x"' } });
  } } };
  const url = new URL(`https://radar.dashhq.org/mn/${"ab".repeat(32)}`);
  assert.ok(MN_PAGE.test(url.pathname));
  assert.ok(!MN_PAGE.test("/mn/abc") && !MN_PAGE.test(`/mn/${"ab".repeat(32)}/x`));
  const r = await mnPage(env, new Request(url), url);
  assert.deepEqual(asked, ["/mn"]);
  assert.equal(r.status, 200);
  assert.equal(r.headers.get("content-security-policy"), MN_CSP);
  assert.equal(r.headers.get("x-content-type-options"), PAGE_HEADERS["X-Content-Type-Options"]);
  assert.equal(r.headers.get("referrer-policy"), PAGE_HEADERS["Referrer-Policy"]);
  assert.equal(r.headers.get("content-type"), "text/html");
  assert.equal(r.headers.get("etag"), '"x"');
  assert.equal(await r.text(), "<html>");
  // A revalidation stays a 304, with the same policy.
  env.ASSETS.fetch = async () => new Response(null, { status: 304 });
  const not = await mnPage(env, new Request(url), url);
  assert.equal(not.status, 304);
  assert.equal(not.headers.get("content-security-policy"), MN_CSP);
});
