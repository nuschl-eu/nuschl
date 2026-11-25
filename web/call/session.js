// SPDX-License-Identifier: EUPL-1.2
// One call on the page (docs/video-plan.md §2, §4): admission at the SFU, the
// subscribe connection with each publisher's frames decrypted in the stream worker,
// who is in the call, and this page's own camera and screen on the publish
// connection, encrypted in the stream worker and encoded in the call codec.
import { DirectPaths, ENCODINGS } from "./direct.js";
import { MAX_PEERS, join, parse, publishTier } from "./signal.js";
import { HINTS, preferences, scalabilityMode, sendCodecs, setLatency } from "./support.js";

/** A publisher whose keys have not arrived is asked again after this. */
const RESUBSCRIBE_MS = 5000;
/** The stream worker's rekey clock while publishing (crates/stream-wasm). */
const TICK_MS = 250;
/** Samples transport readiness and the outgoing estimate once per second. */
const PATH_MS = 1000;
const LEASE_MS = 10000;

export class Call extends EventTarget {
  /** The other peers: id → role. */
  peers = new Map();
  /** Forwarded sources by mid on the subscribe connection. */
  tracks = new Map();
  /** Publishers asked for their keys: id → {keyed, asked}. */
  keys = new Map();
  /** This page's camera, screen and optional tab audio. */
  sources = new Map();
  /** The user's Sharp/Smooth and Low latency choice per video source. */
  modes = { camera: { prefer: "smooth", low_latency: false }, screen: { prefer: "sharp", low_latency: false } };
  self = null;
  role = null;
  codec = null;
  /** Why the call ended for this page; null while it lasts. */
  ended = null;

  /** `stream`: a StreamClient. `codecs`: the codecs this browser decodes. */
  constructor(stream, { codecs, relay, allowDirect = !relay }) {
    super();
    this.stream = stream;
    this.codecs = codecs;
    this.relay = relay;
    this.allowDirect = allowDirect && !relay;
    this.direct = new DirectPaths(this);
  }

  /** Opens an invitation, or creates a room on the configured stream host. */
  async start({ link, host }) {
    this.host = host;
    this.ownsLease = link === undefined;
    try {
      this.opened = await this.stream.request({
        kind: this.ownsLease ? "create" : "open", link, host, allow_direct: this.allowDirect,
      });
    } catch {
      return this.end("link");
    }
    if (this.ended) return;
    this.socket = new WebSocket(`wss://${host}/sfu`);
    let inbox = Promise.resolve();
    this.socket.addEventListener("message", ({ data }) => {
      inbox = inbox.then(() => this.receive(parse(data))).catch((error) => {
        console.error("call:", error);
        this.end("lost");
      });
    });
    // A close ends the call once the messages before it are handled: the last may say why.
    this.socket.addEventListener("close", () => {
      inbox = inbox.then(() => this.end("lost"));
    });
    this.timer = setInterval(() => this.resubscribe(), RESUBSCRIBE_MS);
    if (this.allowDirect) this.pathTimer = setInterval(() => this.checkPath(), PATH_MS);
  }

  get creator() {
    return Boolean(this.opened?.creator);
  }

