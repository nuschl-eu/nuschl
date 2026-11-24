// SPDX-License-Identifier: EUPL-1.2
// The call view: a grid of camera tiles, a large view for a shared screen or a
// pinned camera, the roster and the host's actions. DOM nodes only, no markup
// strings (the stream host requires Trusted Types). This page's own sources show as
// tiles too; `controls.js` starts and stops them.

import { qualityOptions } from "./controls.js";
import { TIERS, fitTier, publishTier, qualityRequest } from "./signal.js";
import { VideoStats } from "./receiver-stats.js";

const endings = {
  transforms: ["Browser not supported", "Use a current Firefox or Chromium."],
  codecs: ["Browser not supported", "No AV1 or VP9 video. Use a current Firefox or Chromium."],
  codec: ["No shared video format", "Use a current Firefox or Chromium."],
  link: ["Link not valid", "Ask for the call link again."],
  proof: ["Link not valid", "Ask for the call link again."],
  no_room: ["Call not found", "It may have ended."],
  ended: ["Call ended", ""],
  removed: ["Removed from the call", ""],
  full: ["Call full", ""],
  busy: ["Call host busy", "Try again later."],
  rate: ["Call host busy", "Try again later."],
  bandwidth: ["Call bandwidth is full", "Try rejoining when a video stops or its quality drops."],
  relay: ["Relay unavailable", "This call host offers no relay."],
};
const rejoin = new Set(["lost", "media", "slow", "busy", "rate", "bandwidth"]);

const element = (document, name, attributes = {}, ...children) => {
  const node = document.createElement(name);
  for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, value);
  node.append(...children);
  return node;
};

const pin = (track) => `${track.peer}/${track.screen ? "screen" : "camera"}`;

/** A speaker icon, crossed out when `silent`, as the native client draws it. */
const speaker = (document, silent) => {
  const svg = "http://www.w3.org/2000/svg";
  const icon = document.createElementNS(svg, "svg");
  for (const [key, value] of Object.entries({ viewBox: "0 0 24 24", fill: "none", stroke: "currentColor",
    "stroke-width": "2", "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" })) {
    icon.setAttribute(key, value);
  }
  for (const d of ["M4 9.5h3.5L12 5.5v13l-4.5-4H4z", silent ? "M16 9.5l5 5M21 9.5l-5 5" : "M15.5 9a4 4 0 0 1 0 6M18.5 6a8 8 0 0 1 0 12"]) {
    const path = document.createElementNS(svg, "path");
    path.setAttribute("d", d);
    icon.append(path);
  }
  return icon;
};

/** Shows the picture inside a received frame. VCN 4 codes AV1 in frames aligned to
 * 64 columns and 16 rows (1920 × 1080 as 1920 × 1082), the encoder core names the
 * picture's size in the Dependency Descriptor, and browsers show the whole frame. The
 * SFU forwards the descriptor outside the end-to-end encryption, so the crop stays
 * within that alignment. */
function crop(video, pictures) {
  const [width, height] = pictures.get(video.srcObject?.getVideoTracks()[0]?.id) ?? [];
  const right = video.videoWidth - width, bottom = video.videoHeight - height;
  video.style.objectViewBox = right >= 0 && right < 64 && bottom >= 0 && bottom < 16 ?
    `inset(0px ${right}px ${bottom}px 0px)` : "";
}

export class View {
  /** The pinned source, "peer/camera" or "peer/screen": it keeps its place when its
   * publisher restarts the encoder on a new media section. */
  pinned = null;
  /** A tile per video track mid, kept while the track lasts. */
  tiles = new Map();
  /** A player per shared audio track mid; they play out of sight. */
  audio = new Map();
  /** Each publisher's shared audio as this viewer set it, for the rest of the call. */
  levels = new Map();
  /** Explicit viewer choices survive a publisher's encoder restart. */
  tiers = new Map();
  sentTiers = new WeakMap();
  sizes = new Map();
  /** Each received video track's picture size, from its keyframes. */
  pictures = new Map();

