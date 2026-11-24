// SPDX-License-Identifier: EUPL-1.2
// Start camera and Share screen (docs/video-plan.md §7): the camera opens only on the
// user's own Start camera, every share asks the browser's picker again, and this
// page never opens the microphone.

import { TIERS } from "./signal.js";

export function qualityOptions(select, kind) {
  const option = (label, value) => {
    const entry = select.ownerDocument.createElement("option");
    entry.textContent = label;
    entry.value = value;
    return entry;
  };
  select.replaceChildren(option("Auto", ""), ...TIERS[kind].map(([height, fps], index) =>
    option(`${height}p${fps}`, index)));
}

const capture = {
  camera: (media) =>
    media.getUserMedia({
      video: { width: { ideal: 1920 }, height: { ideal: 1080 }, frameRate: { ideal: 30 } },
      audio: false,
    }),
  // Window sharing first in the picker; a whole monitor stays possible and shows a warning.
  screen: (media, audio) =>
    media.getDisplayMedia({
      video: { displaySurface: audio ? "browser" : "window", frameRate: { ideal: 60 } },
      audio,
      systemAudio: "exclude",
      windowAudio: "exclude",
      selfBrowserSurface: "exclude",
      surfaceSwitching: "exclude",
      monitorTypeSurfaces: "include",
    }),
};
const callCaptureHandle = "nuschl-call";
export const canShareAudio = (video) => video?.getSettings().displaySurface === "browser" &&
  typeof video.getCaptureHandle === "function" && video.getCaptureHandle()?.handle !== callCaptureHandle;
const labels = { camera: ["Start camera", "Stop camera"], screen: ["Share screen", "Stop sharing"] };
const methods = { camera: "getUserMedia", screen: "getDisplayMedia" };
const failures = {
  camera: { NotAllowedError: "Camera blocked", NotFoundError: "No camera found" },
  screen: {},
};

export class Controls {
  /** Sources between the button and their first offer. */
  busy = new Set();
  notice = "";

  constructor(root, call, enableTabAudio = true) {
    const document = root.ownerDocument ?? root;
    this.call = call;
    this.media = document.defaultView.navigator.mediaDevices;
    const $ = (id) => root.querySelector(`#${id}`);
    this.status = $("notice");
    this.audio = $("screen-audio");
    this.audio.checked = false;
    this.audio.addEventListener("change", () => {
      if (!this.audio.checked) this.stopAudio();
    });
    if (enableTabAudio && typeof document.defaultView.MediaStreamTrack?.prototype.getCaptureHandle === "function") {
      try {
        this.media.setCaptureHandleConfig({
          handle: callCaptureHandle, permittedOrigins: [document.defaultView.location.origin],
        });
        this.audio.parentElement.hidden = false;
      } catch {
        // Browsers with video capture alone still share screens normally.
      }
    }
    this.sources = {};
    for (const kind of ["camera", "screen"]) {
      const button = $(kind);
      const quality = $(`${kind}-quality`);
      qualityOptions(quality, kind);
      quality.addEventListener("change", () => call.setPublishTier(kind, quality.value));
      button.addEventListener("click", () => this.toggle(kind));
      this.sources[kind] = { button, quality };
    }
    this.changed = () => this.render();
    call.addEventListener("change", this.changed);
    this.render();
  }

  close() {
    this.call.removeEventListener("change", this.changed);
  }

  async toggle(kind) {
    if (this.call.sources.has(kind)) return this.call.unpublish(kind);
    this.busy.add(kind);
    this.notice = "";
    this.render();
    const wantsAudio = kind === "screen" && !this.audio.parentElement.hidden && this.audio.checked;
    if (kind === "screen") this.audio.checked = false;
    try {
      const media = await capture[kind](this.media, wantsAudio);
      if (this.call.ended) {
        for (const stopped of media.getTracks()) stopped.stop();
        return;
      }
      const [track] = media.getVideoTracks();
      const [audio, ...extra] = media.getAudioTracks();
      const shareAudio = wantsAudio && canShareAudio(track) && audio;
      for (const unwanted of extra) unwanted.stop();
      if (!shareAudio) audio?.stop();
      if (!track) throw new Error("no video in capture");
      if (shareAudio) {
        track.addEventListener("capturehandlechange", () => {
          if (canShareAudio(track)) return;
          if (this.call.sources.get("audio")?.track === audio) this.stopAudio();
          else audio.stop();
        });
        track.addEventListener("ended", () => audio.stop(), { once: true });
      }
      try {
        await this.call.publish(kind, track, this.sources[kind].quality.value);
        if (shareAudio) await this.call.publish("audio", audio);
      } catch (error) {
        audio?.stop();
        throw error;
      }
      if (wantsAudio && !this.call.sources.has("audio")) this.notice = "Tab audio unavailable for this share";
    } catch (error) {
      // A cancelled screen picker is the user's choice and needs no notice.
      if (kind === "camera" || error.name !== "NotAllowedError") {
        this.notice = failures[kind][error.name] ?? (kind === "camera" ? "Camera unavailable" :
          this.call.sources.has("screen") ? "Tab audio unavailable for this share" : "Screen sharing unavailable");
      }
    } finally {
      this.busy.delete(kind);
      this.render();
    }
  }

  stopAudio() {
    this.call.unpublish("audio").catch(() => this.call.end("media"));
  }

  render() {
    const { call } = this;
    for (const [kind, { button }] of Object.entries(this.sources)) {
      const on = call.sources.has(kind);
      button.textContent = labels[kind][Number(on)];
      button.setAttribute("aria-pressed", String(on));
      const supported = typeof this.media?.[methods[kind]] === "function";
      button.disabled = Boolean(call.ended) || this.busy.has(kind) || (!on && (!call.canPublish || !supported));
      button.title = supported ? "" : `${kind === "camera" ? "Camera" : "Screen sharing"} unavailable in this browser`;
    }
    const screen = call.sources.get("screen");
    const sharingAudio = call.sources.has("audio");
    const monitor = screen?.track.getSettings().displaySurface === "monitor";
    if (screen) this.audio.checked = sharingAudio;
    this.audio.disabled = Boolean(call.ended) || this.busy.has("screen") || Boolean(screen && !sharingAudio);
    const unsupported = call.codec && !call.canPublish ? `Can't send ${call.codec.toUpperCase()}` : "";
    this.status.textContent = [
      monitor && "Sharing your whole screen",
      sharingAudio ? "Sharing tab audio" : this.notice || unsupported,
    ].filter(Boolean).join(" · ");
    this.status.hidden = !this.status.textContent;
    this.status.classList.toggle("warning", monitor);
  }
}
