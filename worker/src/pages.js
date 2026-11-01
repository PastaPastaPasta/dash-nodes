// The site's page headers, in one place: scripts/build.mjs writes them into _headers for the static
// assets, and the Worker sets them itself on the masternode pages it serves (see mnPage).

const DIRECTIVES = {
  "default-src": ["'self'"],
  // Cloudflare Web Analytics, if the zone has it on, injects its beacon; that is the only outside source.
  "script-src": ["'self'", "https://static.cloudflareinsights.com"],
  "style-src": ["'self'"],
  "img-src": ["'self'", "data:"],
  "connect-src": ["'self'", "https://cloudflareinsights.com"],
  "worker-src": ["'self'"],
  "frame-ancestors": ["'none'"],
  "base-uri": ["'none'"],
  "form-action": ["'none'"],
};
const policy = (d) => Object.entries(d).map(([k, v]) => `${k} ${v.join(" ")}`).join("; ");
const adding = (extra) => Object.fromEntries(Object.entries(DIRECTIVES).map(([k, v]) => [k, [...v, ...(extra[k] || [])]]));

/** Every page's Content-Security-Policy. */
export const CSP = policy(DIRECTIVES);
/**
 * Masternode pages' policy: they load the Dash Platform SDK, which runs WebAssembly (in a worker it
 * starts from a blob) and asks evonodes directly, by IP over HTTPS (any of them, and the list
 * changes), plus quorums.*.networks.dash.org for quorum keys.
 */
export const MN_CSP = policy(adding({ "script-src": ["'wasm-unsafe-eval'"], "connect-src": ["https:"], "worker-src": ["blob:"] }));

/**
 * Headers every page gets. Assets aren't content-hashed, so lifetimes stay short enough that a deploy
 * shows up within a day.
 */
export const PAGE_HEADERS = {
  "Content-Security-Policy": CSP,
  "X-Content-Type-Options": "nosniff",
  "Referrer-Policy": "strict-origin-when-cross-origin",
  "Cache-Control": "public, max-age=300",
};

/** A masternode page's path: /mn/<ProTx hash>. */
export const MN_PAGE = /^\/mn\/[0-9a-fA-F]{64}\/?$/;

/**
 * A masternode page. They share one HTML file (the script reads the ProTx from the path), fetched
 * here from the asset store as /mn. Its headers are set here, with MN_CSP: _headers rules match the
 * path an asset is fetched by, so a /mn/* rule wouldn't apply to it.
 */
export async function mnPage(env, request, url) {
  const asset = await env.ASSETS.fetch(new Request(new URL("/mn", url), request));
  const headers = new Headers(asset.headers);
  for (const [name, value] of Object.entries(PAGE_HEADERS)) headers.set(name, value);
  headers.set("Content-Security-Policy", MN_CSP);
  return new Response(asset.body, { status: asset.status, statusText: asset.statusText, headers });
}
