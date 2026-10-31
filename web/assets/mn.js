import { api, chrome, currentNet, NETWORKS, el, fmt, pct, href, countryName, flag, place, badge, ago, dateTime, dash, nodeStatus, poseBadge, uptimeKind, tile, card,
  changeText, changeKind, DKG_CHECKS, DKG_OVERALL } from "./common.js";

chrome("masternodes");
const net = currentNet();
const protx = location.pathname.split("/").filter(Boolean).pop();
const root = document.getElementById("main");

// Blockchair covers mainnet only; on testnet values are shown without explorer links.
const explorer = net === "mainnet" ? "https://blockchair.com/dash" : null;
const link = (kind, id, text = id) => (explorer && id ? el("a", { class: "mono", href: `${explorer}/${kind}/${id}`, rel: "noopener", text }) : el("span", { class: "mono", text: text ?? "–" }));

function kv(rows) {
  return el("table", { class: "data kv" }, el("tbody", {}, ...rows.filter(Boolean).map(([k, v]) =>
    el("tr", {}, el("td", { text: k }), el("td", {}, v ?? "–")))));
}

function statusOf(m) {
  return m.removed ? ["none", "Unregistered"] : nodeStatus(m, "Not checked yet");
}

/** Service bits a node advertises (Dash Core's protocol.h), by name. */
const SERVICES = [[0, "Full blocks"], [2, "Bloom filters"], [6, "Compact block filters"], [10, "Limited (recent blocks)"],
  [11, "Compressed headers"], [12, "P2P v2 (encrypted)"]];
function services(bits) {
  if (bits == null) return null;
  const named = SERVICES.filter(([b]) => bits & (2 ** b)).map(([, n]) => n);
  const known = SERVICES.reduce((a, [b]) => a + 2 ** b, 0);
  const other = bits - (bits & known);
  return el("span", { title: `0x${bits.toString(16)}` }, [...named, other ? `other bits 0x${other.toString(16)}` : null].filter(Boolean).join(", ") || "none");
}

const DAY = 86400000;
/** Days the strip covers: since the first check we have (today included), at least a week, at most 90. */
function stripDays(days) {
  const today = new Date().toISOString().slice(0, 10);
  const first = days.length ? days[0].day : today;
  return Math.min(90, Math.max(7, Math.round((Date.parse(`${today}T00:00:00Z`) - Date.parse(`${first}T00:00:00Z`)) / DAY) + 1));
}

function uptimeStrip(days) {
  const byDay = new Map(days.map((d) => [d.day, d]));
  const cells = [];
  const now = new Date();
  const today = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate());
  const span = stripDays(days);
  for (let i = span - 1; i >= 0; i--) {
    const day = new Date(today - i * DAY).toISOString().slice(0, 10);
    const d = byDay.get(day);
    let cls = "", title = `${day}: not checked`;
    if (d) {
      const r = d.ok / d.attempts;
      cls = uptimeKind(r);
      title = `${day}: ${pct(r)} (${d.ok} of ${d.attempts} checks answered)`;
    }
    cells.push(el("div", { class: cls, title, "aria-label": title }));
  }
  return el("div", {},
    el("div", { class: "strip", role: "img", "aria-label": `Daily port-check results, last ${span} days`, style: { gridTemplateColumns: `repeat(${span}, 1fr)` } }, ...cells),
    el("div", { class: "strip-axis" }, el("span", { text: `${span - 1} days ago` }), el("span", { text: "today" })),
    el("div", { class: "legend" },
      el("span", {}, el("i", { style: { background: "var(--good)" } }), "≥ 99% of checks answered"),
      el("span", {}, el("i", { style: { background: "var(--warning)" } }), "90–99%"),
      el("span", {}, el("i", { style: { background: "var(--critical)" } }), "< 90%"),
      el("span", {}, el("i", { style: { background: "var(--none)", boxShadow: "inset 0 0 0 1px var(--ring)" } }), "not checked")));
}

function paymentRows(list) {
  return list.map((p) => el("tr", {},
    el("td", {}, link("block", p.block_hash, fmt(p.height))),
    el("td", { text: dateTime(p.time) }),
    el("td", { class: "r", text: dash(p.amount_duffs) })));
}

