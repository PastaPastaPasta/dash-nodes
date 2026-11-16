// Window labels and rows for a masternode page (mn.js) when the node has fewer than 30 days of
// history, or registered after the board's six-epoch pace window started. Pure functions, no DOM:
// formatting helpers come in from common.js, and missing fields fall back to the 30-day behaviour
// while a deploy rolls out.

/** A unix timestamp (seconds) or "YYYY-MM-DD" day as "25 Oct" (UTC, en-GB, like common.js's dateTime). */
export function shortDate(when) {
  const d = typeof when === "string" ? new Date(`${when}T00:00:00Z`) : new Date(when * 1000);
  return d.toLocaleDateString("en-GB", { day: "numeric", month: "short", timeZone: "UTC" });
}

/** Whether `platform_rewards` describes a node registered fewer than 30 days ago. */
export const isShort = (p) => p?.history_days != null && p.history_days < 30;

/**
 * The uptime tile's { label, value, foot } from `uptime_daily` (oldest first, up to 90 daily rows).
 * With fewer than 30 daily rows, names the row count and the first day instead of "30 days".
 */
export function uptimeTile(uptimeDaily, { fmt, pct, now = Date.now() }) {
  const rows = (uptimeDaily || []).slice(-30);
  const n = rows.length;
  const ok = rows.reduce((a, d) => a + d.ok, 0);
  const attempts = rows.reduce((a, d) => a + d.attempts, 0);
  const label = n === 0 ? "Uptime" : `Uptime, ${fmt(n)} day${n === 1 ? "" : "s"}`;
  const value = attempts ? pct(ok / attempts) : "–";
  const counts = attempts ? `${fmt(ok)} of ${fmt(attempts)} port checks` : "no checks yet";
  // Rows stop when a node leaves the list: then name the last day too, so the window doesn't read as current.
  const last = n ? rows[n - 1].day : null;
  const ended = last && Date.parse(`${last}T00:00:00Z`) < now - 2 * 86400 * 1000;
  const window = !n ? "" : ended ? `${shortDate(rows[0].day)} – ${shortDate(last)}` : `since ${shortDate(rows[0].day)}`;
  const foot = attempts && n < 30 ? `${counts}, ${window}` : counts;
  return { label, value, foot };
}

/**
 * Replacement text for the "Last 30 days" rank row when the node has fewer than 30 days of history
 * (no meter or comparison is drawn), or null when it should be ranked as usual.
 */
export function rank30Note(p, { fmt }) {
  return isShort(p) ? `Here only ${fmt(p.history_days)} of the last 30 days; ranked once it has 30` : null;
}

/**
 * The window and lifetime rows of "Earned on Platform" as [{ label, value, note }]: drops any
 * "Last D days" row (30, 90, 365) whose D exceeds `history_days`, and labels a < 30-day node's
 * lifetime row "Since <date> (N days)".
 */
export function earnedRows(p, { fmt, amount, duration, now = Date.now() }) {
  const t = p.totals;
  const days = p.history_days;
  const windows = [[30, t.d30], [90, t.d90], [365, t.d365]]
    .filter(([d]) => days == null || d <= days)
    .map(([d, v]) => ({ label: `Last ${d} days`, value: amount(v), note: null }));
  const lifetimeLabel = isShort(p) && p.since != null
    ? `Since ${shortDate(p.since)} (${days >= 2 ? `${fmt(days)} days` : duration(now / 1000 - p.since)})`
    : `Since epoch ${fmt(p.first_epoch)}`;
  return [
    ...windows,
    { label: lifetimeLabel, value: amount(t.lifetime), note: ` · ${fmt(t.epochs_paid)} epochs paid` },
  ];
}

/**
 * The "Platform, a month" estimate row as { value, note }: `value` is null before a whole epoch has
 * run since the node registered, and `note` is the muted text.
 */
export function monthlyRow(p, { amount, lastSpan, duration }) {
  if (p?.monthly_since != null && p?.monthly != null && p?.monthly_basis_days != null) {
    return {
      value: amount(p.monthly),
      note: ` at its pace since ${shortDate(p.monthly_since)} (${duration(p.monthly_basis_days * 86400)})`,
    };
  }
  if (p?.monthly === null) {
    return { value: null, note: "once it has a full epoch" };
  }
  const span = lastSpan(p?.monthly_basis_days);
  return {
    value: amount(p?.monthly),
    note: span ? ` at its pace of ${span}` : " once six epochs have finished",
  };
}

/** The "Missed turns we saw" value text, or null when no turns were seen. */
export function missedTurns(p, { fmt }) {
  const missed30 = p?.turns_30d;
  if (!p?.current?.turns?.turns && !missed30?.turns) return null;
  const window = isShort(p) && p.since != null ? `since ${shortDate(p.since)}` : "last 30 days";
  return `${fmt(missed30?.missed ?? 0)} of ${fmt(missed30?.turns ?? 0)} turns, ${window}`;
}
