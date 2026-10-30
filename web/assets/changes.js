import { api, chrome, currentNet, el, fmt, href, shortHash, badge, ago, dateTime, segmented, CHANGE_KINDS, changeText, changeKind } from "./common.js";

chrome("changes");
const net = currentNet();
const params = new URLSearchParams(location.search);
let kind = CHANGE_KINDS.some(([k]) => k === params.get("kind")) ? params.get("kind") : "";
let changes = [];

function render() {
  const match = CHANGE_KINDS.find(([k]) => k === kind)?.[2];
  const list = match ? changes.filter((c) => match(c.field)) : changes;
  const heads = ["When", "Masternode", "Change"];
  const row = (c) => {
    const tr = el("tr", {},
      el("td", { title: dateTime(c.ts), text: `${ago(c.ts)}${c.height != null ? ` · block ${fmt(c.height)}` : ""}` }),
      el("td", {}, el("a", { class: "mono", href: href(`/mn/${c.protx}`), text: shortHash(c.protx) }),
        c.type === "Evo" ? [" ", el("span", { class: "pill evo", text: "Evo" })] : null),
      el("td", {}, badge(changeKind(c), changeText(c))));
    [...tr.children].forEach((td, i) => { td.dataset.label = heads[i]; });
    return tr;
  };
  document.getElementById("changes-table").replaceChildren(
    el("thead", {}, el("tr", {}, ...heads.map((h) => el("th", { text: h })))),
    el("tbody", {}, ...(list.length ? list.map(row)
      : [el("tr", {}, el("td", { colspan: 3, class: "empty", text: changes.length ? "No changes of this kind lately." : "No changes recorded yet." }))])));
  const u = new URL(location.href);
  if (kind) u.searchParams.set("kind", kind); else u.searchParams.delete("kind");
  history.replaceState(null, "", u);
}

async function main() {
  document.getElementById("filters").replaceChildren(
    segmented([["", "Everything"], ...CHANGE_KINDS.map(([k, label]) => [k, label])], kind, (v) => { kind = v; render(); }, "Kind of change"));
  try {
    ({ changes } = await api(`${net}/changes`));
    document.getElementById("lede").textContent =
      `The latest ${fmt(changes.length)} changes to masternode registrations, newest first: new and removed masternodes, PoSe bans and revivals, changed addresses and keys.`;
  } catch (e) {
    document.getElementById("lede").textContent = `Couldn't load the changes: ${e.message}`;
  }
  render();
}

main();
