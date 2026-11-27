// "When is my evonode's turn?" on /platform: finds an evonode by ProTx hash (or a prefix only it has) or IP
// in the /platform document, and says when it next proposes. Formatting comes in from the page (common.js).

/** IPv6 in its shortest form (2001:0db8:0:0:0:0:0:1 → 2001:db8::1); anything else as it is. */
const canonical = (ip) => {
  if (!ip.includes(":")) return ip;
  try { return new URL(`http://[${ip}]/`).hostname.slice(1, -1); } catch { return ip; }
};

/** "1.2.3.4" from "1.2.3.4:9999", "2001:db8::1" from "[2001:db8::1]:9999". */
export const host = (service) => {
  const s = service.trim().toLowerCase();
  const v6 = s.match(/^\[([^\]]+)\](?::\d+)?$/);
  if (v6) return canonical(v6[1]);
  return canonical(s.split(":").length === 2 ? s.replace(/:\d+$/, "") : s);
};
/** Whether a query names a port: "1.2.3.4:9999" or "[2001:db8::1]:9999". */
const hasPort = (s) => /^\[[^\]]+\]:\d+$/.test(s) || /^[^:]+:\d+$/.test(s);

/** The evonodes a query names: a ProTx hash or its start (at least 4 hex digits, 0x or not), or an IP with or without port. */
export function findEvonodes(evonodes, query) {
  const q = query.trim().toLowerCase().replace(/^0x(?=[0-9a-f])/, "");
  if (!q) return [];
  const byService = evonodes.filter((e) => e.service && (e.service.toLowerCase() === q || (hasPort(q) &&
    host(e.service) === host(q) && e.service.slice(e.service.lastIndexOf(":")) === q.slice(q.lastIndexOf(":")))));
  if (byService.length || hasPort(q)) return byService;
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

/** A schedule older than this was left behind by a stalled crawler or publisher (both refresh it every few minutes). */
const SCHEDULE_STALE = 15 * 60;

/**
 * What the lookup shows for a query, as { kind, protx?, service?, text } where kind is "empty", "none",
 * "many", "banned", "idle", "unknown" or "turn".
 * @param f { fmt, inAbout, dateTime } from common.js, and `now` (unix seconds) to judge the schedule's age by
 */
export function turnView(doc, query, { fmt, inAbout, dateTime, now = Date.now() / 1000 }) {
  if (!query.trim()) return { kind: "empty", text: "" };
  const found = findEvonodes(doc.evonodes, query);
  if (!found.length) return { kind: "none", text: "No evonode has that ProTx hash or IP. Check it against the table below." };
  if (found.length > 1) {
    return { kind: "many", text: `That matches ${fmt(found.length)} evonodes. Type more of the ProTx hash.`, matches: found.map((e) => e.protx) };
  }
  const e = found[0];
  const base = { protx: e.protx, service: e.service };
  if (e.status !== "enabled") return { ...base, kind: "banned", text: "is PoSe-banned, so it gets no turns until it's revived." };
  if (!doc.schedule || now - doc.schedule.ts > SCHEDULE_STALE) return { ...base, kind: "unknown", text: "has no estimate right now: the validator sets haven't been read lately." };
  if (!e.next) return { ...base, kind: "idle", text: "isn't in a validator set that takes turns now, so no turn is due." };
  const n = turnsAway(doc.schedule, e);
  return {
    ...base, kind: "turn", eta: e.next, turns: n,
    text: `proposes ${inAbout(e.next)} (${dateTime(e.next)}), ${n === 1 ? "next in line" : `${fmt(n)} turns from now`}.`,
  };
}
