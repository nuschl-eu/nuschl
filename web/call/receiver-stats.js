// SPDX-License-Identifier: EUPL-1.2

import { videoLimitLabel } from "./signal.js";

const INTERVAL_MS = 1000;
const MAX_SAMPLE_SPAN_MS = 5 * INTERVAL_MS;

const delta = (before, after) =>
  Number.isFinite(before) && Number.isFinite(after) && before >= 0 && after >= before
    ? after - before : undefined;

export function receiverRates(previous, current) {
  const span = current.timestamp - previous?.timestamp;
  if (current.id !== previous?.id || !(span > 0 && span <= MAX_SAMPLE_SPAN_MS)) return {};
  const rate = (field, scale = 1000 / span) => {
    const count = delta(previous[field], current[field]);
    return count === undefined ? undefined : count * scale;
  };
  const emitted = delta(previous.jitterBufferEmittedCount, current.jitterBufferEmittedCount);
  return {
    decoded: rate("framesDecoded"),
    presented: rate("presentedFrames"),
    bitrate: rate("bytesReceived", 8000 / span),
    jitterBufferMs: emitted > 0 ? rate("jitterBufferDelay", 1000 / emitted) : undefined,
  };
}

function videoSample(report, video) {
  for (const stats of report.values()) {
    if (stats.type !== "inbound-rtp" || (stats.kind ?? stats.mediaType) !== "video") continue;
    const { id, timestamp, framesDecoded, bytesReceived, jitterBufferDelay, jitterBufferEmittedCount } = stats;
    const quality = video.getVideoPlaybackQuality?.();
    return {
      id, timestamp, framesDecoded, bytesReceived, jitterBufferDelay, jitterBufferEmittedCount,
      presentedFrames: delta(quality?.droppedVideoFrames, quality?.totalVideoFrames),
    };
  }
}

function outgoingSample(report) {
  for (const entry of report.values()) {
    if (entry.type === "outbound-rtp" && (entry.kind ?? entry.mediaType) === "video" && entry.active !== false) return entry;
  }
}

const number = (value, unit) => Number.isFinite(value) ? `${value.toFixed(1)} ${unit}` : "—";
const dimension = (value) => Number.isInteger(value) && value > 0;
const dimensions = (width, height) => dimension(width) && dimension(height) ? `${width} × ${height}` : "—";

function display(target, texts, status = "") {
  const { values, message } = target;
  for (const [index, text] of texts.entries()) {
    if (values[index].textContent !== text) values[index].textContent = text;
  }
  if (message.textContent !== status) message.textContent = status;
  message.hidden = !status;
}

const tier = (limits) => limits ? `${limits[0]}p${limits[1]}` : "—";

function bitrateText(value) {
  return value >= 1_000_000 ? number(value / 1_000_000, "Mb/s") : number(value / 1000, "kb/s");
}

function displayReceiver(target, rates = {}, status = "") {
  const { video, requested, limit } = target;
  const { videoWidth: width, videoHeight: height } = video;
  const reported = limit();
  display(target, [dimensions(width, height), number(rates.decoded, "fps"), number(rates.presented, "fps"),
    bitrateText(rates.bitrate), number(rates.jitterBufferMs, "ms"), tier(requested()),
    reported === "within_target" ? "SFU within receive target" : videoLimitLabel(reported)], status);
}

const limitations = new Map([
  ["cpu", "Sender CPU"],
  ["bandwidth", "Sender upload"],
  ["other", "Sender encoder"],
]);

