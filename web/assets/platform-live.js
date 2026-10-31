// An evonode's Platform data straight from Platform, proof-checked in the browser by Dash's SDK
// (@dashevo/evo-sdk, served from /assets/vendor). Loaded by mn.js only on evonode pages, after the
// rest of the page, since the SDK is a 9 MB download.
import { el, fmt, credits, dateTime, ago, badge, kv, labelled } from "./common.js";

const SDK = "/assets/vendor/evo-sdk-5.0.1/evo-sdk.module.js";
/** The withdrawals system contract and its document type. */
const WITHDRAWALS = "4fJLR2GYTPFdomuTVvNy3VRrvWgvkKPzqehEBpNf2nk6";
const STATUS = [["none", "Queued"], ["none", "Pooled"], ["warning", "Broadcast"], ["good", "Complete"], ["serious", "Expired (refunded)"]];
/** Finalized epochs checked against the server's numbers: one SDK call. */
const VERIFY_EPOCHS = 12;

const B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
/** An identity id (the ProTx hash as displayed, hex) in base58, as document queries want it. */
function base58(hex) {
  let n = BigInt(`0x${hex}`), out = "";
  while (n > 0n) { out = B58[Number(n % 58n)] + out; n /= 58n; }
  for (let i = 0; i < hex.length && hex.slice(i, i + 2) === "00"; i += 2) out = `1${out}`;
  return out;
}
const hexOf = (b58) => {
  let n = 0n;
  for (const c of b58) n = n * 58n + BigInt(B58.indexOf(c));
  return n.toString(16).padStart(64, "0");
};

/**
 * What `protx` was paid for one finalized epoch, the way Drive pays it: in identifier order, pool ×
 * blocks / total rounded down, the last proposer also getting what the divisions left.
 */
function payout(info, protx) {
  const pool = BigInt(info.totalProcessingFees) + BigInt(info.totalDistributedStorageFees) + BigInt(info.coreBlockRewards);
  const total = BigInt(info.totalBlocksInEpoch);
  const list = [...info.blockProposers].map(([k, v]) => [hexOf(k), BigInt(v)]).sort((a, b) => (a[0] < b[0] ? -1 : 1));
  let left = pool;
  for (let i = 0; i < list.length; i++) {
    const share = total ? (pool * list[i][1]) / total : 0n;
    left -= share;
    if (list[i][0] === protx) return i === list.length - 1 ? share + left : share;
  }
  return 0n;
}

const message = (e) => e?.message || String(e);

/** Fills `box` with live data for evonode `protx`; `server` is the page's platform_rewards. */
export async function live(box, net, protx, server) {
  const status = el("p", { class: "muted small", text: "Loading Dash's Platform SDK (about 9 MB, once)…" });
  box.replaceChildren(status);
  const started = performance.now();
  let sdk;
  try {
    const { EvoSDK } = await import(SDK);
    sdk = net === "mainnet" ? EvoSDK.mainnetTrusted() : EvoSDK.testnetTrusted();
    status.textContent = "Connecting to evonodes…";
    await sdk.connect();
  } catch (e) {
    status.textContent = `Couldn't start the Platform SDK: ${message(e)}`;
    return;
  }
  status.textContent = "Asking Platform…";
  const cur = await sdk.epoch.current().catch(() => null);
  const [balance, blocks, withdrawals, verified] = await Promise.allSettled([
    sdk.identities.balance(protx),
    cur ? sdk.epoch.evonodesProposedBlocksByIds(cur.index, [protx]) : Promise.reject(new Error("no current epoch")),
    withdrawalsOf(sdk, protx),
    verify(sdk, protx, server),
  ]);
  const ok = () => badge("good", "proof checked");
  const failed = (r) => el("span", { class: "muted", text: `couldn't check: ${message(r.reason)}` });
  const blockCount = blocks.status === "fulfilled" ? Number([...blocks.value.values()][0] ?? 0) : null;
  const rows = [
    ["Claimable now", balance.status === "fulfilled"
      ? el("span", {}, balance.value == null ? "no identity" : credits(Number(balance.value)), " ", ok()) : failed(balance)],
    ["Blocks this epoch", blocks.status === "fulfilled" ? el("span", {}, `${fmt(blockCount)} in epoch ${fmt(cur.index)}`, " ", ok()) : failed(blocks)],
    ["Our epoch numbers", verified.status === "fulfilled" ? verified.value : failed(verified)],
  ];
  const before = earlier(balance, withdrawals, server);
  if (before) rows.push(before);
  const list = withdrawals.status === "fulfilled" ? withdrawalTable(withdrawals.value)
    : el("p", { class: "muted", text: `Couldn't list withdrawals: ${message(withdrawals.reason)}` });
  box.replaceChildren(kv(rows), el("h3", { class: "section", text: "Withdrawals" }), list,
    el("p", { class: "muted small", text: `Checked against Platform's proofs in your browser ${ago(Date.now() / 1000)}, in ${((performance.now() - started) / 1000).toFixed(1)} s. ` +
      "Withdrawals move credits from the masternode identity to its payout address on Core; each costs a small fee." }));
}

