// SPDX-License-Identifier: EUPL-1.2
// The call page's link handling, admission, worker messages, SFU message bounds and
// codec support. STREAM_WASM names the wasm-bindgen build of crates/stream-wasm
// (nix build .#web-stream-wasm); the admission tests run the real stream worker.
import assert from "node:assert/strict";
import { createHmac, createHash, hkdfSync, randomBytes } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join as joinPath } from "node:path";
import { test } from "node:test";
import { pathToFileURL } from "node:url";
import { unsized } from "./frames.js";
import { canShareAudio } from "./controls.js";
import { publisherMeasurements, receiverRates } from "./receiver-stats.js";
import { Call } from "./session.js";
import { MAX_MESSAGE, MAX_PEERS, TIERS, fitTier, join, linkOf, parse, qualityRequest } from "./signal.js";
import { preferences, scalabilityMode, sendCodecs, support } from "./support.js";
import { View } from "./view.js";

const directory = process.env.STREAM_WASM;
assert.ok(directory, "STREAM_WASM must name the stream worker's wasm-bindgen build");
const { initSync, StreamWorker } = await import(pathToFileURL(joinPath(directory, "nuschl_stream_wasm.js")).href);
initSync({ module: await readFile(joinPath(directory, "nuschl_stream_wasm_bg.wasm")) });

// The key schedule of apps/stream-sfu/src/protocol.rs, independently of the worker.
const hkdf = (secret, label, length) =>
  Buffer.from(hkdfSync("sha256", secret, "nuschl/stream/v2", `nuschl/stream/v2 ${label}`, length));
const mac = (key, label, nonce) =>
  createHmac("sha256", key).update(`nuschl/stream/v2 sfu ${label}`).update(nonce).digest("hex");
const b64 = (bytes) => bytes.toString("base64url");
const S = randomBytes(32), P = randomBytes(32);
const hostKey = hkdf(P, "host", 32);
const V = createHash("sha256").update(hostKey).digest();
const host = "stream.example";
const location = (href) => {
  const url = new URL(href);
  return { origin: url.origin, pathname: url.pathname, search: url.search, hash: url.hash, host: url.host };
};
const request = (worker, body) => JSON.parse(worker.handle(JSON.stringify({ v: 1, ...body }), Date.now()));

test("creating a browser call returns only the guest invitation, with host admission in the worker", () => {
  const worker = new StreamWorker();
  const opened = request(worker, { kind: "create", host, allow_direct: false });
  assert.equal(opened.creator, true);
  const link = new URL(opened.invitation);
  assert.equal(link.host, host);
  assert.equal(link.pathname, "/call");
  assert.deepEqual([...new URLSearchParams(link.hash.slice(1)).keys()], ["v", "k", "h"]);
  const joined = request(worker, { kind: "admit", nonce: "0".repeat(64) });
  assert.ok(joined.create.host_key);
  assert.ok(joined.host_proof);
  const guest = new StreamWorker();
  assert.equal(request(guest, { kind: "open", host, link: opened.invitation }).creator, false);
  const guestAdmission = request(guest, { kind: "admit", nonce: "0".repeat(64) });
  assert.equal(guestAdmission.create, null);
  assert.equal(guestAdmission.host_proof, null);
  assert.equal(request(worker, { kind: "admit", nonce: "0".repeat(64) }).create.host_key, joined.create.host_key);
});

test("audio capture requires a browser tab and excludes call tabs", () => {
  const capture = (displaySurface, handle) => ({
    getSettings: () => ({ displaySurface }),
    getCaptureHandle: () => handle,
  });
  assert.equal(canShareAudio(capture("browser", null)), true);
  assert.equal(canShareAudio(capture("browser", { handle: "nuschl-call" })), false);
  assert.equal(canShareAudio(capture("monitor", null)), false);
  assert.equal(canShareAudio(capture("window", null)), false);
  assert.equal(canShareAudio({ getSettings: () => ({ displaySurface: "browser" }) }), false);
});

