// SPDX-License-Identifier: EUPL-1.2
// DJ mode controls: commands to `api/dj` and the values a control shows
// while it moves. Continuous controls (faders, knobs) send the latest value
// about 30 times a second at most, one request at a time per control, and
// show their own value until the bot's state has caught up.
import { postJSON } from "./api.js";
import { store } from "./store.svelte.js";
import { DJ_SEND_MS, clamp, paced } from "./logic.js";

/** How long a moved control keeps its own value after its last request. */
const HOLD_MS = 700;

class Dj {
  /** Values of controls being moved, by key (`player:deck:name` or `player:name`). */
  local = $state.raw({});
  /** The length of the one-button fade, in seconds; the page and MIDI share it. */
  fadeSeconds = $state(8);
  #senders = new Map();
  #releases = new Map();

  /** The value a control shows: its own while it moves, otherwise the bot's. */
  value(key, server) {
    return Object.hasOwn(this.local, key) ? this.local[key] : server;
  }

  /** Sends one control; on failure shows the bot's reason and returns false. */
  async send(body) {
    try {
      await postJSON("api/dj", body);
      return true;
    } catch (error) {
      store.toast(error instanceof Error ? error.message : String(error));
      return false;
    }
  }

  #release(key) {
    clearTimeout(this.#releases.get(key));
    this.#releases.set(key, setTimeout(() => {
      if (!this.#senders.get(key)?.idle()) return;
      const local = { ...this.local };
      delete local[key];
      this.local = local;
    }, HOLD_MS));
  }

  #continuous(key, value, body) {
    this.local = { ...this.local, [key]: value };
    let send = this.#senders.get(key);
    if (!send) {
      send = paced(async (body) => {
        await this.send(body);
        this.#release(key);
      }, DJ_SEND_MS);
      this.#senders.set(key, send);
    }
    clearTimeout(this.#releases.get(key));
    send(body);
  }

  setMode(player, on) {
    return this.send({ player, control: "mode", on });
  }

  /** True when `item` belongs to the library the page shows; a stale item gets a toast asking to choose again. */
  #current(item) {
    if (store.browseItemCurrent(item)) return true;
    store.toast("The library changed or is reconnecting. Choose the item from the refreshed library.");
    return false;
  }

  /**
   * Loads onto `deck` a library item (`{ kind, library, id, namespace }`, kind
   * "song" by default) or an entry of the main queue or a deck queue
   * (`{ entry }`). An album or playlist loads its first song and puts the rest
   * at the front of the deck's queue.
   */
  load(player, deck, item) {
    if (item.entry != null) return this.send({ player, control: "load", deck, entry: item.entry });
    if (!this.#current(item)) return Promise.resolve(false);
    return this.send({ player, control: "load", deck, library: item.library, id: item.id, kind: item.kind ?? "song" });
  }

  /** Loads the first song of the deck's queue onto `deck`, else the main queue's first. */
  loadNext(player, deck) {
    const entry = store.snapshot?.players[player]?.deck_queues?.[deck]?.[0] ?? store.queue(player)[0];
    if (!entry) {
      store.toast(`The deck ${deck === 0 ? "A" : "B"} queue and the main queue are empty.`);
      return Promise.resolve(false);
    }
    return this.load(player, deck, { entry: entry.id });
  }

  /** Adds a library song, album or playlist to the queue of `deck` at `index` (null: at the end). */
  queueAdd(player, deck, item, index = null) {
    if (!this.#current(item)) return Promise.resolve(false);
    return this.send({ player, control: "queue_add", deck, library: item.library, id: item.id, kind: item.kind ?? "song", index });
  }

  /**
   * Moves an entry of any queue into the queue of `deck` (0 or 1), or into the
   * main queue for `deck` null, at `index` counted in that queue without the entry.
   */
  queueMove(player, entry, deck, index) {
    return this.send({ player, control: "queue_move", entry, deck, index });
  }

  queueRemove(player, entry) {
    return this.send({ player, control: "queue_remove", entry });
  }

  queueClear(player, deck) {
    return this.send({ player, control: "queue_clear", deck });
  }

  eject(player, deck) {
    return this.send({ player, control: "eject", deck });
  }

  play(player, deck, playing) {
    return this.send({ player, control: "play", deck, playing });
  }

  seek(player, deck, ms) {
    return this.send({ player, control: "seek", deck, position_ms: Math.max(0, Math.round(ms)) });
  }

  setCue(player, deck) {
    return this.send({ player, control: "set_cue", deck });
  }

  jumpToCue(player, deck) {
    return this.send({ player, control: "jump_to_cue", deck });
  }

  sync(player, deck) {
    return this.send({ player, control: "sync", deck });
  }

  tempoRange(player, deck, range) {
    return this.send({ player, control: "tempo_range", deck, range });
  }

  loopIn(player, deck) {
    return this.send({ player, control: "loop_in", deck });
  }

  loopOut(player, deck) {
    return this.send({ player, control: "loop_out", deck });
  }

  /** Leaves the loop, or jumps back into the last loop. */
  loopToggle(player, deck) {
    return this.send({ player, control: "loop_toggle", deck });
  }

  /** A loop of `beats` beats (0.25 to 32) from the playing position. */
  beatLoop(player, deck, beats) {
    return this.send({ player, control: "beat_loop", deck, beats });
  }

  /** `factor` 0.5 halves the loop, 2 doubles it. */
  loopResize(player, deck, factor) {
    return this.send({ player, control: "loop_resize", deck, factor });
  }

  /** Shifts the loop by its own length (`by` "loop") or one beat ("beat") in `direction` -1 or 1. */
  loopMove(player, deck, by, direction) {
    return this.send({ player, control: "loop_move", deck, by, direction });
  }

  /** Sets both loop ends, paced while a handle moves; the bot snaps them to beats with quantise on. */
  loopSet(player, deck, startMs, endMs) {
    const value = { start_ms: Math.max(0, Math.round(startMs)), end_ms: Math.max(0, Math.round(endMs)) };
    this.#continuous(`${player}:${deck}:loop`, value, { player, control: "loop_set", deck, ...value });
  }

  quantize(player, deck, on) {
    return this.send({ player, control: "quantize", deck, on });
  }

  volume(player, deck, volume) {
    const value = clamp(Math.round(volume), 0, 100);
    this.#continuous(`${player}:${deck}:volume`, value, { player, control: "volume", deck, volume: value });
  }

  eq(player, deck, band, db) {
    const value = clamp(Math.round(db * 2) / 2, -26, 6);
    this.#continuous(`${player}:${deck}:eq:${band}`, value, { player, control: "eq", deck, band, db: value });
  }

  filter(player, deck, filter) {
    const value = clamp(Math.round(filter * 100) / 100, -1, 1);
    this.#continuous(`${player}:${deck}:filter`, value, { player, control: "filter", deck, value });
  }

  /** `tempo` in percent, within `range`. */
  tempo(player, deck, tempo, range) {
    const value = clamp(Math.round(tempo * 100) / 100, -range, range);
    this.#continuous(`${player}:${deck}:tempo`, value, { player, control: "tempo", deck, tempo: value });
  }

  crossfader(player, position) {
    const value = clamp(Math.round(position * 100) / 100, -1, 1);
    this.#continuous(`${player}:crossfader`, value, { player, control: "crossfader", position: value });
  }

  curve(player, curve) {
    return this.send({ player, control: "curve", curve });
  }

  transition(player, seconds) {
    this.#senders.get(`${player}:crossfader`)?.cancel();
    return this.send({ player, control: "transition", seconds });
  }

  autoLoad(player, on) {
    return this.send({ player, control: "auto_load", on });
  }
}

export const dj = new Dj();
