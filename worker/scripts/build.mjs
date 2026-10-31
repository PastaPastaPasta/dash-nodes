// Copies the site (../web) into public/ for Workers static assets, with the headers the Rust server
// sets on its own pages.
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";

const out = new URL("../public/", import.meta.url);
rmSync(out, { recursive: true, force: true });
cpSync(new URL("../../web/", import.meta.url), out, { recursive: true });
iconSet();
const sdk = evoSdk();
// Cloudflare Web Analytics, if the zone has it on, injects its beacon; that is the only outside source.
const csp = "default-src 'self'; script-src 'self' https://static.cloudflareinsights.com; style-src 'self'; img-src 'self' data:; " +
  "connect-src 'self' https://cloudflareinsights.com; " +
  "worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
// Masternode pages load the Dash Platform SDK, which runs WebAssembly (in a worker it starts from a blob)
// and asks evonodes directly, by IP over HTTPS (any of them, and the list changes), plus
// quorums.*.networks.dash.org for quorum keys.
const mnCsp = csp.replace("script-src 'self'", "script-src 'self' 'wasm-unsafe-eval'").replace("connect-src 'self'", "connect-src 'self' https:")
  .replace("worker-src 'self'", "worker-src 'self' blob:");
// Assets aren't content-hashed, so lifetimes stay short enough that a deploy shows up within a day;
// each specific block detaches the default Cache-Control rather than adding a second one.
writeFileSync(new URL("_headers", out), `/*
  Content-Security-Policy: ${csp}
  X-Content-Type-Options: nosniff
  Referrer-Policy: strict-origin-when-cross-origin
  Cache-Control: public, max-age=300

/mn/*
  ! Content-Security-Policy
  Content-Security-Policy: ${mnCsp}

/assets/vendor/*
  ! Cache-Control
  Cache-Control: public, max-age=86400

/assets/vendor/${sdk}/*
  ! Cache-Control
  Cache-Control: public, max-age=31536000, immutable

/assets/fonts/*
  ! Cache-Control
  Cache-Control: public, max-age=86400

/assets/map/*
  ! Cache-Control
  Cache-Control: public, max-age=86400
`);
console.log("built public/");

/**
 * The Dash Platform SDK (@dashevo/evo-sdk, pinned in package.json), served from this origin under a
 * versioned path so it can be cached for good. Workers static assets take files up to 25 MiB.
 */
function evoSdk() {
  const pkg = new URL("../node_modules/@dashevo/evo-sdk/", import.meta.url);
  const { version } = JSON.parse(readFileSync(new URL("package.json", pkg), "utf8"));
  const from = new URL("dist/evo-sdk.module.js", pkg);
  const size = statSync(from).size;
  if (size > 25 * 1024 * 1024) throw new Error(`evo-sdk.module.js is ${size} bytes, over Workers' 25 MiB asset limit`);
  const dir = `evo-sdk-${version}`;
  mkdirSync(new URL(`assets/vendor/${dir}/`, out), { recursive: true });
  cpSync(from, new URL(`assets/vendor/${dir}/evo-sdk.module.js`, out));
  return dir;
}

/**
 * The icon slot (web/assets/icons/README.md): every page has one. Once every file of the set is there,
 * each page's favicon.svg link and TODO(icon) comment become links to the set (plus og:image on pages
 * with Open Graph tags), the manifest lists the PNGs and /favicon.ico is served. Until then nothing
 * points at a missing file; part of a set fails the build.
 */
function iconSet() {
  const SITE = "https://radar.dashhq.org";
  // mark-96.png is the header's logo (assets/common.js), so it has to be there too.
  const FILES = ["favicon.ico", "favicon-16.png", "favicon-32.png", "apple-touch-icon.png", "icon-192.png", "icon-512.png", "og.png", "mark-96.png"];
  const SLOT = /  <link rel="icon" href="\/assets\/favicon.svg" type="image\/svg\+xml">\n  <!-- TODO\(icon\):[\s\S]*?-->\n/;
  const dir = new URL("assets/icons/", out);
  const missing = FILES.filter((f) => !existsSync(new URL(f, dir)));
  if (missing.length && missing.length < FILES.length) {
    throw new Error(`web/assets/icons/ lacks ${missing.join(", ")}: add the whole set (tools/icons.sh) or none`);
  }
  const ready = !missing.length;

  const links = [
    '<link rel="icon" href="/assets/icons/favicon.ico" sizes="16x16 32x32 48x48">',
    '<link rel="icon" href="/assets/icons/favicon-32.png" type="image/png" sizes="32x32">',
    '<link rel="icon" href="/assets/icons/favicon-16.png" type="image/png" sizes="16x16">',
    '<link rel="apple-touch-icon" href="/assets/icons/apple-touch-icon.png">',
  ];
  const og = [
    `<meta property="og:image" content="${SITE}/assets/icons/og.png">`,
    '<meta property="og:image:width" content="1200">',
    '<meta property="og:image:height" content="630">',
    '<meta property="og:image:alt" content="DashHQ Radar">',
  ];
  for (const name of readdirSync(out).filter((f) => f.endsWith(".html"))) {
    const file = new URL(name, out);
    const html = readFileSync(file, "utf8");
    if (!SLOT.test(html)) throw new Error(`${name}: no icon slot (copy the favicon.svg link and TODO(icon) comment from index.html)`);
    if (!ready) continue;
    const hasOg = html.includes('property="og:site_name"');
    writeFileSync(file, html.replace(SLOT, [...links, ...(hasOg ? og : [])].map((l) => `  ${l}\n`).join(""))
      .replace('<meta name="twitter:card" content="summary">', '<meta name="twitter:card" content="summary_large_image">'));
  }
  if (!ready) return;
  const manifest = new URL("assets/site.webmanifest", out);
  const m = JSON.parse(readFileSync(manifest, "utf8"));
  m.icons = [192, 512].map((s) => ({ src: `/assets/icons/icon-${s}.png`, sizes: `${s}x${s}`, type: "image/png" }));
  writeFileSync(manifest, `${JSON.stringify(m, null, 2)}\n`);
  // For whatever asks for /favicon.ico without reading the page.
  cpSync(new URL("favicon.ico", dir), new URL("favicon.ico", out));
  console.log("icon set linked");
}
