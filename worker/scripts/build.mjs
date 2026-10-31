// Copies the site (../web) into public/ for Workers static assets, with the headers the Rust server
// sets on its own pages.
import { cpSync, existsSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";

const out = new URL("../public/", import.meta.url);
rmSync(out, { recursive: true, force: true });
cpSync(new URL("../../web/", import.meta.url), out, { recursive: true });
iconSet();
// Cloudflare Web Analytics, if the zone has it on, injects its beacon; that is the only outside source.
const csp = "default-src 'self'; script-src 'self' https://static.cloudflareinsights.com; style-src 'self'; img-src 'self' data:; " +
  "connect-src 'self' https://cloudflareinsights.com; " +
  "worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
// Assets aren't content-hashed, so lifetimes stay short enough that a deploy shows up within a day;
// each specific block detaches the default Cache-Control rather than adding a second one.
writeFileSync(new URL("_headers", out), `/*
  Content-Security-Policy: ${csp}
  X-Content-Type-Options: nosniff
  Referrer-Policy: strict-origin-when-cross-origin
  Cache-Control: public, max-age=300

/assets/vendor/*
  ! Cache-Control
  Cache-Control: public, max-age=86400

/assets/fonts/*
  ! Cache-Control
  Cache-Control: public, max-age=86400

/assets/map/*
  ! Cache-Control
  Cache-Control: public, max-age=86400
`);
console.log("built public/");

/**
 * The icon slot (web/assets/icons/README.md). Once every file of the set is there, each page's
 * favicon.svg link and TODO(icon) comment become links to the set (plus og:image on pages with Open
 * Graph tags), the manifest lists the PNGs and /favicon.ico is served. Until then nothing points at
 * a missing file; part of a set fails the build.
 */
function iconSet() {
  const SITE = "https://radar.dashhq.org";
  const FILES = ["favicon.ico", "favicon-16.png", "favicon-32.png", "apple-touch-icon.png", "icon-192.png", "icon-512.png", "og.png"];
  const dir = new URL("assets/icons/", out);
  const missing = FILES.filter((f) => !existsSync(new URL(f, dir)));
  if (missing.length === FILES.length) return;
  if (missing.length) throw new Error(`web/assets/icons/ lacks ${missing.join(", ")}: add the whole set (tools/icons.sh) or none`);

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
    let html = readFileSync(file, "utf8");
    const hasOg = html.includes('property="og:site_name"');
    const slot = /  <link rel="icon" href="\/assets\/favicon.svg" type="image\/svg\+xml">\n  <!-- TODO\(icon\):[\s\S]*?-->\n/;
    if (!slot.test(html)) throw new Error(`${name}: no icon slot`);
    html = html.replace(slot, [...links, ...(hasOg ? og : [])].map((l) => `  ${l}\n`).join(""))
      .replace('<meta name="twitter:card" content="summary">', '<meta name="twitter:card" content="summary_large_image">');
    writeFileSync(file, html);
  }
  const manifest = new URL("assets/site.webmanifest", out);
  const m = JSON.parse(readFileSync(manifest, "utf8"));
  m.icons = [192, 512].map((s) => ({ src: `/assets/icons/icon-${s}.png`, sizes: `${s}x${s}`, type: "image/png" }));
  writeFileSync(manifest, `${JSON.stringify(m, null, 2)}\n`);
  // For whatever asks for /favicon.ico without reading the page.
  cpSync(new URL("favicon.ico", dir), new URL("favicon.ico", out));
  console.log("icon set linked");
}