  constructor(root, call, retry = () => this.document.defaultView.location.reload()) {
    const document = root.ownerDocument ?? root;
    this.document = document;
    this.call = call;
    this.retry = retry;
    this.stats = new VideoStats(document);
    this.observer = new document.defaultView.ResizeObserver((entries) => {
      for (const { target, contentRect, devicePixelContentBoxSize } of entries) {
        const device = devicePixelContentBoxSize?.[0];
        const pixels = device ? Math.min(device.inlineSize, device.blockSize) :
          Math.min(contentRect.width, contentRect.height) * document.defaultView.devicePixelRatio;
        this.sizes.get(target)?.resize(pixels);
      }
    });
    const $ = (id) => root.querySelector(`#${id}`);
    this.nodes = {
      status: $("status"), stage: $("stage"), large: $("large"), grid: $("grid"), empty: $("empty"),
      audio: $("shared-audio"),
      roster: $("roster"), count: $("count"), controls: $("controls"), host: $("host-actions"),
      ending: $("ending"), main: $("call"),
    };
    this.changed = () => this.render();
    call.addEventListener("change", this.changed);
    this.picture = ({ data }) => {
      if (!data?.picture) return;
      const { track, width, height } = data.picture;
      this.pictures.set(track, [width, height]);
      for (const tile of this.tiles.values()) crop(tile.querySelector("video"), this.pictures);
    };
    call.stream?.worker.addEventListener("message", this.picture);
    this.render();
  }

  name(peer) {
    return peer === this.call.self ? "You" : `Participant ${peer}`;
  }

  /** Shows why the call ended or cannot start. */
  showEnding(reason) {
    const [title, detail] = endings[reason] ?? ["Disconnected", ""];
    const { ending, main } = this.nodes;
    this.close();
    main.hidden = true;
    const actions = [];
    if (this.retry && (rejoin.has(reason) || !(reason in endings))) {
      const button = element(this.document, "button", { type: "button" }, "Rejoin");
      button.addEventListener("click", this.retry);
      actions.push(button);
    }
    ending.replaceChildren(element(this.document, "h1", {}, title), ...(detail ? [element(this.document, "p", {}, detail)] : []), ...actions);
    ending.hidden = false;
  }

  close() {
    this.call.removeEventListener("change", this.changed);
    this.call.stream?.worker.removeEventListener("message", this.picture);
    this.stats.close();
    this.observer.disconnect();
    this.sharedAudio(new Map());
    for (const size of this.sizes.values()) size.cancel();
    this.sizes.clear();
  }

  render() {
    const { call } = this;
    if (call.ended) return this.showEnding(call.ended);
    const { status, large, grid, empty, roster, count, host } = this.nodes;
    const values = call.self === null ? ["Connecting"] : [call.codec.toUpperCase()];
    if (call.relay) values.push("Relay only");
    else if (call.direct.active) values.push("Direct video");
    status.textContent = values.join(" · ");

    // Tiles: one per video track and per own source; the large view holds the pinned
    // tile or someone else's screen.
    const own = [...call.sources].filter(([kind]) => kind !== "audio").map(([kind, source]) => [
      `own:${source.track.id}`,
      { peer: call.self, screen: kind === "screen", stream: source.stream, own: true, source },
    ]);
    const videos = [...call.tracks].filter(([, track]) => track.kind === "video")
      .map(([mid, track]) => [mid, call.direct.track(track)]).concat(own);
    this.sharedAudio(call.tracks);
    const shown = new Map(videos);
    for (const [key, tile] of this.tiles) {
      const video = tile.querySelector("video");
      if (shown.get(key)?.stream === video.srcObject) continue;
      this.observer.unobserve(video);
      this.sizes.get(video)?.cancel();
      this.sizes.delete(video);
      const stats = tile.querySelector(".video-stats");
      if (stats) this.stats.close(stats);
      tile.remove();
      this.tiles.delete(key);
    }
    for (const peer of this.tiers.keys()) if (!call.peers.has(peer)) this.tiers.delete(peer);
    // A pin stays while its source is away, as during an encoder restart.
    const pinned = videos.find(([, track]) => pin(track) === this.pinned)?.[0];
    const featured = pinned ?? videos.find(([, track]) => track.screen && !track.own)?.[0] ?? null;
    for (const [mid, track] of videos) {
      const tile = this.tiles.get(mid) ?? this.tile(mid, track);
      this.audioControl(tile, track.peer);
      const parent = mid === featured ? large : grid;
      if (tile.parentNode !== parent) {
        parent.append(tile);
        tile.querySelector("video").play().catch(() => {});
      }
      tile.querySelector("button").setAttribute("aria-pressed", String(mid === pinned));
    }
    large.hidden = featured === null;
    empty.hidden = videos.length > 0;

    // The roster: this page first, then everyone else in id order.
    const people = [[call.self, call.role], ...[...call.peers].sort(([a], [b]) => a - b)].filter(([id]) => id !== null);
    count.textContent = String(people.length);
    roster.replaceChildren(...people.map(([id, role]) => this.person(id, role)));
    if (host) host.hidden = call.role !== "host";
  }

