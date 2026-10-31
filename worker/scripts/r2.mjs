// Writes straight to R2 through its S3-compatible API, so the publisher's uploads don't count as
// Worker requests (the Workers Free plan allows 100k a day; a full republish is ~26k documents).
// AWS Signature Version 4 with node:crypto, so the publisher stays dependency-free.
import { createHash, createHmac } from "node:crypto";

const sha256 = (data) => createHash("sha256").update(data).digest("hex");
const hmac = (key, data) => createHmac("sha256", key).update(data).digest();

/**
 * The `authorization` header for a request: `headers` (lowercase names) must hold `host`,
 * `x-amz-date` (YYYYMMDDTHHMMSSZ) and `x-amz-content-sha256`; all of them are signed.
 */
export function authorization({ method, path, query = "", headers, accessKeyId, secretAccessKey, region = "auto", service = "s3" }) {
  const names = Object.keys(headers).sort();
  // Method, path, query, one "name:value" line per header, a blank line, the header names, the body hash.
  const canonical = [method, path, query, ...names.map((n) => `${n}:${String(headers[n]).trim()}`), "",
    names.join(";"), headers["x-amz-content-sha256"]].join("\n");
  const date = headers["x-amz-date"];
  const scope = `${date.slice(0, 8)}/${region}/${service}/aws4_request`;
  const toSign = ["AWS4-HMAC-SHA256", date, scope, sha256(canonical)].join("\n");
  let key = `AWS4${secretAccessKey}`;
  for (const part of [date.slice(0, 8), region, service, "aws4_request"]) key = hmac(key, part);
  const signature = createHmac("sha256", key).update(toSign).digest("hex");
  return `AWS4-HMAC-SHA256 Credential=${accessKeyId}/${scope}, SignedHeaders=${names.join(";")}, Signature=${signature}`;
}

/** Each path segment percent-encoded as SigV4 wants it (unreserved characters stay). */
export const encodePath = (p) => p.split("/").map((s) => encodeURIComponent(s).replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`)).join("/");

/**
 * A bucket on an R2 S3 endpoint (`https://<account id>.r2.cloudflarestorage.com`). `put` stores
 * `body` under `key` with the content type the Worker's old ingest set, and resolves to the fetch
 * Response (never throws on an HTTP error; the caller decides what to retry).
 */
export function bucket({ endpoint, name, accessKeyId, secretAccessKey }) {
  const base = new URL(endpoint);
  return {
    async put(key, body, contentType = "application/json; charset=utf-8") {
      const path = encodePath(`/${name}/${key}`);
      const headers = {
        host: base.host,
        "content-type": contentType,
        "x-amz-content-sha256": sha256(body),
        "x-amz-date": new Date().toISOString().replace(/[-:]|\.\d{3}/g, ""),
      };
      headers.authorization = authorization({ method: "PUT", path, headers, accessKeyId, secretAccessKey });
      delete headers.host; // fetch sets it
      return fetch(new URL(path, base), { method: "PUT", body, headers });
    },
  };
}
