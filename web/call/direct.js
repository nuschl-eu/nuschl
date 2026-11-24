// SPDX-License-Identifier: EUPL-1.2
// One optional WebRTC leg per publisher in a two-person call. Signalling goes
// through the stream worker's authenticated pair, and the SFU stays available
// for fallback. A direct leg negotiates with detached senders.
import { MAX_VIDEO_BITRATE, TIERS, fitTier, publishTier } from "./signal.js";
import { sendCodecs, setLatency } from "./support.js";

const GATHER_TIMEOUT = 3000;
const SETUP_TIMEOUT = 15000;
const RETRY_DELAY = 15000;
const MAX_SDP = 12000;
export const ENCODINGS = { camera: 0, screen: 1, audio: 3 };
const id = (value) => Number.isSafeInteger(value) && value > 0;
const mid = (value) => typeof value === "string" && /^[a-zA-Z0-9_-]{1,16}$/.test(value);

function parse(data) {
  let value;
  try { value = JSON.parse(data); } catch { return null; }
  if (value?.type === "ready") return value;
  if (!id(value?.id)) return null;
  switch (value.type) {
    case "offer":
      if (!Array.isArray(value.sources) || value.sources.length < 1 || value.sources.length > 2 ||
          !value.sources.every((source) => mid(source?.mid) && mid(source.source)) ||
          new Set(value.sources.map(({ mid }) => mid)).size !== value.sources.length ||
          new Set(value.sources.map(({ source }) => source)).size !== value.sources.length) return null;
      // An offer and an answer have the same SDP bound.
    case "answer":
      return typeof value.sdp === "string" && value.sdp.length <= MAX_SDP ? value : null;
    case "connected":
    case "active":
    case "close":
      return value;
    default:
      return null;
  }
}

async function candidates(pc) {
  if (pc.iceGatheringState === "complete") return pc.localDescription.sdp;
  await new Promise((resolve) => {
    function finish() {
      clearTimeout(timeout);
      pc.removeEventListener("icegatheringstatechange", gathered);
      resolve();
    }
    function gathered() {
      if (pc.iceGatheringState === "complete") finish();
    }
    // An unresponsive ICE server can leave gathered host candidates usable.
    const timeout = setTimeout(finish, GATHER_TIMEOUT);
    pc.addEventListener("icegatheringstatechange", gathered);
    gathered();
  });
  return pc.localDescription.sdp;
}

async function selectedPair(pc) {
  if (!pc) return {};
  const stats = await pc.getStats();
  const transport = [...stats.values()].find(({ type, selectedCandidatePairId }) =>
    type === "transport" && selectedCandidatePairId);
  const pair = stats.get(transport?.selectedCandidatePairId);
  const local = stats.get(pair?.localCandidateId)?.candidateType;
  const remote = stats.get(pair?.remoteCandidateId)?.candidateType;
  const types = ["host", "srflx", "prflx"];
  const direct = types.includes(local) && types.includes(remote);
  return { direct, bitrate: pair?.availableOutgoingBitrate };
}

function encodingOf(sender) {
  const encodings = sender.getParameters().encodings;
  return encodings?.length === 1 ? encodings[0] : null;
}

function desiredRate(call, sources) {
  let total = 0;
  for (const { kind, local } of sources) {
    const rate = encodingOf(local.sender)?.maxBitrate;
    if (!Number.isFinite(rate) || rate <= 0) return 0;
    if (kind === "screen" && call.screenRate) {
      const source = call.sources.get(kind);
      if (source?.transceiver.sender !== local.sender) return 0;
      const [height] = publishTier(kind, source.tier);
      const remoteHeight = source.remoteCap?.[0] ?? height;
      // Route selection uses the selected tier's bitrate baseline.
      const baseline = TIERS.screen[fitTier(kind, Math.min(height, remoteHeight))][2];
      total += baseline;
    } else {
      total += rate;
    }
  }
  return total;
}

