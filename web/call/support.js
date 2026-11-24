// SPDX-License-Identifier: EUPL-1.2
// What this browser brings to a call: encoded transforms for the stream worker's
// SFrame, and the royalty-free codecs it decodes in WebRTC (docs/video-plan.md §6).
import { TIERS } from "./signal.js";

/** `{codecs}` with the call codecs this browser decodes, AV1 first, or `{refusal}`. */
export function support(scope = globalThis) {
  if (typeof scope.RTCRtpScriptTransform !== "function") return { refusal: "transforms" };
  const decoded = scope.RTCRtpReceiver?.getCapabilities?.("video")?.codecs ?? [];
  const types = new Set(decoded.map((codec) => codec.mimeType.toLowerCase()));
  const codecs = ["av1", "vp9"].filter((name) => types.has(`video/${name}`));
  return codecs.length ? { codecs } : { refusal: "codecs" };
}

/** A video sender's track content hint and degradation preference: Sharp keeps
 * detail and resolution, Smooth motion and frame rate. */
export const HINTS = {
  sharp: ["detail", "maintain-resolution"],
  smooth: ["motion", "maintain-framerate"],
};

/** A receiver renders a Low latency source's frames at once and keeps its
 * adaptive jitter buffer for other sources. */
export function setLatency(receiver, mode) {
  if (receiver && "jitterBufferTarget" in receiver) receiver.jitterBufferTarget = mode?.low_latency ? 0 : null;
}

const kept = {
  video: ["video/av1", "video/vp9", "video/rtx", "video/red", "video/ulpfec", "video/flexfec-03"],
  audio: ["audio/opus", "audio/red"],
};

/** A transceiver's codecs for `kind`: AV1, VP9 and Opus with their repair formats. */
export function preferences(scope, kind) {
  const codecs = scope.RTCRtpReceiver.getCapabilities(kind)?.codecs ?? [];
  const rank = (codec) => kept[kind].indexOf(codec.mimeType.toLowerCase());
  return codecs.filter((codec) => rank(codec) >= 0).sort((a, b) => rank(a) - rank(b));
}

/** The codecs a sender offers in the call codec `codec` ("av1" or "vp9"), with
 * retransmission; empty when this browser cannot encode it. */
export function sendCodecs(scope, codec, kind = "video") {
  const codecs = scope.RTCRtpSender?.getCapabilities?.(kind)?.codecs ?? [];
  const primary = codecs.filter((entry) => entry.mimeType.toLowerCase() === `${kind}/${codec}`);
  if (kind === "audio") return primary;
  const rtx = codecs.filter((entry) => entry.mimeType.toLowerCase() === "video/rtx");
  return primary.length ? [...primary, ...rtx] : [];
}

/** A single AV1 encoding can expose multiple decode targets through its RTP
 * Dependency Descriptor. Probe at the capture size before enabling it. Screens
 * use temporal layers only: Chromium's screen-content AV1 encoder writes key
 * pictures with an undecodable inter-layer predicted frame. */
export async function scalabilityMode(scope, codec, kind, { width, height, frameRate }) {
  if (codec !== "av1" || !width || !height || !frameRate) return undefined;
  const mode = kind === "screen" ? "L1T3" : "L3T3_KEY";
  try {
    const capabilities = await scope.navigator.mediaCapabilities.encodingInfo({
      type: "webrtc",
      video: {
        contentType: "video/AV1", width, height, framerate: frameRate,
        bitrate: TIERS[kind].at(-1)[2], scalabilityMode: mode,
      },
    });
    if (capabilities.supported && capabilities.smooth) return mode;
  } catch {
    // Browsers without a WebRTC encoder capability probe use one layer.
  }
}
