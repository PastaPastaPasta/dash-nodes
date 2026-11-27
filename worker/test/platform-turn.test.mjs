import { test } from "node:test";
import assert from "node:assert/strict";
import { findEvonodes, host, turnView, turnsAway } from "../../web/assets/platform-turn.js";

const f = { fmt: (n) => n.toLocaleString("en-US"), inAbout: (ts) => `in ${Math.round((ts - TS) / 60)} min`, dateTime: () => "27 Nov 2026, 03:00 UTC" };
const TS = 1_795_741_225;
const BLOCK = 115.5;
const evo = (protx, service, slots, status = "enabled") =>
  ({ protx, service, status, next: slots ? TS + Math.round(slots * BLOCK) : null, slots });
const A = evo(`7047${"a".repeat(60)}`, "89.125.209.234:9999", 1);
const B = evo(`7048${"b".repeat(60)}`, "[2001:db8::1]:9999", 45);
const C = evo(`7048${"c".repeat(60)}`, "10.0.0.3:443", null);
const D = evo(`d00d${"d".repeat(60)}`, "10.0.0.4:9999", null, "banned");
const doc = {
  evonodes: [A, B, C, D],
  // The published list stops at the first few; later turns come from each evonode's ETA.
  schedule: { ts: TS, block_secs: BLOCK, next: [{ protx: A.protx, slots: 1, eta: A.next }] },
};

test("an IP matches with or without its port, IPv6 too", () => {
  assert.equal(host("89.125.209.234:9999"), "89.125.209.234");
  assert.equal(host("[2001:db8::1]:9999"), "2001:db8::1");
  assert.equal(host("2001:db8::1"), "2001:db8::1");
  assert.deepEqual(findEvonodes(doc.evonodes, " 89.125.209.234 "), [A]);
  assert.deepEqual(findEvonodes(doc.evonodes, "89.125.209.234:9999"), [A]);
  assert.deepEqual(findEvonodes(doc.evonodes, "2001:DB8::1"), [B]);
  assert.deepEqual(findEvonodes(doc.evonodes, "89.125.209.23"), []);
  // Two evonodes on one IP: the port picks one.
  const E = evo(`e0e0${"e".repeat(60)}`, "89.125.209.234:20001", 3);
  assert.deepEqual(findEvonodes([A, E], "89.125.209.234"), [A, E]);
  assert.deepEqual(findEvonodes([A, E], "89.125.209.234:20001"), [E]);
});

test("a ProTx matches in full or by a prefix of at least 4 hex digits, in any case", () => {
  assert.deepEqual(findEvonodes(doc.evonodes, A.protx.toUpperCase()), [A]);
  assert.deepEqual(findEvonodes(doc.evonodes, "7047"), [A]);
  assert.deepEqual(findEvonodes(doc.evonodes, `0x${A.protx}`), [A]);
  assert.deepEqual(findEvonodes(doc.evonodes, "7048"), [B, C]);
  assert.deepEqual(findEvonodes(doc.evonodes, "704"), []);
});

test("turns come from the published list, else from the ETA", () => {
  assert.equal(turnsAway(doc.schedule, A), 1);
  assert.equal(turnsAway(doc.schedule, B), 45);
});

test("a found evonode gets one line: when, the date, and its place in line", () => {
  assert.deepEqual(turnView(doc, "7047", f), {
    kind: "turn", protx: A.protx, service: A.service, eta: A.next, turns: 1,
    text: "proposes in 2 min (27 Nov 2026, 03:00 UTC), next in line.",
  });
  assert.equal(turnView(doc, "2001:db8::1", f).text, "proposes in 87 min (27 Nov 2026, 03:00 UTC), 45 turns from now.");
});

test("misses, ambiguities, banned and idle evonodes each say so", () => {
  assert.equal(turnView(doc, "  ", f).kind, "empty");
  assert.equal(turnView(doc, "ffff", f).kind, "none");
  assert.equal(turnView(doc, "hello", f).kind, "none");
  const many = turnView(doc, "7048", f);
  assert.equal(many.kind, "many");
  assert.match(many.text, /^That matches 2 evonodes/);
  assert.equal(turnView(doc, "d00d", f).kind, "banned");
  assert.equal(turnView(doc, "10.0.0.3", f).kind, "idle");
  assert.equal(turnView({ ...doc, schedule: null }, "7047", f).kind, "unknown");
});