test("the page hands the worker the call link and reads relay-only from the query", () => {
  const fragment = `#v=1&k=${b64(S)}&h=${b64(V)}`;
  assert.deepEqual(linkOf(location(`https://${host}/call${fragment}`)), {
    link: `https://${host}/call${fragment}`, host, relay: false,
  });
  assert.deepEqual(linkOf(location(`https://${host}:8443/call?relay${fragment}`)), {
    link: `https://${host}:8443/call${fragment}`, host: `${host}:8443`, relay: true,
  });
});

test("admission: the worker's room, verifier and proofs follow the SFU's key schedule", () => {
  const worker = new StreamWorker();
  const { link } = linkOf(location(`https://${host}/call?relay#v=1&k=${b64(S)}&h=${b64(V)}`));
  const opened = request(worker, { kind: "open", link, host });
  assert.deepEqual(opened, {
    v: 1, kind: "opened", room: hkdf(S, "stream id", 16).toString("hex"), verifier: V.toString("hex"), creator: false,
  });
  const nonce = randomBytes(32);
  const admitted = request(worker, { kind: "admit", nonce: nonce.toString("hex") });
  assert.equal(admitted.proof, mac(hkdf(S, "admit", 32), "admit", nonce));
  assert.deepEqual(join(opened, admitted, ["av1", "vp9"]), {
    type: "join", room: opened.room, verifier: opened.verifier, proof: admitted.proof,
    host_proof: null, create: null, codecs: ["av1", "vp9"],
  });
  assert.throws(() => request(worker, { kind: "open", link, host }), /already open/);
  worker.free();
});

test("admission: the creator's link proves P and hands over the room's keys", () => {
  const worker = new StreamWorker();
  const link = `https://${host}/call#v=1&k=${b64(S)}&h=${b64(V)}&p=${b64(P)}`;
  const opened = request(worker, { kind: "open", link, host });
  assert.equal(opened.creator, true);
  const nonce = randomBytes(32);
  const message = join(opened, request(worker, { kind: "admit", nonce: nonce.toString("hex") }), ["vp9"]);
  assert.equal(message.host_proof, mac(hostKey, "host", nonce));
  assert.deepEqual(message.create, { admit_key: hkdf(S, "admit", 32).toString("hex"), host_key: hostKey.toString("hex") });
  worker.free();
});

test("fragment handling: the worker refuses links that are not this host's call link", () => {
  const good = `#v=1&k=${b64(S)}&h=${b64(V)}`;
  for (const [link, linkHost] of [
    [`https://${host}/call#v=1&k=${b64(S)}`, host],
    [`https://${host}/call#v=2&k=${b64(S)}&h=${b64(V)}`, host],
    [`https://${host}/call${good}&x=1`, host],
    [`https://${host}/call#v=1&k=${b64(S)}&h=${b64(V)}&p=${b64(S)}`, host],
    [`http://${host}/call${good}`, host],
    [`https://other.example/call${good}`, host],
    [`https://${host}/watch${good}`, host],
    [`https://${host}/call#v=1&k=${b64(S).slice(1)}&h=${b64(V)}`, host],
  ]) {
    const worker = new StreamWorker();
    assert.throws(() => request(worker, { kind: "open", link, host: linkHost }), (error) => typeof error === "string", link);
    assert.throws(() => request(worker, { kind: "admit", nonce: "00".repeat(32) }), /no call link/);
    worker.free();
  }
});

