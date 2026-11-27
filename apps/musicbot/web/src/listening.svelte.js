// SPDX-License-Identifier: EUPL-1.2
// Listening in this browser: one BrowserAudio for the whole page, so the
// player bar and the big screen control the same stream.
import { store } from "./store.svelte.js";
import { BrowserAudio } from "./browser-audio.js";

class Listening {
  /** "off", "starting", "waiting" or "playing". */
  mode = $state("off");
  /** Browser volume in percent; independent of the Mumble volume. */
  volume = $state(50);
  /** Last failure message, cleared when the user toggles listening again. */
  error = $state("");
  active = $derived(this.mode !== "off");
  /** Text for the player status line, or null when there is nothing to say. */
  status = $derived(this.error || (this.mode === "starting" ? "Starting browser audio…"
    : this.mode === "waiting" ? "Waiting for browser audio…"
    : this.mode === "playing" ? "Playing in this browser"
    : null));

  #audio = new BrowserAudio(
    (state) => { this.mode = state; },
    (failure) => {
      this.error = failure instanceof Error ? failure.message : String(failure);
      store.toast(this.error);
    },
  );

  /** True when queue `player` can be streamed to this browser right now. */
  available(player) {
    return store.connection === "open" && !store.invalid
      && store.browseKey !== null && !!store.snapshot?.players[player];
  }

  /**
   * Call once, during App's initialisation: follows the selected queue and
   * stops when listening is no longer possible or the page goes away.
   */
  attach() {
    $effect(() => { this.select(store.selected); });
    $effect(() => { if (!this.available(store.selected)) this.stop(); });
    $effect(() => {
      const stop = () => this.stop();
      window.addEventListener("pagehide", stop);
      return () => {
        window.removeEventListener("pagehide", stop);
        stop();
      };
    });
  }

  // Must run directly in the click handler: the browser only allows audio
  // to start from a user gesture.
  toggle(player) {
    this.error = "";
    if (this.active) this.stop();
    else if (this.available(player)) this.#audio.start(player, store.browseSignal);
  }

  setVolume(volume) {
    this.volume = volume;
    this.#audio.setVolume(volume);
  }

  select(player) {
    this.#audio.select(player);
  }

  stop() {
    this.#audio.stop();
  }
}

export const listening = new Listening();

/**
 * Headphone cue in this browser: one DJ deck before its channel fader,
 * private to this browser and beside the room's stream, with its own volume.
 */
class CueListening {
  /** The deck heard, or null. */
  deck = $state(null);
  player = $state(null);
  /** "off", "starting", "waiting" or "playing". */
  mode = $state("off");
  volume = $state(70);

  #audio = new BrowserAudio(
    (state) => {
      this.mode = state;
      if (state === "off") this.deck = null;
    },
    (failure) => store.toast(failure instanceof Error ? failure.message : String(failure)),
    "api/cue",
  );

  /** Call once, during App's initialisation: stops when DJ mode or the connection goes. */
  attach() {
    this.#audio.setVolume(this.volume);
    $effect(() => {
      if (this.deck === null) return;
      const player = store.snapshot?.players[this.player];
      if (store.connection !== "open" || store.invalid || !player?.dj) this.stop();
    });
    $effect(() => {
      const stop = () => this.stop();
      window.addEventListener("pagehide", stop);
      return () => {
        window.removeEventListener("pagehide", stop);
        stop();
      };
    });
  }

  // Must run directly in the click handler, like Listening.toggle.
  toggle(player, deck) {
    if (this.deck === deck && this.player === player) {
      this.stop();
      return;
    }
    if (this.deck !== null) {
      this.player = player;
      this.deck = deck;
      this.#audio.select({ player, deck });
      return;
    }
    if (store.connection !== "open" || store.invalid || !store.browseSignal) return;
    this.player = player;
    this.deck = deck;
    this.#audio.start({ player, deck }, store.browseSignal);
  }

  setVolume(volume) {
    this.volume = volume;
    this.#audio.setVolume(volume);
  }

  stop() {
    this.#audio.stop();
    this.deck = null;
  }
}

export const cueListening = new CueListening();