  sharedAudio(tracks) {
    for (const [mid, { track, player }] of this.audio) {
      if (tracks.get(mid) === track) continue;
      player.srcObject = null;
      player.remove();
      this.audio.delete(mid);
    }
    for (const [mid, track] of tracks) {
      if (track.kind !== "audio" || this.audio.has(mid)) continue;
      const player = element(this.document, "audio", { "data-peer": String(track.peer) });
      player.srcObject = track.stream;
      this.nodes.audio.append(player);
      this.audio.set(mid, { track, player });
      this.play(track.peer);
    }
  }

  level(peer) {
    if (!this.levels.has(peer)) this.levels.set(peer, { volume: 1, muted: false });
    return this.levels.get(peer);
  }

  /** Applies `peer`'s level to their players. A browser that refuses to play
   * before a click on the page shows the audio as muted until the viewer unmutes. */
  play(peer) {
    const level = this.level(peer);
    for (const { track, player } of this.audio.values()) {
      if (track.peer !== peer) continue;
      player.muted = level.muted;
      player.volume = level.volume;
      if (!level.muted) {
        player.play().catch(() => {
          level.muted = true;
          player.muted = true;
          this.render();
        });
      }
    }
  }

  /** The speaker and volume on someone else's screen tile; greyed out while
   * their share carries no audio. */
  audioControl(tile, peer) {
    const control = tile.querySelector(".shared-audio");
    if (!control) return;
    const available = [...this.audio.values()].some(({ track }) => track.peer === peer);
    const level = this.level(peer);
    const silent = !available || level.muted || level.volume === 0;
    const [button, volume] = control.children;
    button.disabled = !available;
    button.setAttribute("aria-label", !available ? "No shared audio" : silent ? "Unmute shared audio" : "Mute shared audio");
    if (button.dataset.silent !== String(silent)) {
      button.dataset.silent = String(silent);
      button.replaceChildren(speaker(this.document, silent));
    }
    volume.hidden = !available;
    volume.value = String(Math.round((level.muted ? 0 : level.volume) * 100));
  }

  sharedAudioControl(peer) {
    const doc = this.document;
    const volume = element(doc, "input", { type: "range", min: "0", max: "100", step: "5", "aria-label": "Shared audio volume" });
    const button = element(doc, "button", { type: "button" });
    button.addEventListener("click", () => {
      const level = this.level(peer);
      if (level.muted || level.volume === 0) {
        level.muted = false;
        if (level.volume === 0) level.volume = 1;
      } else {
        level.muted = true;
      }
      this.play(peer);
      this.render();
    });
    volume.addEventListener("input", () => {
      const level = this.level(peer);
      level.volume = Number(volume.value) / 100;
      level.muted = false;
      this.play(peer);
      this.render();
    });
    return element(doc, "div", { class: "shared-audio" }, button, volume);
  }

  tile(mid, track) {
    const video = element(this.document, "video", { autoplay: "", playsinline: "" });
    video.muted = true;
    video.srcObject = track.stream;
    const label = track.screen ? `${this.name(track.peer)} · Screen` : this.name(track.peer);
    const toggle = element(this.document, "button", { type: "button", class: "pin", "aria-pressed": "false" }, "Pin");
    toggle.setAttribute("aria-label", `Pin ${label}`);
    toggle.addEventListener("click", () => {
      this.pinned = this.pinned === pin(track) ? null : pin(track);
      this.render();
    });
    const tile = element(this.document, "figure", { class: "tile", "data-state": "waiting", "data-peer": String(track.peer) },
      video, element(this.document, "figcaption", { title: label }, label), toggle);
    if (track.screen) tile.dataset.screen = "";
    if (track.screen && !track.own) tile.append(this.sharedAudioControl(track.peer));
    if (track.own) {
      tile.dataset.own = "";
      const kind = track.screen ? "screen" : "camera";
      video.addEventListener("resize", () => this.call.applyCap(kind, track.source).catch(() => {}));
    }
    if (track.own || track.receiver) tile.append(this.videoStats(mid, video, track, label));
    video.addEventListener("loadeddata", () => { tile.dataset.state = "playing"; });
    video.addEventListener("resize", () => crop(video, this.pictures));
    this.tiles.set(mid, tile);
    return tile;
  }

