import { test } from "node:test";
import assert from "node:assert/strict";
import { upgradeView } from "../../web/assets/platform-upgrade.js";

const f = {
  fmt: (n) => (n == null ? "–" : Math.round(n).toLocaleString("en-US")),
  pct: (x, digits = 1) => `${(x * 100).toFixed(digits)}%`,
  inAbout: () => "in 20 min",
};
const NOW = 1_800_000_000;
const evo = (vote, supports, status = "enabled") => ({ status, vote, supports });
const page = (upgrade, evonodes = [], now = {}) => ({
  upgrade: { versions: [], next_epoch: null, next_epoch_reported_by: 0, ...upgrade },
  now: { epoch: 20_372, ends: NOW + 1200, ...now },
  evonodes,
});

test("with every vote for the running protocol, it says that protocol is live and shows no meter", () => {
  const v = upgradeView(page({ phase: "active", current: 14, candidate: null, votes: 0, required: 29, required_pct: 67, enabled_evonodes: 43,
    versions: [[14, 28]], next_epoch: 14, next_epoch_reported_by: 44 }), NOW, f);
  assert.equal(v.state, "active");
  assert.equal(v.title, "Protocol 14 is live");
  assert.equal(v.badge, null);
  assert.equal(v.meter, null);
  assert.match(v.lede, /every vote is for the protocol Platform runs now, 14/);
  assert.deepEqual(v.stats, [{ label: "Voted for 14", value: "28", note: "the protocol running now" }]);
  assert.match(v.note, /^44 evonodes report which protocol Drive runs next epoch: 14\./);
});

test("running 14 with votes for 15 shows the vote in progress, with the live threshold", () => {
  const evonodes = [evo(15, 15), evo(15, 15), evo(14, 15), evo(14, 14), evo(15, 15, "banned")];
  const v = upgradeView(page({ phase: "voting", current: 14, candidate: 15, votes: 20, required: 29, required_pct: 67, enabled_evonodes: 43,
    versions: [[14, 8], [15, 20]], next_epoch: 14, next_epoch_reported_by: 40 }, evonodes), NOW, f);
  assert.equal(v.state, "voting");
  assert.equal(v.version, 15);
  assert.equal(v.title, "Protocol 15: ");
  assert.deepEqual(v.badge, { tone: "warning", text: "voting" });
  assert.match(v.lede, /^Platform runs protocol 14; evonodes are voting for 15\./);
  assert.match(v.lede, /If 29 of the 43 enabled evonodes have voted for 15 when epoch 20,372 ends \(ends in 20 min\), protocol 15 starts with epoch 20,373\. It needs 9 more\.$/);
  assert.deepEqual(v.meter, { votes: 20, required: 29, enabled: 43, met: false });
  const byLabel = Object.fromEntries(v.stats.map((s) => [s.label, s]));
  assert.equal(byLabel["Voted for 15"].value, "20");
  assert.equal(byLabel["Voted for 15"].note, "46.5% of enabled evonodes");
  assert.equal(byLabel["Votes needed"].note, "1 + 67% of 43 enabled, rounded down");
  // Banned evonodes aren't in the electorate; one enabled evonode runs 15 but last voted 14.
  assert.equal(byLabel["Run software for 15"].value, "3");
  assert.equal(byLabel["Upgraded, vote not counted yet"].value, "1");
  assert.equal(byLabel["Voted for 14"].note, "the protocol running now");
  assert.equal(v.pending, 1);
});

test("a vote past the threshold says it has enough, and the meter is met", () => {
  const v = upgradeView(page({ phase: "voting", current: 14, candidate: 15, votes: 300, required: 238, required_pct: 67, enabled_evonodes: 355,
    versions: [[14, 54], [15, 300]] }, [], { epoch: 88 }), NOW, f);
  assert.match(v.lede, /It has enough votes now\.$/);
  assert.equal(v.meter.met, true);
  assert.equal(v.stats[0].tone, "good");
});

test("a locked-in upgrade shows when it starts, without a vote meter", () => {
  const v = upgradeView(page({ phase: "locked_in", current: 14, candidate: null, votes: 0, required: 29, required_pct: 67, enabled_evonodes: 43,
    versions: [[15, 3]], next_epoch: 15, next_epoch_reported_by: 41 }, [evo(15, 15), evo(14, 14)]), NOW, f);
  assert.equal(v.state, "locked_in");
  assert.equal(v.title, "Protocol 15: ");
  assert.deepEqual(v.badge, { tone: "good", text: "locked in" });
  assert.equal(v.meter, null);
  assert.equal(v.lede, "Drive has scheduled protocol 15 for the next epoch. Platform runs 14 until epoch 20,372 ends in 20 min; protocol 15 starts with epoch 20,373.");
  assert.deepEqual(v.stats.map((s) => s.label), ["Run software for 15"]);
});

test("nothing is tied to one version: running 15 with a vote for 16 reads the same way", () => {
  const v = upgradeView(page({ phase: "voting", current: 15, candidate: 16, votes: 4, required: 7, required_pct: 67, enabled_evonodes: 10,
    versions: [[15, 6], [16, 4]] }), NOW, f);
  assert.equal(v.title, "Protocol 16: ");
  assert.match(v.lede, /^Platform runs protocol 15; evonodes are voting for 16\..*It needs 3 more\.$/);
});

test("without the share from the crawler, the threshold is still explained", () => {
  const v = upgradeView(page({ phase: "voting", current: 14, candidate: 15, votes: 1, required: 29, enabled_evonodes: 43, versions: [[15, 1]] }), NOW, f);
  assert.equal(v.stats.find((s) => s.label === "Votes needed").note, "of 43 enabled, as Drive counts them");
});

test("an epoch past its estimate ends any moment", () => {
  const v = upgradeView(page({ phase: "voting", current: 14, candidate: 15, votes: 1, required: 29, required_pct: 67, enabled_evonodes: 43,
    versions: [[15, 1]] }, [], { ends: NOW - 60 }), NOW, f);
  assert.match(v.lede, /when epoch 20,372 ends \(ends any moment\)/);
});
