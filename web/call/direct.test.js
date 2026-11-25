// SPDX-License-Identifier: EUPL-1.2
import assert from "node:assert/strict";
import { test } from "node:test";
import { DirectPaths } from "./direct.js";

const bitrates = (availableOutgoingBitrate) => new Map([
  ["transport", { type: "transport", selectedCandidatePairId: "pair" }],
  ["pair", { availableOutgoingBitrate, localCandidateId: "host", remoteCandidateId: "host" }],
  ["host", { candidateType: "host" }],
]);

function sender(track, encoding, replace) {
  return {
    track,
    getParameters: () => ({ encodings: [{ ...encoding }] }),
    setParameters: async ({ encodings }) => { encoding = encodings[0]; },
    async replaceTrack(next) {
      replace(next);
      this.track = next;
    },
  };
}

class Connection extends EventTarget {
  static opened = [];
  connectionState = "new";
  iceGatheringState = "complete";
  bitrate = 10_000_000;
  transceivers = [];

  constructor(configuration) {
    super();
    this.configuration = configuration;
    Connection.opened.push(this);
  }

  addTransceiver() {
    const transceiver = {
      mid: "d0",
      setCodecPreferences() {},
      sender: sender(null, {}, (track) => {
        if (track) assert.equal(sfuSender.track, null, "SFU must release the track first");
      }),
      receiver: { track: { kind: "video" } },
    };
    this.transceivers.push(transceiver);
    return transceiver;
  }

  getTransceivers() { return this.transceivers; }
  async setRemoteDescription({ type }) {
    if (type === "offer") {
      this.signalingState = "have-remote-offer";
      this.addTransceiver();
    } else {
      this.signalingState = "stable";
    }
  }
  async setLocalDescription() {
    this.localDescription = { sdp: "v=0" };
    this.signalingState = this.signalingState === "have-remote-offer" ? "stable" : "have-local-offer";
  }
  async getStats() { return bitrates(this.bitrate); }
  close() { this.setState("closed"); }
  setState(state) {
    this.connectionState = state;
    this.dispatchEvent(new Event("connectionstatechange"));
  }
}

const track = { kind: "video", readyState: "live" };
const sfuReceiver = { track };
const remoteTrack = { kind: "video", peer: 1, source: "s0", receiver: sfuReceiver };
const sent = [];
let sfuSender;

function call(id, relay = false) {
  return {
    self: id,
    relay,
    codec: "vp9",
    peers: new Map([[id === 1 ? 2 : 1, "participant"]]),
    sources: new Map(),
    tracks: new Map(),
    keys: new Map(),
    configuration: { iceServers: [{ urls: ["turn:turn.test:3478?transport=udp"], credential: "secret" }] },
    stream: { request: async (request) => ({ from: id, ...request }) },
    deliver(message) { sent.push(message); },
    changed() {},
  };
}

async function signal(from, paths, type) {
  const index = sent.findIndex((entry) => entry.from === from && JSON.parse(entry.data).type === type);
  assert.notEqual(index, -1, `missing ${type} from ${from}`);
  const [{ kind, peer, publisher, data }] = sent.splice(index, 1);
  assert.equal(kind, "direct");
  assert.equal(peer, from === 1 ? 2 : 1);
  await paths.receive({ peer: from, publisher: !publisher, data });
}

test("direct waits for consent, upload headroom and both ICE legs, then falls back on bandwidth loss", async (t) => {
  globalThis.RTCPeerConnection = Connection;
  globalThis.RTCRtpScriptTransform = class {};
  globalThis.MediaStream = class {};
  const host = call(1);
  const guest = call(2);
  host.publisher = { getStats: async () => bitrates(upload) };
  let upload = 5_000_000;
  sfuSender = sender(track, { maxBitrate: 4_500_000 }, (next) => {
    if (next && Connection.opened.length) {
      assert.equal(Connection.opened[0].connectionState, "closed", "direct must close before restoring SFU");
    }
  });
  host.sources.set("camera", { track, transceiver: { mid: "s0", sender: sfuSender } });
  guest.tracks.set("g0", remoteTrack);
  guest.keys.set(1, { keyed: true });
  const publish = new DirectPaths(host);
  const subscribe = new DirectPaths(guest);
  t.after(() => { publish.close(); subscribe.close(); });

  await publish.tick();
  assert.equal(Connection.opened.length, 0);
  await subscribe.tick();
  await signal(2, publish, "ready");
  assert.equal(Connection.opened.length, 0);

  upload = 6_000_000;
  await publish.tick();
  const [outgoing] = Connection.opened;
  assert.deepEqual(outgoing.configuration, { ...host.configuration, bundlePolicy: "max-bundle" });
  assert.equal(sfuSender.track, track);
  assert.equal(outgoing.getTransceivers()[0].sender.track, null);
  await signal(1, subscribe, "offer");
  await signal(2, publish, "answer");
  const incoming = Connection.opened[1];
  outgoing.setState("connected");
  await publish.tick();
  assert.equal(sfuSender.track, track);
  incoming.setState("connected");
  await subscribe.tick();
  await signal(2, publish, "connected");
  assert.equal(sfuSender.track, null);
  assert.equal(outgoing.getTransceivers()[0].sender.track, track);
  assert.equal(publish.sender(sfuSender), outgoing.getTransceivers()[0].sender);
  await signal(1, subscribe, "active");
  assert.notEqual(subscribe.track(remoteTrack).receiver, remoteTrack.receiver);

  outgoing.bitrate = 4_000_000;
  await publish.tick();
  assert.equal(sfuSender.track, track);
  assert.equal(publish.sender(sfuSender), sfuSender);
  await signal(1, subscribe, "close");
  assert.equal(subscribe.track(remoteTrack), remoteTrack);
});

test("relay-only and calls with a third participant skip direct signaling and ICE", async () => {
  const initial = Connection.opened.length;
  for (const relay of [true, false]) {
    const host = call(1, relay);
    host.keys.set(2, { keyed: true });
    if (!relay) host.peers.set(3, "participant");
    host.deliver = () => assert.fail("the direct leg sent signaling");
    const paths = new DirectPaths(host);
    await paths.tick();
    await paths.receive({ peer: 2, publisher: true, data: JSON.stringify({ type: "ready" }) });
  }
  assert.equal(Connection.opened.length, initial);
});

test("available upload above the screen tier baseline can offer direct despite a higher encoder cap", async (t) => {
  globalThis.RTCPeerConnection = Connection;
  globalThis.RTCRtpScriptTransform = class {};
  const host = call(1);
  host.screenRate = true;
  host.publishLimits = { screen: { height: 1440, fps: 30 } };
  const initial = Connection.opened.length;
  let upload = 9_900_000;
  host.publisher = { getStats: async () => bitrates(upload) };
  host.sources.set("screen", {
    track, remoteCap: [2160, 60, 24_000_000],
    transceiver: { mid: "s0", sender: sender(track, { maxBitrate: 24_000_000 }, () => {}) },
  });
  const publish = new DirectPaths(host);
  t.after(() => publish.close());
  await publish.receive({ peer: 2, publisher: true, data: '{"type":"ready"}' });
  assert.equal(Connection.opened.length, initial);
  upload = 10_000_000;
  await publish.tick();
  assert.equal(Connection.opened.length, initial + 1);
});