async function applyCaps({ kind, local, direct }, screenRate) {
  const caps = encodingOf(local.sender);
  const { degradationPreference } = local.sender.getParameters();
  const parameters = direct.sender.getParameters();
  const encoding = parameters.encodings[0];
  const values = {
    maxBitrate: kind === "screen" && screenRate ? MAX_VIDEO_BITRATE : caps.maxBitrate,
    maxFramerate: caps.maxFramerate,
    scaleResolutionDownBy: caps.scaleResolutionDownBy,
  };
  if (parameters.degradationPreference === degradationPreference &&
    !Object.entries(values).some(([key, value]) => encoding[key] !== value)) return;
  parameters.degradationPreference = degradationPreference;
  for (const [key, value] of Object.entries(values)) {
    if (value === undefined) delete encoding[key];
    else encoding[key] = value;
  }
  await direct.sender.setParameters(parameters);
}

function sameSources(sources, call) {
  return sources.length === call.sources.size - Number(call.sources.has("audio")) && sources.every(({ kind, local }) => {
    const source = call.sources.get(kind);
    return local.track.readyState === "live" && source?.track === local.track && source.transceiver.sender === local.sender;
  });
}

export class DirectPaths {
  #call;
  #queue = Promise.resolve();
  #next = 0;
  #asked = null;
  #ready = null;
  #incoming = null;
  #outgoing = null;
  #retryAt = 0;
  #pending = 0;
  #checking = null;

  constructor(call) {
    this.#call = call;
  }