async function main() {
  let m;
  try {
    m = await api(`${net}/masternodes/${protx}`);
  } catch (e) {
    if (e.message === "unknown protx") return notFound();
    root.append(el("div", { class: "banner", text: `Couldn't load this masternode: ${e.message}` }));
    return;
  }
  document.title = `DashHQ Radar: masternode ${protx.slice(0, 8)}${net === "testnet" ? " · testnet" : ""}`;
  // One page per masternode and network, on either hostname: name the radar.dashhq.org one.
  document.head.append(el("link", { rel: "canonical", href: `https://radar.dashhq.org${href(`/mn/${protx}`)}` }));
  const [sk, sl] = statusOf(m);
  const banned = m.removed ? null : poseBadge(m);
  root.querySelector("h1").replaceChildren(...[m.type === "Evo" ? "Evonode " : "Masternode ", badge(sk, sl), banned && " ", banned].filter(Boolean));
  document.getElementById("lede").textContent = m.protx;

  const ps = m.payments_summary;
  const tip = ps.indexed?.to;
  const up30 = m.uptime_daily.slice(-30);
  const ok30 = up30.reduce((a, d) => a + d.ok, 0), at30 = up30.reduce((a, d) => a + d.attempts, 0);
  root.append(el("div", { class: "tiles" },
    tile("Status", el("span", {}, badge(sk, sl), banned ? " " : null, m.removed ? null : poseBadge(m)),
      m.last_seen ? `last answered ${ago(m.last_seen)}` : "never answered our crawler"),
    tile("Version", m.version || "–", m.protocol ? `protocol ${m.protocol}` : "no handshake yet"),
    tile("Uptime, 30 days", at30 ? pct(ok30 / at30) : "–", at30 ? `${fmt(ok30)} of ${fmt(at30)} port checks` : "no checks yet"),
    tile("Last paid", m.last_paid_height ? (tip ? `${fmt(tip - m.last_paid_height)} blocks ago` : fmt(m.last_paid_height)) : "never",
      m.last_paid_height ? `block ${fmt(m.last_paid_height)}` : ""),
    earnedTile(m),
  ));

  const dkg = dkgSection(m);
  if (dkg) root.append(dkg);

  // Speed tests come from the Worker's API; the crawler's own server doesn't have them, so the card
  // only appears when there's something to show. Their handshakes also feed the latency row.
  const speed = el("section", { class: "card speed", hidden: true, id: "speed" });
  root.append(speed);
  const latencyCell = el("span", {}, m.latency_ms != null ? `${fmt(m.latency_ms)} ms from our crawler` : "–");
  const crawlerAt = api("status").then((s) => s.crawler_location, () => null);
  api(`${net}/masternodes/${protx}/bench`).then(async (b) => { speedSection(speed, b); bestLatency(latencyCell, m, b, await crawlerAt); }, () => {});

  root.append(el("div", { class: "cols-2" },
    card("Registration", "from the deterministic masternode list",
      kv([
        ["Type", m.type],
        ["Collateral", m.collateral ? link("transaction", m.collateral.split("-")[0], m.collateral) : null],
        ["Collateral address", link("address", m.collateral_address)],
        ["Core P2P address", m.service ? el("span", { class: "mono", text: m.service }) : null],
        m.platform_p2p && ["Platform P2P", el("span", { class: "mono", text: m.platform_p2p })],
        m.platform_https && ["Platform HTTPS", el("span", { class: "mono", text: m.platform_https })],
        m.platform_node_id && ["Platform node ID", el("span", { class: "mono", text: m.platform_node_id })],
        ["Payout address", m.payout_address ? el("span", {}, ...m.payout_address.split(",").flatMap((a, i) => [i ? ", " : "", link("address", a)])) : null],
        m.operator_payout_address && ["Operator payout", link("address", m.operator_payout_address)],
        ["Operator reward", m.operator_reward != null ? `${m.operator_reward}%` : null],
        ["Owner address", m.owner_address ? el("span", {}, ...m.owner_address.split(",").flatMap((a, i) => [i ? ", " : "", link("address", a)])) : null],
        ["Voting address", link("address", m.voting_address)],
        ["Operator public key", el("span", { class: "mono", text: m.pubkey_operator || "–" })],
        ["Registered at block", fmt(m.registered_height)],
        ["Last paid at block", m.last_paid_height ? fmt(m.last_paid_height) : "never"],
        // Banned says it all; otherwise the penalty is what matters (it's what gets a node banned).
        ["PoSe", m.pose_ban_height > 0 ? badge("serious", `Banned at block ${fmt(m.pose_ban_height)}`)
          : m.pose_penalty ? `Penalty ${fmt(m.pose_penalty)}, not banned` : "No penalty"],
        m.pose_revived_height > 0 && ["PoSe revived at", fmt(m.pose_revived_height)],
        m.revocation_reason ? ["Revocation reason", String(m.revocation_reason)] : null,
        m.removed && ["Left the list", dateTime(m.removed)],
      ])),
    card("Network", "what our crawler sees",
      kv([
        ["Latest port check", m.reachable == null ? "not checked in the latest crawl" : badge(m.reachable ? "good" : "critical", m.reachable ? "Answered" : "No answer")],
        m.last_error && ["Error", el("span", { class: "mono", text: m.last_error })],
        ["Last answered", m.last_seen ? `${dateTime(m.last_seen)} (${ago(m.last_seen)})` : "never"],
        ["User agent", el("code", { text: m.user_agent || "–" })],
        ["Protocol", m.protocol ?? "–"],
        ["Services", services(m.services)],
        ["Height it reported", m.start_height != null ? fmt(m.start_height) : null],
        ["Latency", latencyCell],
        !m.removed && m.service ? ["Check now", checkNow(m)] : null,
        ["Location", m.country ? el("span", {}, place(m),
          m.lat != null && !m.removed ? [" · ", el("a", { href: href(`/map?protx=${m.protx}`), text: "show on map" })] : null) : null],
        ["Hosting network", m.asn ? el("a", { href: href(`/masternodes?host=AS${m.asn}`), title: "Masternodes on this network", text: `AS${m.asn} ${m.as_org || ""}` }) : null],
      ])),
  ));

  if (m.platform) root.append(platformSection(m));

  root.append(el("section", { class: "section card" },
    el("div", { class: "card-head" }, el("h2", { text: "Port-check uptime" }),
      el("span", { class: "sub", text: `daily, last ${stripDays(m.uptime_daily)} days (UTC)` })),
    uptimeStrip(m.uptime_daily)));

  root.append(changesSection(m));

  root.append(el("div", { class: "cols-2 section" },
    el("section", {}, el("h2", { text: "Version history", style: { marginBottom: "10px" } }),
      el("div", { class: "table-wrap" }, el("table", { class: "data stack" },
        el("thead", {}, el("tr", {}, el("th", { text: "First seen" }), el("th", { text: "User agent" }), el("th", { class: "r", text: "Protocol" }))),
        el("tbody", {}, ...(m.versions.length ? m.versions.map((v) => labelled(["First seen", "User agent", "Protocol"], el("tr", {},
          el("td", { text: dateTime(v.ts) }), el("td", {}, el("code", { text: v.user_agent })), el("td", { class: "r", text: v.protocol }))))
          : [el("tr", {}, el("td", { colspan: 3, class: "empty", text: "No handshake recorded yet." }))]))))),
    paymentsSection(m)));
}