  send(message) {
    if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(message));
  }

  /** Sends a stream worker reply's key messages and notes its key events. */
  deliver({ send, events }) {
    for (const { to, data } of send) this.send({ type: "key", to, data });
    for (const event of events) {
      const keys = this.keys.get(event.peer);
      if (event.kind === "keyed" && keys) {
        keys.keyed = true;
        this.checkPath();
      }
      if (event.kind === "direct") this.direct.receive(event).catch(() => {});
    }
  }

  async receive(message) {
    switch (message?.type) {
      case "hello": {
        this.videoLimitReports = message.video_limits === true;
        this.screenRate = message.screen_rate === true;
        const admission = await this.stream.request({ kind: "admit", nonce: message.nonce });
        if (this.ownsLease && admission.create) {
          this.leaseBody = JSON.stringify({ stream_id: this.opened.room, host_key: admission.create.host_key });
        }
        this.send(join(this.opened, admission, this.codecs, {
          videoLimits: this.videoLimitReports, screenRate: this.screenRate,
          maxScreenBitrate: message.max_screen_bitrate,
        }));
        return;
      }
      case "welcome":
        if (this.relay && !message.turn) return this.end("relay");
        if (this.leaseBody) this.leaseTimer = setInterval(() => this.renewLease(), LEASE_MS);
        this.self = message.peer;
        this.role = message.role;
        this.codec = message.codec;
        for (const { id, role } of message.peers) this.peers.set(id, role);
        this.configuration = {
          iceTransportPolicy: this.relay ? "relay" : "all",
          iceServers: message.turn ? [message.turn] : [],
        };
        this.subscriber = new RTCPeerConnection(this.configuration);
        this.subscriber.addEventListener("connectionstatechange", () => {
          if (this.subscriber.connectionState === "failed") this.end("lost");
        });
        break;
      case "joined":
        if (this.peers.size < MAX_PEERS) this.peers.set(message.peer, message.role);
        this.checkPath();
        break;
      case "left":
        this.peers.delete(message.peer);
        this.checkPath();
        this.keys.delete(message.peer);
        for (const [mid, track] of this.tracks) if (track.peer === message.peer) this.tracks.delete(mid);
        await this.stream.request({ kind: "leave", peer: message.peer });
        break;
      case "codec":
        this.codec = message.codec;
        if (this.sources.size) await this.recodec();
        break;
      case "answer":
        if (!this.publisher) throw new Error("answer without an offer");
        await this.publisher.setRemoteDescription({ type: "answer", sdp: message.sdp });
        this.offering = false;
        for (const [kind, source] of this.sources) await this.applyCap(kind, source);
        if (this.again) await this.offer();
        break;
      case "offer":
        await this.answer(message);
        break;
      case "key":
        // A key message that fails leaves the keys as they were; resubscribe asks again.
        this.deliver(await this.stream.request({ kind: "key", from: message.from, data: message.data }).catch(
          () => ({ send: [], events: [] }),
        ));
        break;
      case "quality":
        await this.capSource(message);
        return;
      case "video_limit": {
        const track = this.tracks.get(message.mid);
        if (this.videoLimitReports && track?.peer === message.peer && track.kind === "video") {
          track.limit = message.limit;
        }
        return;
      }
      case "mode": {
        const track = this.tracks.get(message.mid);
        if (track?.peer !== message.peer || track.kind !== "video") return;
        track.mode = message.mode;
        setLatency(track.receiver, track.mode);
        setLatency(this.direct.track(track).receiver, track.mode);
        return;
      }
      case "error":
        return this.end(message.reason);
      default:
        if (message) this.dispatchEvent(new CustomEvent("message", { detail: message }));
        return;
    }
    this.changed();
  }

  /** Answers the SFU's offer on the subscribe connection; every receiver decrypts
   * in the stream worker under its publisher's keys. */
  async answer({ sdp, tracks }) {
    const pc = this.subscriber;
    if (!pc) throw new Error("offer before welcome");
    const offered = new Map(tracks.map((track) => [track.mid, track]));
    await pc.setRemoteDescription({ type: "offer", sdp });
    const current = new Map();
    for (const transceiver of pc.getTransceivers()) {
      const track = offered.get(transceiver.mid);
      if (!track || transceiver.currentDirection === "stopped") continue;
      const { receiver } = transceiver;
      const audio = track.kind === "audio";
      if (audio !== (receiver.track.kind === "audio")) throw new Error("track kind mismatch");
      const known = this.tracks.get(transceiver.mid);
      const sameSource = known?.receiver === receiver && known.peer === track.peer && known.source === track.source;
      if (!sameSource) {
        receiver.transform = new RTCRtpScriptTransform(this.stream.worker, {
          side: "receive", peer: track.peer, audio, track: receiver.track.id,
        });
        try {
          transceiver.setCodecPreferences(preferences(globalThis, receiver.track.kind));
        } catch {
          // The browser keeps its own order.
        }
      }
      current.set(transceiver.mid, sameSource ? known : {
        peer: track.peer,
        source: track.source,
        screen: track.kind === "screen",
        kind: receiver.track.kind,
        receiver,
        stream: new MediaStream([receiver.track]),
      });
    }
    this.tracks = current;
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    this.send({ type: "answer", sdp: answer.sdp });
    const publishers = new Set([...current.values()].map((track) => track.peer));
    for (const peer of this.keys.keys()) if (!publishers.has(peer)) this.keys.delete(peer);
    for (const peer of publishers) if (!this.keys.has(peer)) await this.subscribe(peer);
  }

  async subscribe(peer) {
    this.keys.set(peer, { keyed: false, asked: Date.now() });
    this.deliver(await this.stream.request({ kind: "subscribe", peer }).catch(() => ({ send: [], events: [] })));
  }

  resubscribe() {
    for (const [peer, { keyed, asked }] of this.keys) {
      if (!keyed && Date.now() - asked >= RESUBSCRIBE_MS) this.subscribe(peer);
    }
  }

  /** Whether this browser encodes the call codec. */
  get canPublish() {
    return this.codec !== null && sendCodecs(globalThis, this.codec).length > 0;
  }

  /** Publishes `track`, from the user's own Start camera or Share screen, as this
   * page's camera, screen or tab audio source. */
  async publish(kind, track, tier) {
    const pending = Promise.resolve(this.publishing).catch(() => {})
      .then(() => this.publishTrack(kind, track, tier));
    this.publishing = pending;
    try {
      await pending;
    } catch (error) {
      track.stop();
      throw error;
    } finally {
      if (this.publishing === pending) this.publishing = null;
    }
  }

  async publishTrack(kind, track, tier) {
    const audio = kind === "audio";
    if (audio !== (track.kind === "audio") || (audio && !this.sources.has("screen"))) return track.stop();
    if (audio && !sendCodecs(globalThis, "opus", "audio").length) throw new Error("Opus unavailable");
    const mode = audio ? undefined : await scalabilityMode(globalThis, this.codec, kind, track.getSettings());
    if (this.ended || this.sources.has(kind) || track.readyState === "ended") return track.stop();
    if (this.publisher && !audio) await this.direct.resetPublisher();
    if (this.ended || track.readyState === "ended") return track.stop();
    if (!this.publisher) {
      this.deliver(await this.stream.request({ kind: "publish" }));
      if (this.ended) return track.stop();
      this.publisher = new RTCPeerConnection(this.configuration);
      // An unused data channel, offered first, holds the transport: stopping a source's
      // media section, also its first, leaves it up.
      this.publisher.createDataChannel("sfu");
      await this.offer();
      this.ticker = setInterval(() => {
        this.stream.request({ kind: "tick" }).then((reply) => this.deliver(reply), () => {});
      }, TICK_MS);
    }
    const transceiver = this.sender(kind, track, mode);
    const source = { track, transceiver, stream: new MediaStream([track]), tier };
    this.sources.set(kind, source);
    // The browser's own Stop sharing, or an unplugged camera.
    track.addEventListener("ended", () => {
      if (this.sources.get(kind) === source) this.unpublish(kind).catch(() => this.end("media"));
    });
    this.changed();
    try {
      await this.offer();
    } catch (error) {
      if (this.sources.get(kind) === source) this.sources.delete(kind);
      transceiver.stop();
      this.changed();
      throw error;
    }
  }

  /** A send transceiver for `track`, encrypted in the stream worker. */
  sender(kind, track, mode) {
    if (kind !== "audio") track.contentHint = HINTS[this.modes[kind].prefer][0];
    mode = this.codec === "av1" ? mode : undefined;
    const add = (encoding) => this.publisher.addTransceiver(track, {
      direction: "sendonly", ...(encoding ? { sendEncodings: [{ scalabilityMode: encoding }] } : {}),
    });
    let transceiver;
    try {
      transceiver = add(mode);
    } catch (error) {
      if (!mode) throw error;
      transceiver = add();
    }
    transceiver.sender.transform = new RTCRtpScriptTransform(this.stream.worker, {
      side: "send",
      encoding: ENCODINGS[kind],
    });
    transceiver.codec = this.codec;
    return transceiver;
  }

  /** Apply the SFU's limits to the encoder for this publish MID. */
  async capSource({ source: mid, max_height: height, max_fps: fps, max_bitrate: bitrate, limit }) {
    for (const [kind, source] of this.sources) {
      if (source.transceiver.mid !== mid) continue;
      source.remoteCap = [height, fps, bitrate];
      source.remoteLimit = this.videoLimitReports ? limit : undefined;
      await this.applyCap(kind, source);
      return;
    }
  }

  setPublishTier(kind, tier) {
    const source = this.sources.get(kind);
    if (!source) return;
    source.tier = tier;
    return this.applyCap(kind, source);
  }

  /** Sets `kind`'s Sharp/Smooth and Low latency choice, also for its live source. */
  async setMode(kind, mode) {
    this.modes[kind] = mode;
    const source = this.sources.get(kind);
    if (!source) return;
    source.track.contentHint = HINTS[mode.prefer][0];
    this.sendModes();
    await this.applyCap(kind, source);
  }

  /** Tells the SFU the mode of each video source on the publish connection. */
  sendModes() {
    const modes = {};
    for (const [kind, { transceiver }] of this.sources) {
      if (kind !== "audio" && transceiver.mid) modes[transceiver.mid] = this.modes[kind];
    }
    this.send({ type: "modes", modes });
  }

  applyCap(kind, source) {
    if (kind === "audio") return;
    source.applying = Promise.resolve(source.applying).then(async () => {
      if (this.sources.get(kind) !== source || this.ended) return;
      const [localHeight, localFps, localRate] = publishTier(kind, source.tier);
      const [remoteHeight, remoteFps, remoteRate] = source.remoteCap ?? [localHeight, localFps, localRate];
      const height = Math.min(localHeight, remoteHeight);
      const fps = Math.min(localFps, remoteFps);
      const bitrate = kind === "screen" && this.screenRate ? remoteRate : Math.min(localRate, remoteRate);
      const { sender } = source.transceiver;
      const parameters = sender.getParameters();
      const encoding = parameters.encodings?.[0];
      if (!encoding) return;
      const settings = source.track.getSettings();
      const scale = Math.max(1, Math.min(settings.width ?? 0, settings.height ?? 0) / height);
      const degradation = HINTS[this.modes[kind].prefer][1];
      if (encoding.scaleResolutionDownBy === scale && encoding.maxFramerate === fps && encoding.maxBitrate === bitrate &&
        parameters.degradationPreference === degradation) return;
      parameters.degradationPreference = degradation;
      encoding.scaleResolutionDownBy = scale;
      encoding.maxFramerate = fps;
      encoding.maxBitrate = bitrate;
      try {
        await sender.setParameters(parameters);
      } catch {
        // A stopped sender or a browser with fewer encoding controls keeps its settings.
      }
    });
    return source.applying;
  }

  /** Restarts each source's encoder in the new call codec on a new transceiver: the
   * SFU keeps a media section's first codec. The first frame is a key frame, and
   * the encryptors continue, so no SFrame nonce repeats. */
  async recodec() {
    await this.direct.resetPublisher();
    for (const [kind, source] of this.sources) {
      if (kind === "audio" || source.transceiver.codec === this.codec) continue;
      source.transceiver.stop();
      const mode = await scalabilityMode(globalThis, this.codec, kind, source.track.getSettings());
      if (this.sources.get(kind) !== source || this.ended) continue;
      source.transceiver = this.sender(kind, source.track, mode);
      source.remoteCap = undefined;
      source.remoteLimit = undefined;
    }
    await this.offer();
  }

  /** Stops this page's `kind` source: the capture ends and the SFU drops its tracks. */
  async unpublish(kind) {
    const source = this.sources.get(kind);
    if (!source) return;
    this.sources.delete(kind);
    source.track.stop();
    if (kind === "screen") {
      const audio = this.sources.get("audio");
      if (audio) {
        this.sources.delete("audio");
        audio.track.stop();
        audio.transceiver.stop();
      }
    }
    if (kind !== "audio") await this.direct.resetPublisher();
    source.transceiver.stop();
    this.changed();
    await this.offer();
  }

  /** Offers the publish connection in the call codec, one offer at a time. */
  async offer() {
    if (this.offering) {
      this.again = true;
      return;
    }
    this.offering = true;
    this.again = false;
    const codecs = sendCodecs(globalThis, this.codec);
    try {
      for (const [kind, { transceiver }] of this.sources) {
        transceiver.setCodecPreferences(kind === "audio" ? sendCodecs(globalThis, "opus", "audio") : codecs);
      }
      await this.publisher.setLocalDescription();
      const screen = this.sources.get("screen")?.transceiver.mid;
      this.send({ type: "offer", sdp: this.publisher.localDescription.sdp, screens: screen ? [screen] : [] });
      this.sendModes();
    } catch (error) {
      this.offering = false;
      throw error;
    }
  }

  /** Host actions (P): end the call for everyone, or remove one peer. */
  endForEveryone() {
    this.send({ type: "end" });
  }

  renewLease() {
    // A simple POST also renews on older stream hosts. The room's WebSocket
    // reports expiration independently of the HTTP response.
    fetch(`https://${this.host}/sfu/lease`, {
      method: "POST", credentials: "omit", redirect: "error",
      body: this.leaseBody, signal: AbortSignal.timeout(LEASE_MS),
    }).catch(() => {});
  }

  remove(peer) {
    this.send({ type: "remove", peer });
  }

  /** Ends the call for this page; the first reason stays. */
  end(reason) {
    if (this.ended) return;
    this.ended = reason;
    clearInterval(this.timer);
    clearInterval(this.ticker);
    clearInterval(this.pathTimer);
    clearInterval(this.leaseTimer);
    this.leaseBody = null;
    this.direct.close();
    for (const { track } of this.sources.values()) track.stop();
    this.sources.clear();
    this.socket?.close();
    this.subscriber?.close();
    this.publisher?.close();
    this.stream?.close();
    this.changed();
  }

  checkPath() {
    if (this.allowDirect && !this.ended) this.direct.tick().catch(() => {});
  }

  changed() {
    this.dispatchEvent(new Event("change"));
  }
}