export function publisherMeasurements({ width, height, frameRate }, requested, { cap, reason, scale, screenRate }, sample, previous) {
  const planned = Number.isFinite(scale) && scale >= 1
    ? dimensions(Math.floor(width / scale), Math.floor(height / scale)) : "—";
  const ceiling = cap ? `${tier(cap)} · ${bitrateText(cap[2])}` : "—";
  const span = sample?.timestamp - previous?.timestamp;
  const bytes = sample?.id === previous?.id ? delta(previous?.bytesSent, sample?.bytesSent) : undefined;
  const bitrate = bytes !== undefined && span > 0 && span <= MAX_SAMPLE_SPAN_MS ? 8000 * bytes / span : undefined;
  let active = limitations.get(sample?.qualityLimitationReason);
  if (!active) {
    const limited = screenRate ? reason && reason !== "within_target" : cap?.[2] < requested[2];
    if (cap && (cap[0] < requested[0] || cap[1] < requested[1] || limited)) {
      active = reason ? videoLimitLabel(reason) : "Server encoder ceiling · reason unavailable";
    } else if (Math.min(width, height) + 2 < requested[0]) {
      active = "Capture resolution";
    } else if (Number.isFinite(frameRate) && frameRate + 1 < requested[1]) {
      active = "Capture frame rate";
    } else {
      active = sample?.qualityLimitationReason === "none" ? "Within selected quality" : "Unavailable";
    }
  }
  return [dimensions(width, height), tier(requested), ceiling, planned,
    dimensions(sample?.frameWidth, sample?.frameHeight), number(sample?.framesPerSecond, "fps"),
    bitrateText(bitrate), active];
}

function displayPublisher(target, sender, sample, status = "") {
  const { source, requested, screenRate } = target;
  const scale = sender?.getParameters().encodings?.[0]?.scaleResolutionDownBy;
  const onSfu = sender === source.transceiver.sender;
  const cap = onSfu ? source.remoteCap : undefined;
  const reason = onSfu ? source.remoteLimit : undefined;
  const previous = target.sampleSender === sender ? target.previous : undefined;
  display(target, publisherMeasurements(source.track.getSettings(), requested(), { cap, reason, scale, screenRate }, sample, previous), status);
  target.previous = sample;
  target.sampleSender = sender;
}

/** One stats disclosure and one browser read at a time, even across track changes. */
export class VideoStats {
  constructor(document) {
    this.document = document;
    this.window = document.defaultView;
  }

  open(target) {
    if (this.target?.details === target.details) return;
    this.close();
    this.target = target;
    target.previous = undefined;
    if (target.sender) displayPublisher(target, target.sender(), undefined, "Measuring…");
    else displayReceiver(target, {}, "Measuring…");
    this.timer = this.window.setInterval(() => void this.sample(), INTERVAL_MS);
    void this.sample();
  }

  close(details) {
    if (!this.target || (details && details !== this.target.details)) return;
    this.target.details.open = false;
    this.target = undefined;
    this.window.clearInterval(this.timer);
  }

  paused(target) {
    if (!(this.document.hidden || target.video.paused || target.video.ended)) return false;
    target.previous = undefined;
    if (target.sender) displayPublisher(target, target.sender(), undefined, "Video paused");
    else displayReceiver(target, {}, "Video paused");
    return true;
  }

  async sample() {
    const target = this.target;
    if (!target) return;
    const { details, receiver, video } = target;
    if (!details.open || !details.isConnected) return this.close(details);
    if (this.paused(target) || this.pending) return;
    const sender = target.sender?.();
    this.pending = true;
    let current;
    try {
      const report = await (sender ?? receiver).getStats();
      current = sender ? outgoingSample(report) : videoSample(report, video);
    } catch {
      // A stopped sender or receiver may reject an outstanding read.
    } finally {
      this.pending = false;
    }
    if (this.target !== target || !details.open || !details.isConnected || this.paused(target)) return;
    if (sender) {
      if (target.sender() === sender) {
        const status = dimension(current?.frameWidth) ? "" : "Waiting for encoded video stats";
        displayPublisher(target, sender, current, status);
      }
      return;
    }
    const rates = current ? receiverRates(target.previous, current) : {};
    target.previous = current;
    const status = current ? ("decoded" in rates ? "" : "Measuring…") : "Waiting for video stats";
    displayReceiver(target, rates, status);
  }
}