test("worker messages: versions, kinds and bounds are checked before use", () => {
  const worker = new StreamWorker();
  const handle = (text) => () => worker.handle(text, Date.now());
  assert.throws(handle(JSON.stringify({ v: 2, kind: "tick" })), /unsupported version/);
  assert.throws(handle(JSON.stringify({ v: 1, kind: "bogus" })), /malformed/);
  assert.throws(handle("x".repeat(64 * 1024)), /too long/);
  assert.throws(handle(JSON.stringify({ v: 1, kind: "subscribe", peer: 1 })), /no call link/);
  request(worker, { kind: "open", link: `https://${host}/call#v=1&k=${b64(S)}&h=${b64(V)}`, host });
  const subscribed = request(worker, { kind: "subscribe", peer: 7 });
  assert.equal(subscribed.kind, "done");
  assert.equal(subscribed.send.length, 1);
  assert.equal(subscribed.send[0].to, 7);
  assert.match(subscribed.send[0].data, /^([0-9a-f]{2})+$/);
  assert.throws(() => request(worker, { kind: "key", from: 8, data: subscribed.send[0].data }), (e) => typeof e === "string");
  assert.deepEqual(request(worker, { kind: "leave", peer: 7 }), { v: 1, kind: "done", send: [], events: [] });
  assert.throws(() => worker.decrypt(7, new Uint8Array([0x08, 1, 2, 3]), Date.now()), /unknown peer/);
  worker.free();
});

test("SFU messages: the page's bounds", () => {
  const welcome = { type: "welcome", peer: 4, role: "participant", peers: [{ id: 1, role: "host" }], codec: "vp9", turn: null };
  assert.deepEqual(parse(JSON.stringify(welcome)), welcome);
  const turn = { urls: ["turn:turn.example:3478?transport=udp"], username: "1:ab", credential: "c" };
  assert.deepEqual(parse(JSON.stringify({ ...welcome, turn })).turn, turn);
  assert.equal(parse(JSON.stringify({ type: "later", x: 1 })), null, "unknown types are ignored");
  assert.equal(parse(JSON.stringify({ type: "constructor" })), null);
  const offer = { type: "offer", sdp: "v=0", tracks: [{ mid: "0", peer: 1, source: "1", kind: "screen" }] };
  assert.deepEqual(parse(JSON.stringify(offer)), offer);
  for (const bad of [
    { ...welcome, codec: "h264" },
    { ...welcome, peer: -1 },
    { ...welcome, peer: 2 ** 53 },
    { ...welcome, peers: Array.from({ length: MAX_PEERS + 1 }, (_, id) => ({ id, role: "participant" })) },
    { ...welcome, turn: { ...turn, urls: "turn:x" } },
    { type: "hello", nonce: "00".repeat(31) },
    { type: "hello", nonce: "ZZ".repeat(32) },
    { type: "key", from: 1, data: "abc" },
    { type: "key", from: 1, data: "ab".repeat(16 * 1024 + 1) },
    { ...offer, tracks: [{ mid: "0", peer: 1, source: "1" }] },
    { ...offer, tracks: [{ mid: "0", peer: 1, source: "1", kind: "slides" }] },
    { type: "mode", mid: "0", peer: 1, mode: { prefer: "crisp" } },
    { type: "mode", mid: "0", peer: 1, mode: { prefer: "sharp", low_latency: 1 } },
    { type: "mode", mid: "0", peer: 1 },
    { type: "error", reason: 5 },
    [],
    "text",
  ]) {
    assert.throws(() => parse(JSON.stringify(bad)), JSON.stringify(bad).slice(0, 80));
  }
  assert.throws(() => parse(" ".repeat(MAX_MESSAGE + 1)), /too long/);
  assert.throws(() => parse("{"));
});

test("viewer quality uses tile pixels and the v1 request layout", () => {
  assert.equal(fitTier("camera", 300), 1);
  assert.equal(fitTier("camera", 700), 2);
  assert.equal(fitTier("screen", 720), 0);
  assert.equal(fitTier("screen", 1200), 2);
  assert.equal(fitTier("screen", 4000), 3);
  assert.deepEqual(qualityRequest(0x01020304050607, "screen", 3),
    { type: "quality", request: "0100010203040506070103" });
  assert.equal(qualityRequest(23, "camera", 0).request, "0100000000000000170000");
});