const CHECK_BADGE = { pass: ["good", "Pass"], fail: ["critical", "Fail"], unknown: ["none", "Unknown"] };

/** What each DKG check found, as [what we saw, how to fix it] (the fix only for failures). */
function dkgTexts(m, d) {
  const { rules } = d;
  const port = m.service?.split(":").pop();
  const noHandshake = "No handshake with our crawler in the past week.";
  const proto = `Protocol ${m.protocol}. Masternodes running ${rules.release} vote nodes below protocol ${rules.min_protocol} bad in DKGs.`;
  const bit = `service bit ${rules.compact_filters_bit}`;
  const streak = rules.reach_failures;
  return {
    protocol: { pass: [`Protocol ${m.protocol}, at least ${rules.min_protocol}.`], fail: [proto, `Upgrade to ${rules.release}.`], unknown: [noHandshake] },
    relay: {
      pass: ["Relays transactions."],
      fail: ["Relay is off, so it won't have the transactions it must sign InstantSend locks for.",
        `Remove blocksonly=1 from dash.conf. ${rules.release} won't start a masternode with it.`],
      // Filters come from the same handshake: known filters mean only the relay flag is missing.
      unknown: [d.checks.filters !== "unknown" ? "Not recorded yet; the next crawl will tell." : noHandshake],
    },
    filters: {
      pass: [`Serves compact block filters (${bit}).`],
      fail: [`Doesn't serve compact block filters (${bit}).`,
        `Set peerblockfilters=1 and blockfilterindex=1, or remove a =0 for either. ${rules.release} won't start a masternode without them.`],
      unknown: [noHandshake],
    },
    reachable: {
      pass: [m.reachable === false ? `Missed our latest port check, but not ${streak} in a row. Core judges repeated failures, not one.`
        : "Answered our latest port check."],
      fail: [`Missed ${streak} or more of our port checks in a row. Members vote against a masternode they fail to connect to more than ${rules.core_max_failed_outbound} times.`,
        port ? `Open port ${port} to inbound connections and keep the node online.` : "Keep the node online and its port open to inbound connections."],
      unknown: ["Not checked in our latest crawl."],
    },
    platform_ban: {
      pass: ["Our dashd knows of no Platform ban. Bans arrive over P2P, so it could miss one."],
      fail: ["Platform has PoSe-banned this evonode.", "Fix its Platform services, then send a ProUpServTx (protx update_service) to lift the ban."],
      unknown: ["Our dashd hasn't said yet."],
    },
  };
}

