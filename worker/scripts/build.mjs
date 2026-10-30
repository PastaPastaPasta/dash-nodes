// Copies the site (../web) into public/ for Workers static assets, with the headers the Rust server
// sets on its own pages.
import { cpSync, rmSync, writeFileSync } from "node:fs";

const out = new URL("../public/", import.meta.url);
rmSync(out, { recursive: true, force: true });
cpSync(new URL("../../web/", import.meta.url), out, { recursive: true });
const csp = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; " +
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
