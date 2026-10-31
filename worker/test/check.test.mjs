import { test } from "node:test";
import assert from "node:assert/strict";
import { encode, parseAddress, versionPayload, parseVersion, Messages, checkNode } from "../src/check.js";

test("addresses: IPv4 maps into IPv6, IPv6 expands, anything else is refused", () => {
  const v4 = parseAddress("1.2.3.4:9999");
  assert.equal(v4.hostname, "1.2.3.4");
  assert.equal(v4.port, 9999);
  assert.deepEqual([...v4.ip16.slice(10)], [0xff, 0xff, 1, 2, 3, 4]);
  const v6 = parseAddress("[2001:db8::1]:19999");
  assert.equal(v6.hostname, "2001:db8::1");
  assert.deepEqual([...v6.ip16], [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
  for (const bad of ["example.com:9999", "abc.onion:9999", "1.2.3.4", "[::1]"]) assert.equal(parseAddress(bad), null, bad);
});

test("a version message round-trips through the stream splitter", async () => {
  const msg = await encode("mainnet", "version", versionPayload(parseAddress("1.2.3.4:9999")));
  assert.deepEqual([...msg.slice(0, 4)], [0xbf, 0x0c, 0x6b, 0xbd]);
  const m = new Messages("mainnet");
  m.push(msg.slice(0, 30));
  assert.equal(m.next(), null); // not all there yet
  m.push(msg.slice(30));
  const got = m.next();
  assert.equal(got.command, "version");
  const v = parseVersion(got.payload);
  assert.equal(v.protocol, 70242);
  assert.equal(v.user_agent, "/dash-nodes:check(radar.dashhq.org)/");
  assert.equal(v.height, 0);
  assert.throws(() => parseVersion(new Uint8Array(40)), /bad version message/);
  const testnet = new Messages("testnet");
  testnet.push(msg);
  assert.throws(() => testnet.next(), /wrong network magic/);
});

/** A socket that answers with `reply` (split in two) once anything is written. */
function fakeConnect(reply) {
  return () => {
    let push;
    const readable = new ReadableStream({ start(c) { push = c; } });
    return {
      opened: Promise.resolve(),
      writable: new WritableStream({ write() { push.enqueue(reply.slice(0, 7)); push.enqueue(reply.slice(7)); } }),
      readable,
      close: async () => {},
    };
  };
}

test("checkNode reports the node's version, and failures as results", async () => {
  const reply = await encode("testnet", "version", versionPayload(parseAddress("5.6.7.8:19999")));
  const ok = await checkNode(fakeConnect(reply), "testnet", "5.6.7.8:19999");
  assert.equal(ok.ok, true);
  assert.equal(ok.user_agent, "/dash-nodes:check(radar.dashhq.org)/");
  assert.ok(ok.connect_ms >= 0 && ok.handshake_ms >= 0);
  const refused = await checkNode(() => ({ opened: Promise.reject(new Error("refused")), close: async () => {} }), "mainnet", "1.2.3.4:9999");
  assert.deepEqual([refused.ok, refused.error], [false, "connect: refused"]);
  const silent = await checkNode(() => ({ opened: Promise.resolve(), writable: new WritableStream(), readable: new ReadableStream(), close: async () => {} }),
    "mainnet", "1.2.3.4:9999", 50);
  assert.equal(silent.ok, false);
  assert.match(silent.error, /no answer/);
  assert.equal((await checkNode(fakeConnect(reply), "mainnet", "x.onion:9999")).ok, false);
});
