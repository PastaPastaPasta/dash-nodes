import { test } from "node:test";
import assert from "node:assert/strict";
import { shortDate, uptimeTile, rank30Note, earnedRows, monthlyRow, missedTurns } from "../../web/assets/history.js";

const f = {
  fmt: (n) => (n == null ? "–" : Math.round(n).toLocaleString("en-US")),
  pct: (x, digits = 1) => `${(x * 100).toFixed(digits)}%`,
  amount: (c) => {
    if (c == null) return "–";
    const d = Math.abs(c / 1e11);
    return `${(c / 1e11).toLocaleString("en-US", { maximumFractionDigits: d >= 100 ? 0 : d >= 1 ? 2 : 4 })} DASH`;
  },
  lastSpan: (d) => (d > 0 ? `the last ${Math.round(d)} days` : null),
  // common.js's duration(); common.js itself needs a browser to load.
  duration: (s) => {
    if (s < 5400) return `${Math.max(1, Math.round(s / 60))} min`;
    if (s < 129600) return `${Math.round(s / 3600)} h`;
    const d = s / 86400;
    if (d < 60) return `${Math.round(d)} days`;
    if (d < 365) return `${Math.round(d / 30.44)} months`;
    const y = Number((d / 365.25).toFixed(1));
    return `${y} year${y === 1 ? "" : "s"}`;
  },
  now: Date.UTC(2026, 10, 17, 12),
};

const OCT_25 = 1_792_936_000; // 2026-10-25
const OCT_30 = 1_793_386_337; // 2026-10-30, epoch 87 start

test("shortDate formats UTC dates like the rest of the page", () => {
  assert.equal(shortDate(OCT_25), "25 Oct");
  assert.equal(shortDate(OCT_30), "30 Oct");
  assert.equal(shortDate("2026-10-29"), "29 Oct");
});

test("uptime tile names the actual days and first day under 30 rows, and keeps 30 days at 30+", () => {
  const short = Array.from({ length: 19 }, (_, i) => ({
    day: i === 0 ? "2026-10-29" : `2026-11-${String(i).padStart(2, "0")}`,
    attempts: i === 0 ? 64 : 88,
    ok: i === 0 ? 60 : 88,
  }));
  // 19 rows: 1,648 attempts, 1,644 ok -> 99.8%, "Uptime, 19 days", "since 29 Oct".
  assert.deepEqual(uptimeTile(short, f), {
    label: "Uptime, 19 days",
    value: "99.8%",
    foot: "1,644 of 1,648 port checks, since 29 Oct",
  });
  assert.deepEqual(uptimeTile([{ day: "2026-11-16", attempts: 10, ok: 10 }], f), {
    label: "Uptime, 1 day",
    value: "100.0%",
    foot: "10 of 10 port checks, since 16 Nov",
  });
  assert.deepEqual(uptimeTile([], f), { label: "Uptime", value: "–", foot: "no checks yet" });
  // A node that left the list stops collecting rows: its window ends on the last one.
  assert.deepEqual(uptimeTile(short.slice(0, 6), { ...f, now: Date.UTC(2026, 11, 20) }).foot, "500 of 504 port checks, 29 Oct – 5 Nov");

  const full = Array.from({ length: 45 }, (_, i) => ({
    day: `2026-10-${String((i % 28) + 1).padStart(2, "0")}`,
    attempts: 96,
    ok: 96,
  }));
  assert.deepEqual(uptimeTile(full, f), {
    label: "Uptime, 30 days",
    value: "100.0%",
    foot: "2,880 of 2,880 port checks",
  });
});

test("30-day rank bar shows the short-history note under 30 days, and ranks from 30 days on", () => {
  assert.equal(rank30Note({ history_days: 22 }, f), "Here only 22 of the last 30 days; ranked once it has 30");
  assert.equal(rank30Note({ history_days: 0 }, f), "Here only 0 of the last 30 days; ranked once it has 30");
  assert.equal(rank30Note({ history_days: 30 }, f), null);
  assert.equal(rank30Note({ history_days: 400 }, f), null);
  assert.equal(rank30Note({}, f), null);
});