test("a receive limit applies before layout and caps the tile's tier", () => {
  class Element extends EventTarget {
    constructor(ownerDocument) {
      super();
      this.ownerDocument = ownerDocument;
      this.children = [];
    }
    setAttribute() {}
    replaceChildren(...children) { this.children = children; }
    append(...children) { this.children.push(...children); }
  }
  const document = { createElement: () => new Element(document) };
  const track = { peer: 4, screen: true };
  const video = {};
  const sent = [];
  const view = {
    document,
    call: { tracks: new Map([["source", track]]), send: (message) => sent.push(message) },
    tiers: new Map(),
    sentTiers: new WeakMap(),
    sizes: new Map(),
    observer: { observe() {} },
  };
  const { control, requested } = View.prototype.viewerQuality.call(view, "source", video, track, "Screen");
  assert.equal(requested(), undefined);
  assert.deepEqual(sent, []);
  const select = control.children[1];
  select.value = "2";
  select.dispatchEvent(new Event("change"));
  assert.equal(requested(), TIERS.screen[2]);
  assert.deepEqual(sent, [{ type: "quality", request: "0100000000000000040102" }]);
  view.sizes.get(video).resize(400);
  assert.equal(requested(), TIERS.screen[0]);
  view.sizes.get(video).resize(4000);
  assert.equal(requested(), TIERS.screen[2]);
  select.value = "";
  select.dispatchEvent(new Event("change"));
  assert.equal(requested(), TIERS.screen[3]);
});

test("negotiated video limits follow only the accepted video track", async () => {
  const call = new Call({ request: async () => ({ proof: "proof" }) }, { codecs: ["av1"] });
  call.opened = { room: "room", verifier: "verifier" };
  const messages = [];
  call.send = (message) => messages.push(message);
  const hello = { type: "hello", nonce: "01".repeat(32) };
  await call.receive(parse(JSON.stringify(hello)));
  assert.equal(messages[0].video_limits, undefined);
  assert.equal(messages[0].screen_rate, undefined);
  const track = { peer: 4, kind: "video" };
  call.tracks.set("1", track);
  const report = { type: "video_limit", mid: "1", peer: 4, limit: "sfu_host" };
  await call.receive(parse(JSON.stringify(report)));
  assert.equal(track.limit, undefined);
  await call.receive(parse(JSON.stringify({ ...hello, video_limits: true, screen_rate: true })));
  assert.equal(messages[1].video_limits, true);
  assert.equal(messages[1].screen_rate, true);
  assert.throws(() => parse(JSON.stringify({ ...hello, screen_rate: 1 })), /malformed hello/);
  await call.receive(parse(JSON.stringify({ ...report, peer: 7 })));
  assert.equal(track.limit, undefined);
  await call.receive(parse(JSON.stringify(report)));
  assert.equal(track.limit, "sfu_host");
  await call.receive(parse(JSON.stringify({ ...report, limit: "within_target" })));
  assert.equal(track.limit, "within_target");
  for (const invalid of [{ peer: 0 }, { mid: "x".repeat(17) }, { limit: "unknown" }]) {
    assert.throws(() => parse(JSON.stringify({ ...report, ...invalid })), /malformed video_limit/);
  }
});