/**
 * Would quorum members vote this masternode a bad connection in DKGs under the next Core release's
 * rules? One row per check, with a fix for each failure.
 */
function dkgSection(m) {
  const d = m.dkg_readiness;
  if (!d?.rules || m.removed) return null;
  const rules = d.rules;
  const texts = dkgTexts(m, d);
  const rows = DKG_CHECKS.filter(([k]) => d.checks[k]).map(([k, label]) => {
    const result = d.checks[k];
    const [said, fix] = texts[k][result];
    return labelled(["Check", "Result", "Fix"], el("tr", {},
      el("td", { text: label }),
      el("td", {}, badge(...CHECK_BADGE[result]), " ", el("span", { class: "muted", text: said })),
      el("td", { text: fix || "–" })));
  });
  const states = Object.values(d.checks);
  const failing = states.filter((c) => c === "fail").length;
  const lede = el("p", { class: "sub" });
  if (d.overall === "pass") lede.textContent = "Passes every check we can see.";
  else if (d.overall === "unknown") lede.textContent = "Nothing fails, but we can't see every check yet.";
  else if (d.checks.protocol === "fail" && states.filter((c) => c !== "pass").length === 1) {
    lede.textContent = `Only the version check fails. Masternodes running ${rules.release} vote nodes below protocol ${rules.min_protocol} bad in DKGs.`;
  } else {
    lede.textContent = `Fails ${failing} of ${rows.length} checks. Quorum members vote against masternodes that fail these in DKGs, which costs PoSe score.`;
  }
  // How close the network is to the new minimum is what decides when the version rule starts to bite.
  if (d.checks.protocol === "fail") {
    api(`${net}/summary`).then((sum) => {
      const s = sum.dkg_readiness;
      if (s?.masternodes) lede.append(` ${pct(s.checks.protocol.pass / s.masternodes)} of enabled masternodes already run protocol ${rules.min_protocol} or newer.`);
    }, () => {});
  }
  const [kind, text] = DKG_OVERALL[d.overall];
  return el("section", { class: "card dkg" },
    el("div", { class: "card-head" }, el("h2", {}, `Ready for ${rules.release} quorums? `, badge(kind, text)),
      el("span", { class: "sub", text: "an estimate, from what our crawler sees" })),
    lede,
    m.status === "pose_banned" ? el("p", { class: "sub", text: "It's PoSe-banned now, so it isn't picked for quorums until it's revived." }) : null,
    el("div", { class: "table-wrap" }, el("table", { class: "data stack" },
      el("thead", {}, el("tr", {}, el("th", { text: "Check" }), el("th", { text: "Result" }), el("th", { text: "Fix" }))),
      el("tbody", {}, ...rows))),
    el("p", { class: "muted small" },
      "When a DKG starts, each quorum member votes against members it can't reach or that fail these checks on their masternode connections. ",
      `Reachability and Platform bans count already; ${rules.release} raises the minimum protocol and adds the relay and filter checks. `,
      "We see the handshake this node sends our crawler, an ordinary inbound connection, so relay and services can differ only if the operator set something unusual. ",
      "Rules from Dash Core ", el("a", { href: `https://github.com/dashpay/dash/blob/${rules.core_commit}/src/active/dkgsession.cpp`, text: `develop at ${rules.core_commit}` }), "."));
}

