// "Check my node now": a Dash P2P version handshake from Cloudflare's edge with a registered
// masternode's core P2P address, as the crawler does (src/p2p.rs). The message encoding and parsing are
// pure and tested in Node; the socket comes from the caller (cloudflare:sockets in the Worker).

export const MAGIC = { mainnet: [0xbf, 0x0c, 0x6b, 0xbd], testnet: [0xce, 0xe2, 0xca, 0xff] };
const PROTOCOL = 70242;
const UA = "/dash-nodes:check(nodes.dashhq.org)/";
const MAX_PAYLOAD = 64 * 1024;
const TIMEOUT_MS = 8000;

async function sha256d(bytes) {
  const once = await crypto.subtle.digest("SHA-256", bytes);
  return new Uint8Array(await crypto.subtle.digest("SHA-256", once));
}

/** A P2P message: magic, command (12 bytes, zero-padded), payload length, checksum, payload. */
export async function encode(net, command, payload) {
  const out = new Uint8Array(24 + payload.length);
  out.set(MAGIC[net], 0);
  out.set(new TextEncoder().encode(command), 4);
  new DataView(out.buffer).setUint32(16, payload.length, true);
  out.set((await sha256d(payload)).subarray(0, 4), 20);
  out.set(payload, 24);
  return out;
}

/** "1.2.3.4:9999" or "[2001:db8::1]:9999" → { hostname, port, ip16 } (IPv4 as v4-mapped IPv6). */
export function parseAddress(s) {
  let m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3}):(\d{1,5})$/.exec(s);
  if (m) {
    const ip16 = new Uint8Array(16);
    ip16[10] = ip16[11] = 0xff;
    for (let i = 0; i < 4; i++) ip16[12 + i] = Number(m[i + 1]);
    return { hostname: m.slice(1, 5).join("."), port: Number(m[5]), ip16 };
  }
  m = /^\[([0-9a-fA-F:]+)\]:(\d{1,5})$/.exec(s);
  if (!m) return null;
  const [head, tail = ""] = m[1].split("::");
  const groups = (x) => (x ? x.split(":") : []);
  const h = groups(head), t = groups(tail);
  if (h.length + t.length > 8 || (!m[1].includes("::") && h.length !== 8)) return null;
  const all = [...h, ...Array(8 - h.length - t.length).fill("0"), ...t];
  const ip16 = new Uint8Array(16);
  all.forEach((g, i) => { const v = parseInt(g, 16); ip16[2 * i] = v >> 8; ip16[2 * i + 1] = v & 0xff; });
  return { hostname: m[1], port: Number(m[2]), ip16 };
}

/** Our version message: no services, the node's address as addr_recv, no relay (see src/p2p.rs). */
export function versionPayload(addr) {
  const ua = new TextEncoder().encode(UA);
  const p = new Uint8Array(81 + ua.length + 5);
  const dv = new DataView(p.buffer);
  dv.setInt32(0, PROTOCOL, true);
  dv.setBigInt64(12, BigInt(Math.floor(Date.now() / 1000)), true);
  // addr_recv: services (8 zero bytes), IPv6, port big-endian. addr_from stays all zeros.
  p.set(addr.ip16, 28);
  dv.setUint16(44, addr.port, false);
  p.set(crypto.getRandomValues(new Uint8Array(8)), 72);
  p[80] = ua.length;
  p.set(ua, 81);
  // start_height 0 and fRelay false: the trailing five bytes stay zero.
  return p;
}

/** Splits a byte stream into messages for `net`. */
export class Messages {
  constructor(net) { this.magic = MAGIC[net]; this.buf = new Uint8Array(0); }
  push(chunk) {
    const b = new Uint8Array(this.buf.length + chunk.length);
    b.set(this.buf); b.set(chunk, this.buf.length);
    this.buf = b;
  }
  /** The next whole message, or null until more bytes arrive. Throws on garbage. */
  next() {
    if (this.buf.length < 24) return null;
    if (this.magic.some((x, i) => this.buf[i] !== x)) throw new Error("not a Dash node (wrong network magic)");
    const len = new DataView(this.buf.buffer, this.buf.byteOffset).getUint32(16, true);
    if (len > MAX_PAYLOAD) throw new Error("message too large");
    if (this.buf.length < 24 + len) return null;
    const command = new TextDecoder().decode(this.buf.subarray(4, 16)).replace(/\0+$/, "");
    const payload = this.buf.slice(24, 24 + len);
    this.buf = this.buf.slice(24 + len);
    return { command, payload };
  }
}

/** The node's version message: protocol, services (hex), user agent, start height. */
export function parseVersion(p) {
  if (p.length < 81) throw new Error("bad version message");
  const dv = new DataView(p.buffer, p.byteOffset, p.byteLength);
  let at = 80;
  let n = p[at++];
  if (n === 0xfd) { n = dv.getUint16(at, true); at += 2; } else if (n > 0xfd) throw new Error("bad version message");
  if (at + n + 4 > p.length) throw new Error("bad version message");
  const userAgent = new TextDecoder().decode(p.subarray(at, at + n));
  return {
    protocol: dv.getInt32(0, true),
    services: dv.getBigUint64(4, true).toString(16),
    user_agent: userAgent,
    height: dv.getInt32(at + n, true),
  };
}

/**
 * Handshakes with `address` (see parseAddress) on `net` through `connect` (cloudflare:sockets).
 * Resolves to { ok, connect_ms, handshake_ms, protocol, services, user_agent, height } or
 * { ok: false, error, connect_ms? }; never rejects.
 */
export async function checkNode(connect, net, address, timeoutMs = TIMEOUT_MS) {
  const addr = parseAddress(address);
  if (!addr) return { ok: false, error: "only IPv4 and IPv6 addresses can be checked" };
  const started = Date.now();
  let timer;
  const deadline = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("no answer within 8 s")), timeoutMs); });
  let socket, connectMs;
  try {
    socket = connect({ hostname: addr.hostname, port: addr.port });
    await Promise.race([socket.opened, deadline]);
    connectMs = Date.now() - started;
    const writer = socket.writable.getWriter();
    await Promise.race([writer.write(await encode(net, "version", versionPayload(addr))), deadline]);
    const reader = socket.readable.getReader();
    const messages = new Messages(net);
    for (;;) {
      const { value, done } = await Promise.race([reader.read(), deadline]);
      if (done) throw new Error("the node closed the connection");
      messages.push(value);
      for (let m = messages.next(); m; m = messages.next()) {
        if (m.command === "version") {
          return { ok: true, connect_ms: connectMs, handshake_ms: Date.now() - started - connectMs, ...parseVersion(m.payload) };
        }
      }
    }
  } catch (e) {
    return { ok: false, error: connectMs == null ? `connect: ${e.message}` : e.message, connect_ms: connectMs ?? null };
  } finally {
    clearTimeout(timer);
    socket?.close().catch(() => {});
  }
}