test("publisher quality applies bounded encoder limits to the matching MID", async () => {
  const request = "0100000000000000040100";
  const quality = { type: "quality", request, source: "3", max_height: 720, max_fps: 15, max_bitrate: 800_000 };
  assert.equal(parse(JSON.stringify({ type: "quality", request })), null);
  for (const limits of [{ source: "x".repeat(17) }, { max_height: 0 }, { max_fps: 61 }, { max_bitrate: 1.5 }, { max_bitrate: 50_000_001 }]) {
    assert.throws(() => parse(JSON.stringify({ ...quality, ...limits })), /malformed quality/);
  }

  const call = new Call({}, { codecs: ["av1"] });
  const applied = [];
  const sender = {
    getParameters: () => ({ encodings: [{}] }),
    setParameters: async ({ encodings }) => applied.push(encodings[0]),
  };
  const transceiver = { mid: "3", sender };
  const track = { getSettings: () => ({ width: 3440, height: 1440 }) };
  call.sources.set("screen", { transceiver, track });

  await call.receive(parse(JSON.stringify(quality)));
  assert.deepEqual(applied, [{ scaleResolutionDownBy: 2, maxFramerate: 15, maxBitrate: 800_000 }]);
  transceiver.mid = "4";
  await call.receive(quality);
  assert.equal(applied.length, 1);
  await call.receive({ ...quality, source: "4", max_height: 2160, max_fps: 60, max_bitrate: 15_000_000 });
  assert.deepEqual(applied[1], { scaleResolutionDownBy: 1, maxFramerate: 60, maxBitrate: 15_000_000 });
  await call.setPublishTier("screen", "2");
  assert.deepEqual(applied[2], { scaleResolutionDownBy: 1, maxFramerate: 60, maxBitrate: 8_000_000 });
  await call.setPublishTier("screen", "1");
  assert.deepEqual(applied[3], {
    scaleResolutionDownBy: 1440 / 1080, maxFramerate: 30, maxBitrate: 3_000_000,
  });
  await call.receive({ ...quality, source: "4" });
  assert.deepEqual(applied[4], { scaleResolutionDownBy: 2, maxFramerate: 15, maxBitrate: 800_000 });

  call.screenRate = true;
  await call.receive(parse(JSON.stringify({ ...quality, source: "4", max_height: 2160, max_fps: 60, max_bitrate: 24_000_000 })));
  assert.deepEqual(applied[5], { scaleResolutionDownBy: 1440 / 1080, maxFramerate: 30, maxBitrate: 24_000_000 });
  await call.setPublishTier("screen", "2");
  assert.deepEqual(applied[6], { scaleResolutionDownBy: 1, maxFramerate: 60, maxBitrate: 24_000_000 });
  await call.receive({ ...quality, source: "4", max_height: 1440, max_fps: 60, max_bitrate: 6_000_000 });
  assert.deepEqual(applied[7], { scaleResolutionDownBy: 1, maxFramerate: 60, maxBitrate: 6_000_000 });
  call.sources.set("camera", { transceiver: { mid: "5", sender }, track, tier: "2" });
  await call.receive({ ...quality, source: "5", max_height: 1080, max_fps: 30, max_bitrate: 4_000_000 });
  assert.deepEqual(applied[8], { scaleResolutionDownBy: 2, maxFramerate: 30, maxBitrate: 2_000_000 });
});

test("a source's mode sets its sender's hints and reaches the SFU", async () => {
  const call = new Call({}, { codecs: ["av1"] });
  const messages = [];
  call.send = (message) => messages.push(message);
  const parameters = [];
  const sender = {
    getParameters: () => ({ encodings: [{}] }),
    setParameters: async (value) => parameters.push(value),
  };
  const screen = { getSettings: () => ({ width: 1920, height: 1080 }) };
  call.publisher = { addTransceiver: () => ({ mid: "1", sender }) };
  call.stream = { worker: {} };
  globalThis.RTCRtpScriptTransform = class {};
  const transceiver = call.sender("screen", screen);
  assert.equal(screen.contentHint, "detail");
  call.sources.set("screen", { track: screen, transceiver, tier: "" });
  await call.applyCap("screen", call.sources.get("screen"));
  assert.equal(parameters.at(-1).degradationPreference, "maintain-resolution");
  await call.setMode("screen", { prefer: "smooth", low_latency: true });
  assert.equal(screen.contentHint, "motion");
  assert.equal(parameters.at(-1).degradationPreference, "maintain-framerate");
  assert.deepEqual(messages, [{ type: "modes", modes: { 1: { prefer: "smooth", low_latency: true } } }]);
  // A camera starts Smooth.
  const camera = { getSettings: () => ({}) };
  call.sender("camera", camera);
  assert.equal(camera.contentHint, "motion");
  delete globalThis.RTCRtpScriptTransform;
});