/**
 * No masternode with this ProTx on this network. A link without ?net opens on the network picked
 * last, so a shared mainnet link can land on testnet: if the other network has it, go there.
 */
async function notFound() {
  const other = NETWORKS.find((n) => n !== net);
  if (!NETWORKS.includes(new URLSearchParams(location.search).get("net"))) {
    const there = await api(`${other}/masternodes/${protx}`).then(() => true, () => false);
    if (there) {
      const u = new URL(location.href);
      u.searchParams.set("net", other);
      location.replace(u);
      return;
    }
  }
  root.querySelector("h1").textContent = "Masternode not found";
  root.append(el("div", { class: "banner" }, `No ${net} masternode has this ProTx hash. It may have been registered moments ago, or the link may be cut short. `,
    el("a", { href: href(`/masternodes?q=${encodeURIComponent(protx.slice(0, 16))}`), text: "Search the list" }), "."));
}

/** What it earned over the last year, and the yield on its collateral that works out to. */
function earnedTile(m) {
  const r = m.returns;
  if (!r) return tile("Earned, last 365 days", "–", "");
  // While the payments index covers less than a year, say how far it goes back.
  const partial = r.indexed_since && Date.now() / 1000 - r.indexed_since < 365 * 86400;
  const label = partial ? `Earned since ${new Date(r.indexed_since * 1000).toLocaleDateString("en-GB", { dateStyle: "medium", timeZone: "UTC" })}` : "Earned, last 365 days";
  const y = r.annual_yield != null ? `≈ ${pct(r.annual_yield)} a year on its ${m.type === "Evo" ? "4,000" : "1,000"} DASH (last ${r.window_days} days)`
    : m.removed ? "no longer in the list" : "too new to tell its yield";
  return tile(label, dash(r.paid_365d), y);
}

/** Registration changes we've recorded, newest first. */
function changesSection(m) {
  const rows = (m.changes || []).map((c) => labelled(["When", "Block", "Change"], el("tr", {},
    el("td", { text: dateTime(c.ts) }),
    el("td", { class: "r", text: c.height != null ? fmt(c.height) : "–" }),
    el("td", {}, badge(changeKind(c), changeText(c))))));
  return el("section", { class: "section card" },
    el("div", { class: "card-head" }, el("h2", { text: "Registration changes" }),
      el("span", { class: "sub", text: "addresses, keys, PoSe status; recorded since 30 Oct 2026" })),
    el("div", { class: "table-wrap" }, el("table", { class: "data stack" },
      el("thead", {}, el("tr", {}, el("th", { text: "When" }), el("th", { class: "r", text: "Block" }), el("th", { text: "Change" }))),
      el("tbody", {}, ...(rows.length ? rows : [el("tr", {}, el("td", { colspan: 3, class: "empty", text: "No changes since we started recording them." }))])))));
}

/** A button that asks the Worker to handshake with this masternode now, from Cloudflare's edge. */
function checkNow(m) {
  const out = el("span", { class: "muted" });
  const btn = el("button", { class: "btn", type: "button", text: "Check now" });
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    out.replaceChildren(" checking…");
    try {
      const r = await fetch(`/api/v1/${net}/masternodes/${protx}/check`, { method: "POST" });
      const j = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
      out.replaceChildren(" ", j.ok
        ? badge("good", `Answered in ${fmt(j.connect_ms + j.handshake_ms)} ms`)
        : badge("critical", `No answer: ${j.error}`),
      ` from ${j.from}${j.ok ? ` · ${j.user_agent} · height ${fmt(j.height)}` : ""}${j.cached ? ` (checked ${ago(j.checked)})` : ""}`);
    } catch (e) {
      out.replaceChildren(` ${e.message}`);
    } finally {
      btn.disabled = false;
    }
  });
  return el("span", {}, btn, out);
}

