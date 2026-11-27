// "When is my evonode's turn?" on /platform: finds an evonode by ProTx hash (or a prefix only it has) or IP
// in the /platform document, and says when it next proposes. Formatting comes in from the page (common.js).

/** "1.2.3.4" from "1.2.3.4:9999", "2001:db8::1" from "[2001:db8::1]:9999". */
export const host = (service) => {
  const s = service.trim().toLowerCase();
  const v6 = s.match(/^\[([^\]]+)\](?::\d+)?$/);
  if (v6) return v6[1];
  return s.split(":").length === 2 ? s.replace(/:\d+$/, "") : s;
};

/** The evonodes a query names: a ProTx hash or its start (at least 4 hex digits), or an IP with or without port. */
export function findEvonodes(evonodes, query) {
  const q = query.trim().toLowerCase();
  if (!q) return [];
  const byIp = evonodes.filter((e) => e.service && host(e.service) === host(q));
  if (byIp.length) return byIp;
  if (!/^[0-9a-f]{4,64}$/.test(q)) return [];
  return evonodes.filter((e) => e.protx.startsWith(q));
}

/** Blocks until an evonode's turn: from the schedule's list, else from its ETA (the crawler's ts + slots × block_secs). */
export function turnsAway(schedule, e) {
  const listed = schedule.next?.find((x) => x.protx === e.protx);
  if (listed) return listed.slots;
  return Math.max(1, Math.round((e.next - schedule.ts) / schedule.block_secs));
}

/**
 * What the lookup shows for a query, as { kind, protx?, service?, text } where kind is "empty", "none",
 * "many", "banned", "idle", "unknown" or "turn".
 * @param f { fmt, inAbout, dateTime } from common.js
 */
export function turnView(doc, query, { fmt, inAbout, dateTime }) {
  if (!query.trim()) return { kind: "empty", text: "" };
  const found = findEvonodes(doc.evonodes, query);
  if (!found.length) return { kind: "none", text: "No evonode has that ProTx hash or IP. Check it against the table below." };
  if (found.length > 1) {
    return { kind: "many", text: `That matches ${fmt(found.length)} evonodes. Type more of the ProTx hash.`, matches: found.map((e) => e.protx) };
  }
  const e = found[0];
  const base = { protx: e.protx, service: e.service };
  if (e.status !== "enabled") return { ...base, kind: "banned", text: "is PoSe-banned, so it gets no turns until it's revived." };
  if (!doc.schedule) return { ...base, kind: "unknown", text: "has no estimate right now: the validator sets haven't been read lately." };
  if (!e.next) return { ...base, kind: "idle", text: "isn't in a validator set that takes turns now, so no turn is due." };
  const n = turnsAway(doc.schedule, e);
  return {
    ...base, kind: "turn", eta: e.next, turns: n,
    text: `proposes ${inAbout(e.next)} (${dateTime(e.next)}), ${n === 1 ? "next in line" : `${fmt(n)} turns from now`}.`,
  };
}