test("a Low latency source's receiver renders at once", async () => {
  const call = new Call({}, { codecs: ["av1"] });
  const receiver = { jitterBufferTarget: null };
  const track = { peer: 4, source: "2", kind: "video", receiver };
  call.tracks.set("1", track);
  const mode = { type: "mode", mid: "1", peer: 4, mode: { prefer: "smooth", low_latency: true } };
  await call.receive(parse(JSON.stringify({ ...mode, peer: 5 })));
  assert.equal(receiver.jitterBufferTarget, null);
  await call.receive(parse(JSON.stringify(mode)));
  assert.equal(receiver.jitterBufferTarget, 0);
  assert.deepEqual(track.mode, mode.mode);
  await call.receive(parse(JSON.stringify({ ...mode, mode: { prefer: "smooth" } })));
  assert.equal(receiver.jitterBufferTarget, null);
});

test("ultrawide publisher stats distinguish capture, verified caps and encoded resolution", () => {
  const capture = { width: 3440, height: 1440 };
  const target = TIERS.screen[2];
  const previous = { id: "screen", timestamp: 1000, bytesSent: 0 };
  const encoded = {
    id: "screen", timestamp: 2000, bytesSent: 250_000, framesPerSecond: 30,
    frameWidth: 2580, frameHeight: 1080, qualityLimitationReason: "bandwidth",
  };
  assert.deepEqual(publisherMeasurements(capture, target, { cap: TIERS.screen[3], scale: 1 }, encoded, previous),
    ["3440 × 1440", "1440p60", "2160p60 · 15.0 Mb/s", "3440 × 1440", "2580 × 1080", "30.0 fps", "2.0 Mb/s", "Sender upload"]);
  assert.deepEqual(publisherMeasurements(capture, target, { cap: TIERS.screen[1], scale: 1440 / 1080, reason: "traffic_budget" }),
    ["3440 × 1440", "1440p60", "1080p30 · 3.0 Mb/s", "2580 × 1080", "—", "—", "—", "SFU traffic budget pacing"]);
  assert.deepEqual(publisherMeasurements(capture, target, {
    cap: [1440, 60, 24_000_000], scale: 1, reason: "sfu_viewer", screenRate: true,
  }).slice(-2), ["—", "SFU per-viewer cap"]);
  const idle = { ...encoded, frameWidth: 3440, frameHeight: 1440, framesPerSecond: 0, bytesSent: 0, qualityLimitationReason: "none" };
  assert.deepEqual(publisherMeasurements(capture, target, { scale: 1 }, idle, previous).slice(-2),
    ["0.0 kb/s", "Within selected quality"]);
});

test("support: encoded transforms and AV1 or VP9, AV1 first", () => {
  const scope = (mimeTypes, transform = class {}) => ({
    RTCRtpScriptTransform: transform,
    RTCRtpReceiver: { getCapabilities: () => ({ codecs: mimeTypes.map((mimeType) => ({ mimeType })) }) },
  });
  assert.deepEqual(support(scope(["video/VP8", "video/VP9", "video/AV1", "video/H264"])), { codecs: ["av1", "vp9"] });
  assert.deepEqual(support(scope(["video/VP9"])), { codecs: ["vp9"] });
  assert.deepEqual(support(scope(["video/H264", "video/VP8"])), { refusal: "codecs" });
  assert.deepEqual(support(scope(["video/AV1"], null)), { refusal: "transforms" });
  assert.deepEqual(support({}), { refusal: "transforms" });
  const preferred = preferences(scope(["video/H264", "video/rtx", "video/VP9", "video/AV1", "video/VP8"]), "video");
  assert.deepEqual(preferred.map((codec) => codec.mimeType), ["video/AV1", "video/VP9", "video/rtx"]);
});