/** An evonode's Platform endpoints as our latest check found them, and the last 30 days of checks. */
function platformSection(m) {
  const s = m.platform.latest;
  const box = el("section", { class: "section card" },
    el("div", { class: "card-head" }, el("h2", { text: "Platform" }),
      el("span", { class: "sub", text: s ? `checked ${ago(s.ts)}, every 15 minutes` : "not checked yet" })));
  if (!s) return box;
  const endpoint = (e, okText) => (e.error ? badge("critical", e.error) : badge("good", `${okText} in ${fmt(e.ms)} ms`));
  const days = s.cert.expires != null ? (s.cert.expires - Date.now() / 1000) / 86400 : null;
  const cert = days == null ? null : el("span", {},
    badge(days < 0 ? "critical" : days < 3 ? "warning" : "good", days < 0 ? "Expired" : `Expires in ${days < 2 ? `${Math.max(0, Math.round(days * 24))} h` : `${Math.round(days)} days`}`),
    s.cert.issuer ? ` · ${s.cert.issuer}` : "");
  const wrongNode = s.node_protx && s.node_protx !== m.protx;
  const heightCell = el("span", {}, s.height != null ? fmt(s.height) : "–", s.catching_up ? el("span", { class: "muted", text: " · catching up" }) : null);
  api(`${net}/summary`).then((sum) => {
    const top = sum.platform?.max_height;
    if (top && s.height != null && top - s.height > 2) heightCell.append(el("span", { class: "muted", text: ` · ${fmt(top - s.height)} behind the highest we've seen` }));
  }, () => {});
  box.append(el("div", { class: "cols-2" },
    kv([
      ["Tenderdash P2P", endpoint(s.p2p, "Open")],
      ["DAPI (HTTPS)", wrongNode ? badge("critical", "Answers as a different evonode") : endpoint(s.https, "Answered")],
      ["TLS certificate", cert],
      ["Platform height", heightCell],
    ]),
    kv([
      ["Drive", s.software.drive || "–"],
      ["Tenderdash", s.software.tenderdash || "–"],
      ["DAPI", s.software.dapi || "–"],
      ["Drive protocol", s.drive_protocol.current != null ? `${s.drive_protocol.current} (supports ${s.drive_protocol.latest})` : "–"],
      ["Platform peers", s.peers != null ? fmt(s.peers) : "–"],
    ])));
  const daily = m.platform.daily || [];
  if (daily.length) {
    const at = daily.reduce((a, d) => a + d.attempts, 0), ok = daily.reduce((a, d) => a + d.ok, 0);
    const n = (k, word) => `${fmt(k)} ${word}${k === 1 ? "" : "s"}`;
    box.append(el("p", { class: "muted small", text: `Healthy in ${pct(ok / at)} of ${n(at, "check")} over the last ${n(daily.length, "day")} (both endpoints answered, as this evonode).` }));
  }
  return box;
}

/** Gives each cell its column's name, which stacked tables show as the row label on phones. */
function labelled(heads, tr) {
  let i = 0;
  for (const td of tr.children) {
    td.dataset.label ||= heads[i];
    i += Number(td.getAttribute("colspan") || 1);
  }
  return tr;
}

const GRADE_KIND = { A: "good", B: "good", C: "warning", D: "serious", F: "critical" };
const ms = (v) => (v == null ? "–" : `${fmt(v)} ms`);
const secs = (v) => (v == null ? "–" : `${v < 10 ? v.toFixed(1) : fmt(v)} s`);
const SPEED_HEADS = ["Location", "Tested", "Ping", "Connect", "50k headers", "Headers/s", "MN list", "Grade"];
const SPEED_DIMS = [["completeness", "Served everything asked"], ["throughput", "Header throughput"], ["reliability", "No stalls or bad data"],
  ["latency", "Ping"], ["connectivity", "Connect and handshake"]];
// The newest run decides: a node that stopped answering must not keep showing an old grade.
const latestResult = (v) => v.results[0];
const score1 = (x) => (Math.round(x * 10) / 10).toFixed(1);