  #enqueue(action) {
    const result = this.#queue.then(action);
    this.#queue = result.catch(() => {});
    return result;
  }

  #peer() {
    const call = this.#call;
    if (call.relay || call.ended || call.self === null || call.peers.size !== 1) return null;
    return call.peers.keys().next().value;
  }

  async #send(peer, publisher, message) {
    if (this.#peer() !== peer) throw new Error("direct peer left");
    const { stream } = this.#call;
    const reply = await stream.request({ kind: "direct", peer, publisher, data: JSON.stringify(message) });
    if (this.#peer() !== peer) throw new Error("direct peer left");
    this.#call.deliver(reply);
  }

  tick() {
    const peer = this.#peer();
    for (const leg of [this.#incoming, this.#outgoing]) {
      if (leg && leg.peer !== peer) leg.pc.close();
    }
    if (this.#checking) return this.#checking;
    this.#checking = this.#enqueue(async () => {
      const peer = this.#peer();
      if (peer === null) {
        this.#asked = this.#ready = null;
        await this.#drop(this.#incoming);
        await this.#drop(this.#outgoing);
        return;
      }
      for (const leg of [this.#incoming, this.#outgoing]) {
        if (leg && (leg.peer !== peer || leg.pc.connectionState === "closed")) await this.#drop(leg);
      }
      if (this.#call.keys.get(peer)?.keyed && !this.#incoming) {
        await this.#send(peer, false, { type: "ready" }).then(() => { this.#asked = peer; }, () => {});
      }
      await this.#consider(peer);
    }).finally(() => { this.#checking = null; });
    return this.#checking;
  }

  /** An event from the stream worker, after verifying the peer's pairwise key. */
  async receive({ peer, publisher, data }) {
    if (this.#peer() !== peer || this.#pending >= 8) return;
    const value = parse(data);
    if (!value) return;
    this.#pending += 1;
    return this.#enqueue(async () => {
      if (this.#peer() !== peer) return;
      if (publisher && value.type === "ready") {
        this.#ready = peer;
        await this.#consider(peer);
        return;
      }
      if (!publisher && value.type === "offer" && this.#asked === peer && !this.#incoming) {
        await this.#accept(peer, value);
        return;
      }
      const leg = publisher ? this.#outgoing : this.#incoming;
      if (leg?.peer !== peer || leg.id !== value.id) return;
      try {
        if (value.type === "close") await this.#drop(leg);
        if (publisher && value.type === "answer" && leg.pc.signalingState === "have-local-offer") {
          await leg.pc.setRemoteDescription({ type: "answer", sdp: value.sdp });
        }
        if (publisher && value.type === "connected") {
          leg.remoteConnected = true;
          await this.#activate(leg);
        }
        if (!publisher && value.type === "active" && leg.pc.connectionState === "connected") {
          leg.active = true;
          this.#call.changed();
        }
      } catch {
        await this.#drop(leg, true);
      }
    }).finally(() => { this.#pending -= 1; });
  }

  /** Return the current receiver for a source, with its existing SFU identity. */
  track(track) {
    const leg = this.#incoming;
    const direct = leg?.active && leg.peer === track.peer && leg.tracks.get(track.source);
    return direct ? { ...track, ...direct } : track;
  }

  /** Sample the sender carrying this source's media. */
  sender(local) {
    const leg = this.#outgoing;
    if (!leg?.active) return local;
    return leg.sources.find((source) => source.local.sender === local)?.direct.sender ?? local;
  }

  get active() {
    return Boolean(this.#incoming?.active || this.#outgoing?.active);
  }

  /** Fall back before replacing the sources or changing the call codec. */
  resetPublisher() {
    return this.#enqueue(() => this.#drop(this.#outgoing, true));
  }

  close() {
    for (const leg of [this.#outgoing, this.#incoming]) {
      if (leg) {
        clearTimeout(leg.timeout);
        leg.pc.close();
      }
    }
    this.#incoming = this.#outgoing = this.#asked = this.#ready = null;
  }

  #connection(peer, publisher, id) {
    // Use existing ICE credentials for discovery. Only a nominated non-relay
    // pair can carry media on this connection.
    const pc = new RTCPeerConnection({ ...this.#call.configuration, bundlePolicy: "max-bundle" });
    const leg = { peer, publisher, id, pc, active: false };
    pc.addEventListener("connectionstatechange", () => {
      this.#enqueue(async () => {
        if (leg !== (publisher ? this.#outgoing : this.#incoming)) return;
        try {
          if (["disconnected", "failed", "closed"].includes(pc.connectionState)) {
            await this.#drop(leg, true);
          } else if (pc.connectionState === "connected") {
            if (!(await selectedPair(pc)).direct) await this.#drop(leg, true);
            else if (publisher) await this.#activate(leg);
            else await this.#send(peer, false, { type: "connected", id });
          }
        } catch {
          await this.#drop(leg, true);
        }
      });
    });
    leg.timeout = setTimeout(() => {
      this.#enqueue(() => !leg.active && this.#drop(leg, true));
    }, SETUP_TIMEOUT);
    return leg;
  }

  async #consider(peer) {
    const call = this.#call;
    if (this.#ready !== peer || performance.now() < this.#retryAt || !call.publisher || !call.sources.size) return;
    const sources = [...call.sources].filter(([kind]) => kind !== "audio").map(([kind, { track, transceiver }]) => ({
      kind,
      mid: transceiver.mid,
      local: { track, sender: transceiver.sender },
    }));
    if (sources.some(({ mid, local }) => !mid || local.track.readyState !== "live")) return;
    const desired = desiredRate(call, sources);
    if (!desired) return;
    const leg = this.#outgoing;
    if (leg && (!sameSources(leg.sources, call) || leg.peer !== peer)) return this.#drop(leg, true);
    const { direct, bitrate } = await selectedPair(leg?.active ? leg.pc : call.publisher);
    if (leg?.active) {
      if (!direct || (Number.isFinite(bitrate) && bitrate * 0.8 < desired)) return this.#drop(leg, true);
      try {
        for (const source of leg.sources) await applyCaps(source, call.screenRate);
      } catch {
        await this.#drop(leg, true);
      }
      return;
    }
    if (!Number.isFinite(bitrate) || bitrate * 0.8 < desired || leg || this.#peer() !== peer) return;
    const attempt = this.#connection(peer, true, ++this.#next);
    attempt.sources = sources;
    this.#outgoing = attempt;
    try {
      for (const source of sources) {
        const transceiver = attempt.pc.addTransceiver("video", { direction: "sendonly" });
        transceiver.setCodecPreferences(sendCodecs(globalThis, call.codec));
        transceiver.sender.transform = new RTCRtpScriptTransform(call.stream.worker, {
          side: "send", encoding: ENCODINGS[source.kind],
        });
        source.direct = transceiver;
      }
      await attempt.pc.setLocalDescription();
      const sdp = await candidates(attempt.pc);
      await this.#send(peer, true, {
        type: "offer", id: attempt.id, sdp,
        sources: sources.map(({ mid, direct }) => ({ source: mid, mid: direct.mid })),
      });
    } catch {
      await this.#drop(attempt, true);
    }
  }

  async #accept(peer, offer) {
    const call = this.#call;
    const known = new Set([...call.tracks.values()].filter((track) => track.peer === peer && track.kind === "video")
      .map(({ source }) => source));
    if (!offer.sources.every(({ source }) => known.has(source))) return;
    const leg = this.#connection(peer, false, offer.id);
    this.#incoming = leg;
    try {
      await leg.pc.setRemoteDescription({ type: "offer", sdp: offer.sdp });
      const transceivers = leg.pc.getTransceivers();
      if (transceivers.length !== offer.sources.length) throw new Error("direct offer sources changed");
      leg.tracks = new Map();
      for (const { mid, source } of offer.sources) {
        const transceiver = transceivers.find((entry) => entry.mid === mid);
        const receiver = transceiver?.receiver;
        if (receiver?.track.kind !== "video") throw new Error("direct offer source is missing");
        transceiver.direction = "recvonly";
        receiver.transform = new RTCRtpScriptTransform(call.stream.worker, {
          side: "receive", peer, track: receiver.track.id,
        });
        setLatency(receiver, [...call.tracks.values()].find((track) => track.peer === peer && track.source === source)?.mode);
        leg.tracks.set(source, { receiver, stream: new MediaStream([receiver.track]) });
      }
      await leg.pc.setLocalDescription();
      const sdp = await candidates(leg.pc);
      await this.#send(peer, false, { type: "answer", id: leg.id, sdp });
    } catch {
      await this.#drop(leg, true);
    }
  }

  async #activate(leg) {
    const call = this.#call;
    if (leg.active || !leg.remoteConnected || leg.pc.connectionState !== "connected") return;
    if (this.#peer() !== leg.peer || !sameSources(leg.sources, call) || !(await selectedPair(leg.pc)).direct) {
      return this.#drop(leg, true);
    }
    const { bitrate } = await selectedPair(call.publisher);
    const desired = desiredRate(call, leg.sources);
    if (!desired || !Number.isFinite(bitrate) || bitrate * 0.8 < desired) return this.#drop(leg, true);
    // Install the direct encoder caps while the senders are detached.
    for (const source of leg.sources) await applyCaps(source, call.screenRate);
    if (this.#peer() !== leg.peer || !sameSources(leg.sources, call)) return this.#drop(leg, true);
    leg.detached = true;
    for (const { local } of leg.sources) await local.sender.replaceTrack(null);
    for (const { local, direct } of leg.sources) {
      if (this.#peer() !== leg.peer || leg.pc.connectionState !== "connected") return this.#drop(leg, true);
      await direct.sender.replaceTrack(local.track);
    }
    await this.#send(leg.peer, true, { type: "active", id: leg.id });
    leg.active = true;
    call.changed();
  }

  async #drop(leg, notify = false) {
    if (!leg || leg !== (leg.publisher ? this.#outgoing : this.#incoming)) return;
    clearTimeout(leg.timeout);
    leg.pc.close();
    if (leg.publisher) {
      this.#outgoing = null;
      this.#retryAt = performance.now() + RETRY_DELAY;
    } else {
      this.#incoming = null;
    }
    if (leg.active) this.#call.changed();
    if (leg.detached && !this.#call.ended) {
      const call = this.#call;
      for (const { kind, local } of leg.sources) {
        if (local.track.readyState === "live" && call.sources.get(kind)?.transceiver.sender === local.sender) {
          await local.sender.replaceTrack(local.track).catch(() => {});
        }
      }
    }
    if (notify) await this.#send(leg.peer, leg.publisher, { type: "close", id: leg.id }).catch(() => {});
  }
}