test("publishing: the call codec only, with retransmission, or nothing", () => {
  const scope = (mimeTypes) => ({
    RTCRtpSender: { getCapabilities: () => ({ codecs: mimeTypes.map((mimeType) => ({ mimeType })) }) },
  });
  const sender = scope(["video/VP8", "video/rtx", "video/AV1", "video/VP9", "video/VP9", "video/H264"]);
  assert.deepEqual(sendCodecs(sender, "av1").map((codec) => codec.mimeType), ["video/AV1", "video/rtx"]);
  assert.deepEqual(sendCodecs(sender, "vp9").map((codec) => codec.mimeType), ["video/VP9", "video/VP9", "video/rtx"]);
  assert.deepEqual(sendCodecs(scope(["video/VP8", "video/rtx"]), "av1"), [], "no fallback to another codec");
  assert.deepEqual(sendCodecs({}, "vp9"), []);
});

test("one AV1 encoding uses layers with confirmed encoder support", async () => {
  const video = { width: 1920, height: 1080, frameRate: 30 };
  const scope = { navigator: { mediaCapabilities: { encodingInfo: async ({ type, video }) => {
    assert.equal(type, "webrtc");
    assert.equal(video.contentType, "video/AV1");
    assert.equal(video.width, 1920);
    return { supported: true, smooth: video.scalabilityMode === "L1T3" };
  } } } };
  assert.equal(await scalabilityMode(scope, "av1", "screen", video), "L1T3");
  assert.equal(await scalabilityMode(scope, "av1", "camera", video), undefined);
  assert.equal(await scalabilityMode(scope, "vp9", "screen", video), undefined);
  assert.equal(await scalabilityMode({}, "av1", "screen", video), undefined);
});

test("frames: the AV1 depacketizer's size field comes off again", () => {
  const sent = [0x08, ...Array.from({ length: 200 }, (_, i) => i)];
  const sized = Uint8Array.from([0x0a, 200 | 0x80, 1, ...sent.slice(1)]);
  assert.deepEqual([...unsized(sized)], sent);
  assert.deepEqual([...unsized(Uint8Array.from([0x2a, 2, 7, 9]))], [0x28, 7, 9]);
  for (const other of [[0x08, 1, 2], [0x28], [0x0a, 3, 1], [0x0a, 0x80], [0x2a]]) {
    const frame = Uint8Array.from(other);
    assert.deepEqual([...unsized(frame)], other, String(other));
  }
});

test("receiver stats use interval frame, byte and jitter buffer counters", () => {
  const previous = {
    id: "video", timestamp: 4000, framesDecoded: 120, presentedFrames: 118,
    bytesReceived: 500_000, jitterBufferDelay: 0.5, jitterBufferEmittedCount: 60,
  };
  const current = {
    id: "video", timestamp: 5000, framesDecoded: 150, presentedFrames: 146,
    bytesReceived: 625_000, jitterBufferDelay: 1, jitterBufferEmittedCount: 80,
  };
  assert.deepEqual(receiverRates(previous, current), {
    decoded: 30, presented: 28, bitrate: 1_000_000, jitterBufferMs: 25,
  });
  assert.deepEqual(receiverRates(previous, { ...current, timestamp: 6000 }), {
    decoded: 15, presented: 14, bitrate: 500_000, jitterBufferMs: 25,
  });
});

test("receiver stats discard stale samples and handle unavailable or reset counters", () => {
  const previous = {
    id: "video", timestamp: 4000, framesDecoded: 120, presentedFrames: 118,
    bytesReceived: 500_000, jitterBufferDelay: 0.5, jitterBufferEmittedCount: 60,
  };
  const current = { ...previous, timestamp: 5000, bytesReceived: 0, presentedFrames: undefined };
  assert.deepEqual(receiverRates(previous, current), {
    decoded: 0, presented: undefined, bitrate: undefined, jitterBufferMs: undefined,
  });
  assert.deepEqual(receiverRates(undefined, current), {});
  assert.deepEqual(receiverRates(previous, { ...current, id: "replacement" }), {});
  for (const timestamp of [undefined, 3999, 4000, 9001]) {
    assert.deepEqual(receiverRates(previous, { ...current, timestamp }), {});
  }
});