test("earned rows drop windows longer than history_days and relabel the lifetime row under 30 days", () => {
  const totals = {
    d30: 1_414_060_081_789,
    d90: 1_414_060_081_789,
    d365: 1_414_060_081_789,
    lifetime: 1_414_060_081_789,
    epochs_paid: 2,
  };
  assert.deepEqual(earnedRows({ first_epoch: 34, since: OCT_25, history_days: 22, totals }, f), [
    { label: "Since 25 Oct (22 days)", value: "14.14 DASH", note: " · 2 epochs paid" },
  ]);
  // Under two days (testnet epochs are an hour), the span is said in hours, not "(0 days)".
  const hours = { ...f, now: (OCT_25 + 3 * 3600) * 1000 };
  assert.deepEqual(earnedRows({ first_epoch: 34, since: OCT_25, history_days: 0, totals }, hours)[0].label, "Since 25 Oct (3 h)");

  // 45 days: keeps Last 30 days and "Since epoch 34", drops 90 and 365.
  const mid = earnedRows({ first_epoch: 34, since: OCT_25, history_days: 45, totals }, f);
  assert.deepEqual(mid.map((r) => r.label), ["Last 30 days", "Since epoch 34"]);

  // 120 days: keeps 30 and 90, drops 365.
  const qtr = earnedRows({ first_epoch: 34, since: OCT_25, history_days: 120, totals }, f);
  assert.deepEqual(qtr.map((r) => r.label), ["Last 30 days", "Last 90 days", "Since epoch 34"]);

  // 400 days (and pre-deploy data with no history_days): all four rows as today.
  const old = earnedRows({ first_epoch: 34, since: 1_750_000_000, history_days: 400, totals }, f);
  assert.deepEqual(old.map((r) => r.label), ["Last 30 days", "Last 90 days", "Last 365 days", "Since epoch 34"]);
  const legacy = earnedRows({ first_epoch: 34, totals }, f);
  assert.deepEqual(legacy, old);
});

test("monthly Platform estimate uses the node's own pace, waits for a full epoch, and leaves old nodes as today", () => {
  assert.deepEqual(
    monthlyRow({ monthly: 3_008_400_234_636, monthly_basis_days: 9.126, monthly_since: OCT_30, history_days: 22 }, f),
    { value: "30.08 DASH", note: " at its pace since 30 Oct (9 days)" },
  );
  assert.deepEqual(
    monthlyRow({ monthly: null, monthly_basis_days: null, monthly_since: null, history_days: 3 }, f),
    { value: null, note: "once it has a full epoch" },
  );
  assert.deepEqual(
    monthlyRow({ monthly: 30_000_000_000, monthly_basis_days: 0.125, monthly_since: OCT_30, history_days: 0 }, f),
    { value: "0.3 DASH", note: " at its pace since 30 Oct (3 h)" },
  );
  assert.deepEqual(
    monthlyRow({ monthly: 2_000_000_000_000, monthly_basis_days: 54.75, monthly_since: null, history_days: 400 }, f),
    { value: "20 DASH", note: " at its pace of the last 55 days" },
  );
  // Pre-deploy data without the new fields, and a young chain before six epochs have finished.
  assert.deepEqual(
    monthlyRow({ monthly: 774_834_157_829, monthly_basis_days: 54.75 }, f),
    { value: "7.75 DASH", note: " at its pace of the last 55 days" },
  );
  assert.deepEqual(
    monthlyRow({ monthly: 0, monthly_basis_days: null, history_days: 100 }, f),
    { value: "0 DASH", note: " once six epochs have finished" },
  );
});

test("missed turns name the registration date under 30 days and keep 'last 30 days' otherwise", () => {
  const turns_30d = { missed: 0, turns: 34 };
  assert.equal(missedTurns({ since: OCT_25, history_days: 22, turns_30d }, f), "0 of 34 turns, since 25 Oct");
  assert.equal(missedTurns({ since: OCT_25, history_days: 30, turns_30d }, f), "0 of 34 turns, last 30 days");
  assert.equal(missedTurns({ turns_30d }, f), "0 of 34 turns, last 30 days");
  assert.equal(missedTurns({ since: OCT_25, history_days: 22, turns_30d: { missed: 0, turns: 0 } }, f), null);
});