  videoStats(mid, video, track, label) {
    const doc = this.document;
    const details = element(doc, "details", { class: "video-stats" });
    const list = element(doc, "dl");
    const jitter = { title: "Average time a frame spent in the receiver’s jitter buffer" };
    const names = track.own
      ? ["Capture resolution", "Send target", "Call server limit", "Encoder size ceiling", "Encoded resolution", "Encoded FPS", "Send bitrate", "Active limit"]
      : ["Resolution", "Decoded frame rate", "Presented frame rate", "Receive bitrate", "Jitter buffer delay", "Receive target", "Active limit"];
    const values = names.map(name => {
      const value = element(doc, "dd", {}, "—");
      list.append(element(doc, "dt", name === "Jitter buffer delay" ? jitter : {}, name), value);
      return value;
    });
    const kind = track.screen ? "screen" : "camera";
    const quality = track.own ? null : this.viewerQuality(mid, video, track, label);
    const message = element(doc, "p", { hidden: "" });
    details.append(element(doc, "summary", { "aria-label": `Video stats for ${label}` }, "Video stats"));
    if (quality) details.append(quality.control);
    details.append(message, list);
    const target = { details, video, message, values, ...(quality ? {
      receiver: track.receiver, requested: quality.requested,
      limit: () => {
        const current = this.call.tracks.get(mid);
        if (current?.receiver === track.receiver && current.peer === track.peer && current.source === track.source) {
          return current.limit;
        }
      },
    } : {
      source: track.source,
      sender: () => this.call.direct.sender(track.source.transceiver.sender),
      requested: () => publishTier(kind, track.source.tier),
      screenRate: kind === "screen" && this.call.screenRate,
    }) };
    details.addEventListener("toggle", () => {
      if (details.open && details.isConnected) this.stats.open({ ...target });
      else this.stats.close(details);
    });
    return details;
  }

  viewerQuality(mid, video, track, label) {
    const kind = track.screen ? "screen" : "camera";
    const select = element(this.document, "select", { "aria-label": `Quality for ${label}` });
    qualityOptions(select, kind);
    const tiers = this.tiers.get(track.peer) ?? {};
    this.tiers.set(track.peer, tiers);
    select.value = tiers[kind] ?? "";

    const original = this.call.tracks.get(mid);
    const sent = this.sentTiers.get(original) ?? { at: -Infinity };
    this.sentTiers.set(original, sent);
    let size = 0, timer;
    // Auto fits the tile; a chosen tier limits it.
    const chosen = () => {
      const limit = select.value !== "" && TIERS[kind][Number(select.value)] ? Number(select.value) : undefined;
      if (size === 0) return limit;
      return Math.min(fitTier(kind, size), limit ?? Infinity);
    };
    const send = () => {
      clearTimeout(timer);
      if (this.call.tracks.get(mid) !== original || this.call.ended) return;
      const tier = chosen();
      if (tier === undefined || tier === sent.tier) return;
      // The SFU accepts one request per second. Keep the latest choice until then.
      const wait = sent.at + 1100 - performance.now();
      if (wait > 0) {
        timer = setTimeout(send, wait);
        return;
      }
      this.call.send(qualityRequest(track.peer, kind, tier));
      sent.tier = tier;
      sent.at = performance.now();
    };
    select.addEventListener("change", () => {
      tiers[kind] = select.value;
      send();
    });
    this.sizes.set(video, { resize: (pixels) => { size = pixels; send(); }, cancel: () => clearTimeout(timer) });
    this.observer.observe(video, { box: "device-pixel-content-box" });
    send();
    return {
      control: element(this.document, "label", { class: "viewer-quality" }, "Receive limit", select),
      requested: () => TIERS[kind][chosen()],
    };
  }

  person(id, role) {
    const labels = { camera: "Camera", screen: "Screen", audio: "Shared audio" };
    const own = id === this.call.self ? [...this.call.sources.keys()].map((kind) => labels[kind]) : [];
    const sources = [...this.call.tracks.values()]
      .filter((track) => track.peer === id)
      .map((track) => labels[track.kind === "audio" ? "audio" : track.screen ? "screen" : "camera"])
      .concat(own);
    const item = element(this.document, "li", {}, element(this.document, "span", { class: "name" }, this.name(id)));
    if (role === "host") item.append(element(this.document, "span", { class: "tag" }, "Host"));
    if (sources.length) item.append(element(this.document, "span", { class: "sources" }, sources.join(", ")));
    if (this.call.role === "host" && id !== this.call.self) {
      const remove = element(this.document, "button", { type: "button", "aria-label": `Remove ${this.name(id)}` }, "Remove");
      remove.addEventListener("click", () => this.call.remove(id));
      item.append(remove);
    }
    return item;
  }
}