/** One table row for a test location's latest result. */
function speedRow(v) {
  const r = latestResult(v);
  const g = r.grade;
  return labelled(SPEED_HEADS, el("tr", {},
    el("td", { title: v.location || "" }, v.location || v.vantage),
    el("td", { title: dateTime(r.ts), text: ago(r.ts) }),
    r.ok
      ? [el("td", { class: "r", text: ms(r.ping_ms?.median) }),
        el("td", { class: "r", text: ms(r.connect_ms != null && r.handshake_ms != null ? r.connect_ms + r.handshake_ms : r.connect_ms) }),
        el("td", { class: "r", title: r.headers ? `${fmt(r.headers.served)} of ${fmt(r.headers.requested)} headers${r.headers.valid ? "" : ", failed validation"}` : "",
          text: r.headers ? `${secs(r.headers.seconds)}${r.headers.served < r.headers.requested ? " (incomplete)" : ""}` : "–" }),
        el("td", { class: "r", text: r.headers?.per_sec != null ? fmt(r.headers.per_sec) : "–" }),
        el("td", { class: "r", text: r.mnlistdiff?.ok ? secs(r.mnlistdiff.seconds) : "–" })]
      : el("td", { colspan: 5, class: "muted", "data-label": "Result", text: `Test failed: ${r.error || "no answer"}` }),
    el("td", { class: "r" }, g ? badge(GRADE_KIND[g.letter], `${g.letter} · ${score1(g.score)}`) : badge("critical", "Failed"))));
}

/** Speed tests from each vantage point: grade, rank and the measurements behind them. */
function speedSection(box, b) {
  const tested = b.vantages.filter((v) => v.results.length);
  const head = el("div", { class: "card-head" }, el("h2", { text: "Speed test" }),
    el("span", { class: "sub", text: "every two weeks, from each test location" }));
  if (!tested.length) {
    box.replaceChildren(head, el("p", { class: "sub", text: "Not tested yet. Every enabled masternode is tested once every two weeks." }));
    box.hidden = false;
    return;
  }
  const c = b.combined;
  const ranks = tested.map((v) => v.percentile).filter((x) => x != null);
  const rank = ranks.length ? ranks.reduce((a, x) => a + x, 0) / ranks.length : null;
  const summary = el("div", { class: "speed-summary" },
    el("div", { class: `grade grade-${c?.letter || "none"}`, "aria-label": c ? `Grade ${c.letter}` : "No grade" }, c?.letter || "–"),
    el("div", {},
      el("p", { class: "speed-score" }, c ? el("b", { class: "num", text: score1(c.score) }) : "–", " out of 100",
        c && c.vantages > 1 ? ` (average of ${c.vantages} locations)` : ""),
      rank != null ? el("p", { class: "sub", text: `Better than ${pct(rank, 0)} of masternodes tested from the same location${ranks.length > 1 ? "s" : ""}.` }) : null,
      c ? null : el("p", { class: "sub", text: "The latest test failed; it's retried within a day." })));
  // The breakdown is one location's latest run; say which when there are several.
  const firstV = tested.find((v) => latestResult(v).grade);
  const first = firstV && latestResult(firstV).grade;
  const dims = first ? el("div", { class: "speed-dims" }, ...SPEED_DIMS.filter(([k]) => first[k] != null).map(([k, label]) =>
    el("div", {}, el("span", { text: label }), el("span", { class: "track" }, el("span", { style: { width: `${first[k]}%` } })),
      el("span", { class: "num", text: Math.round(first[k]) })))) : null;
  const dimsNote = first && tested.length > 1 ? el("p", { class: "muted small", text: `Breakdown from ${firstV.location || firstV.vantage}.` }) : null;
  box.replaceChildren(head, el("div", { class: "speed-grid" }, el("div", {}, summary, dims, dimsNote),
    el("div", { class: "table-wrap" }, el("table", { class: "data stack" },
      el("thead", {}, el("tr", {}, ...SPEED_HEADS.map((h, i) =>
        el("th", { class: i > 1 ? "r" : null, text: h })))),
      el("tbody", {}, ...tested.map(speedRow))))),
    el("p", { class: "muted small" }, "Each test downloads the latest 50,000 block headers and the masternode list from this node and checks them against our own node. Grades use the scoring of ",
      el("a", { href: "https://github.com/dashpay/spv-network-health", text: "spv-network-health" }), "."));
  box.hidden = false;
}