/** About what a withdrawal costs on top of its amount (RESEARCH: the median leftover per withdrawal). */
const WITHDRAWAL_FEE = 204_000_000;

/**
 * What it was credited beyond the rewards since Drive's first epoch record, an estimate: its balance
 * plus everything it withdrew (and the fees) minus those rewards. Rewards from before epoch 34 are
 * most of it; credits transferred in would count too.
 */
function earlier(balance, withdrawals, server) {
  if (balance.status !== "fulfilled" || withdrawals.status !== "fulfilled" || balance.value == null || !server?.totals) return null;
  const out = withdrawals.value.filter((w) => w.status !== 4);
  const held = Number(balance.value) + out.reduce((a, w) => a + Number(w.amount) + WITHDRAWAL_FEE, 0);
  const extra = held - server.totals.lifetime;
  if (extra < 5e9) return null; // under 0.05 DASH: fee noise
  return [`Before epoch ${fmt(server.first_epoch)}`, el("span", {}, `≈ ${credits(extra, 2)}`,
    el("span", { class: "muted", text: " (estimate: its balance and withdrawals beyond the rewards since; transfers in would count too)" }))];
}

/** Every withdrawal document this identity created, newest first. */
async function withdrawalsOf(sdk, protx) {
  const out = [];
  let startAfter;
  for (let page = 0; page < 10; page++) {
    const docs = await sdk.documents.query({
      dataContractId: WITHDRAWALS, documentTypeName: "withdrawal", where: [["$ownerId", "==", base58(protx)]],
      orderBy: [["status", "asc"], ["$createdAt", "asc"]], limit: 100, ...(startAfter ? { startAfter } : {}),
    });
    for (const [id, d] of docs) {
      if (d) out.push(d.toJSON());
      startAfter = id;
    }
    if (docs.size < 100) break;
  }
  return out.sort((a, b) => b.$createdAt - a.$createdAt);
}

function withdrawalTable(list) {
  if (!list.length) return el("p", { class: "muted", text: "None yet: everything it earned is still claimable." });
  const done = list.filter((w) => w.status !== 4).reduce((a, w) => a + Number(w.amount), 0);
  const heads = ["Requested", "Amount", "Status", "Updated"];
  return el("div", {}, el("p", { class: "sub", text: `${fmt(list.length)} withdrawal${list.length === 1 ? "" : "s"}, ${credits(done, 2)} in total (not counting expired ones).` }),
    el("div", { class: "table-wrap" }, el("table", { class: "data stack" },
      el("thead", {}, el("tr", {}, ...heads.map((h, i) => el("th", { class: i === 1 ? "r" : null, text: h })))),
      el("tbody", {}, ...list.slice(0, 50).map((w) => {
        const [kind, label] = STATUS[w.status] || ["none", `Status ${w.status}`];
        return labelled(heads, el("tr", {}, el("td", { text: dateTime(w.$createdAt / 1000) }), el("td", { class: "r", text: credits(Number(w.amount)) }),
          el("td", {}, badge(kind, label)), el("td", { text: w.$updatedAt ? dateTime(w.$updatedAt / 1000) : "–" })));
      })))));
}

/** Recomputes the latest finalized epochs' payouts from proven epoch records and compares them with ours. */
async function verify(sdk, protx, server) {
  const ours = new Map((server?.epochs || []).map((e) => [e.epoch, BigInt(e.credits)]));
  const last = server?.totals?.last_epoch_index;
  if (last == null) return el("span", { class: "muted", text: "nothing to check yet" });
  const start = Math.max(server.first_epoch ?? 0, last - VERIFY_EPOCHS + 1);
  const infos = await sdk.epoch.finalizedInfos({ startEpoch: start, count: last - start + 1, ascending: true });
  let checked = 0;
  const differ = [];
  for (const [epoch, info] of infos) {
    if (!info) continue;
    checked++;
    const theirs = payout(info, protx);
    if (theirs !== (ours.get(epoch) ?? 0n)) differ.push(`${epoch} (${credits(Number(theirs))} by the proofs)`);
  }
  if (!checked) return el("span", { class: "muted", text: "no epochs came back to check" });
  return differ.length
    ? el("span", {}, badge("critical", "differ"), ` in epoch${differ.length > 1 ? "s" : ""} ${differ.join(", ")}`)
    : el("span", {}, badge("good", "match"), ` the proofs for epochs ${fmt(start)}–${fmt(last)}`);
}
