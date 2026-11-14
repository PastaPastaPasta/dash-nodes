// The protocol upgrade vote on /platform as plain data, from the crawler's `upgrade`, `now` and `evonodes`
// (rewards.rs upgrade_json). It knows no version, threshold, epoch length or network: all of it comes from
// what Drive reports. Formatting comes in from the page (common.js).

/**
 * @param doc the /platform document
 * @param nowSecs the time to judge the epoch's end against, in seconds
 * @param f { fmt, pct, inAbout } from common.js
 */
export function upgradeView(doc, nowSecs, { fmt, pct, inAbout }) {
  const u = doc.upgrade;
  const n = doc.now;
  const locked = u.phase === "locked_in" && u.next_epoch != null && u.next_epoch > u.current;
  const v = locked ? u.next_epoch : u.phase === "voting" ? u.candidate ?? null : null;
  const ends = n.ends > nowSecs ? `ends ${inAbout(n.ends)}` : "ends any moment";
  const enabled = doc.evonodes.filter((e) => e.status === "enabled");
  const supports = v == null ? [] : enabled.filter((e) => e.supports >= v);
  // Upgraded software whose last vote is still another version: it votes once it proposes again.
  const pending = supports.filter((e) => e.vote !== v);
  const others = u.versions
    .filter(([ver]) => ver !== v)
    .map(([ver, votes]) => ({ label: `Voted for ${ver}`, value: fmt(votes), note: ver === u.current ? "the protocol running now" : null }));
  const note = (u.next_epoch_reported_by ? `${fmt(u.next_epoch_reported_by)} evonodes report which protocol Drive runs next epoch: ${u.next_epoch}. ` : "") +
    "Vote counts are what Drive reports (getProtocolVersionUpgradeState), as two evonodes agreed on it. The table below shows each evonode's vote.";
  const runs = v == null ? null : { label: `Run software for ${v}`, value: fmt(supports.length), note: "as their Drive told our checker" };

  if (v == null) {
    const older = u.versions.some(([ver, votes]) => ver !== u.current && votes > 0);
    return {
      state: "active", version: u.current, title: `Protocol ${u.current} is live`, badge: null,
      lede: older
        ? `No upgrade is being voted on: no evonode has voted for a protocol above ${u.current}, the one Platform runs now.`
        : `No upgrade is being voted on: every vote is for the protocol Platform runs now, ${u.current}.`,
      meter: null, stats: others, note, pending: 0,
    };
  }
  if (locked) {
    return {
      state: "locked_in", version: v, title: `Protocol ${v}: `, badge: { tone: "good", text: "locked in" },
      lede: `Drive has scheduled protocol ${v} for the next epoch. Platform runs ${u.current} until epoch ${fmt(n.epoch)} ${ends}; ` +
        `protocol ${v} starts with epoch ${fmt(n.epoch + 1)}.`,
      meter: null, stats: [runs, ...others], note, pending: 0,
    };
  }
  const met = u.votes >= u.required;
  const needed = u.required_pct != null
    ? `1 + ${u.required_pct}% of ${fmt(u.enabled_evonodes)} enabled, rounded down`
    : `of ${fmt(u.enabled_evonodes)} enabled, as Drive counts them`;
  return {
    state: "voting", version: v, title: `Protocol ${v}: `, badge: { tone: "warning", text: "voting" },
    lede: `Platform runs protocol ${u.current}; evonodes are voting for ${v}. An evonode votes for a protocol version with each block it ` +
      `proposes, from the software it runs; votes count per epoch. If ${fmt(u.required)} of the ${fmt(u.enabled_evonodes)} enabled ` +
      `evonodes have voted for ${v} when epoch ${fmt(n.epoch)} ends (${ends}), protocol ${v} starts with epoch ${fmt(n.epoch + 1)}. ` +
      (met ? "It has enough votes now." : `It needs ${fmt(u.required - u.votes)} more.`),
    meter: { votes: u.votes, required: u.required, enabled: u.enabled_evonodes, met },
    stats: [
      { label: `Voted for ${v}`, value: fmt(u.votes), note: `${pct(u.votes / Math.max(1, u.enabled_evonodes))} of enabled evonodes`, tone: met ? "good" : null },
      { label: "Votes needed", value: fmt(u.required), note: needed },
      runs,
      { label: "Upgraded, vote not counted yet", value: fmt(pending.length), note: "it counts once they propose a block" },
      ...others,
    ],
    note, pending: pending.length,
  };
}