/**
 * The best handshake latency we've measured, and where from: the crawler's latest one and each test
 * location's latest successful test (a location whose test failed simply doesn't count).
 */
function bestLatency(cell, m, b, crawlerAt) {
  const seen = [];
  if (m.latency_ms != null) seen.push({ ms: m.latency_ms, from: crawlerAt ? `${crawlerAt}, our crawler` : "our crawler", when: m.last_seen });
  for (const v of b.vantages) {
    // Only a location's newest test: one that failed since doesn't count with an older result.
    const r = v.results[0]?.ok && v.results[0].connect_ms != null ? v.results[0] : null;
    if (r) seen.push({ ms: r.connect_ms + (r.handshake_ms || 0), from: v.location || v.vantage, when: r.ts });
  }
  if (!seen.length) return;
  seen.sort((a, b) => a.ms - b.ms);
  const best = seen[0];
  cell.replaceChildren(`${fmt(best.ms)} ms from ${best.from}`,
    seen.length > 1 ? el("span", { class: "muted", text: ` · best of ${seen.length}` }) : null);
  cell.title = seen.map((s) => `${s.from}: ${fmt(s.ms)} ms (${ago(s.when)})`).join("\n");
}

const PAYMENTS_SHOWN = 10;

function paymentsSection(m) {
  const tbody = el("tbody", {}, ...paymentRows(m.payments.slice(0, PAYMENTS_SHOWN)));
  const ps = m.payments_summary;
  // The first page came with the masternode; show part of it, then the rest, then ask for more.
  let pending = m.payments.slice(PAYMENTS_SHOWN);
  let last = m.payments.at(-1)?.height;
  let exhausted = m.payments.length < 25;
  const more = el("button", { class: "btn", type: "button", text: "Show more", hidden: !pending.length && exhausted });
  more.addEventListener("click", async () => {
    if (pending.length) {
      tbody.append(...paymentRows(pending));
      pending = [];
      more.hidden = exhausted;
      return;
    }
    more.disabled = true;
    try {
      const r = await api(`${net}/masternodes/${protx}/payments?before=${last}&limit=50`);
      tbody.append(...paymentRows(r.payments));
      last = r.payments.at(-1)?.height ?? last;
      exhausted = r.payments.length < 50;
      more.hidden = exhausted;
      more.textContent = "Show more";
    } catch (e) {
      more.textContent = `Retry (${e.message})`;
    } finally {
      more.disabled = false;
    }
  });
  const note = ps.indexed ? `indexed blocks ${fmt(ps.indexed.from)}–${fmt(ps.indexed.to)}` : "payment index not started yet";
  // How often it gets paid lately: the spacing of its latest payment rounds. An evonode is paid several
  // blocks in a row, so a run of consecutive blocks counts as one round.
  const rounds = m.payments.filter((p, i, all) => i === 0 || all[i - 1].height - p.height > 1).slice(0, 6);
  const every = rounds.length > 2 ? (rounds[0].time - rounds.at(-1).time) / (rounds.length - 1) / 86400 : null;
  const lede = ps.count
    ? `${fmt(ps.count)} payments, ${dash(ps.total_duffs)} in total${every ? `; lately one every ${every < 2 ? every.toFixed(1) : Math.round(every)} days` : ""}.`
    : null;
  return el("section", {}, el("div", { class: "card-head" }, el("h2", { text: "Payments" }), el("span", { class: "sub", text: note })),
    lede ? el("p", { class: "sub", text: lede }) : null,
    el("div", { class: "table-wrap" }, el("table", { class: "data" },
      el("thead", {}, el("tr", {}, el("th", { text: "Block" }), el("th", { text: "Time" }), el("th", { class: "r", text: "Amount" }))),
      m.payments.length ? tbody : el("tbody", {}, el("tr", {}, el("td", { colspan: 3, class: "empty", text: "No payments in the indexed range." }))))),
    el("div", { class: "pager" }, more));
}

main();
