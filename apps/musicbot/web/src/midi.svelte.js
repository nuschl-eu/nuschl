// SPDX-License-Identifier: EUPL-1.2
// A hardware DJ controller through Web MIDI: one input device, a learn mode
// that binds a page control to the next fader, knob or button moved, and a
// mapping per device name kept in this browser's localStorage. Messages
// drive the selected queue's DJ mode through the same controls as the page.
import { store } from "./store.svelte.js";
import { dj } from "./dj.svelte.js";
import {
  boundControls, controlFraction, controlValue, djControls, learnBinding, midiFraction, midiPressed,
  midiStorageKey, parseMapping, parseMidi, relativeDelta, serializeMapping,
} from "./logic.js";

/** Messages a learn collects after the first one, to find the LSB of a 14-bit control. */
const LEARN_WINDOW_MS = 250;
const INPUT_KEY = "nuschl-musicbot:midi-input";

function stored(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function remember(key, value) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    // Storage may be disabled; the mapping then lasts for this page only.
  }
}

class Midi {
  supported = typeof navigator !== "undefined" && typeof navigator.requestMIDIAccess === "function";
  controls = djControls();
  enabled = $state(false);
  error = $state("");
  /** Connected inputs: `{ id, name }`. */
  inputs = $state([]);
  input = $state(null);
  mapping = $state.raw({});
  /** The control id the next MIDI message is bound to, or null. */
  learning = $state(null);
  /** Clicking a control on the decks picks it for learning. */
  learnMode = $state(false);
  #access = null;
  #port = null;
  #learned = null;
  #msb = new Map();

  get inputName() {
    return this.inputs.find((input) => input.id === this.input)?.name ?? null;
  }

  async enable() {
    if (!this.supported || this.enabled) return;
    this.error = "";
    try {
      this.#access = await navigator.requestMIDIAccess();
    } catch (error) {
      this.error = `MIDI access was refused: ${error instanceof Error ? error.message : String(error)}`;
      return;
    }
    this.enabled = true;
    this.#access.onstatechange = () => this.#refresh();
    this.#refresh();
  }

  disable() {
    this.learnMode = false;
    this.cancelLearn();
    this.#detach();
    if (this.#access) this.#access.onstatechange = null;
    this.#access = null;
    this.enabled = false;
    this.inputs = [];
    this.input = null;
    this.mapping = {};
  }

  #refresh() {
    if (!this.#access) return;
    this.inputs = [...this.#access.inputs.values()].map((input) => ({ id: input.id, name: input.name || input.id }));
    if (this.input && this.inputs.some((input) => input.id === this.input)) return;
    const remembered = stored(INPUT_KEY);
    const choice = this.inputs.find((input) => input.name === remembered) ?? this.inputs[0];
    this.select(choice?.id ?? null);
  }

  select(id) {
    this.cancelLearn();
    this.#detach();
    this.input = id;
    this.#msb.clear();
    const port = id === null ? null : this.#access?.inputs.get(id) ?? null;
    if (!port) {
      this.mapping = {};
      return;
    }
    this.#port = port;
    port.onmidimessage = (event) => this.#message(event.data);
    const name = this.inputName;
    remember(INPUT_KEY, name);
    this.mapping = parseMapping(stored(midiStorageKey(name)), this.controls);
  }

  #detach() {
    if (this.#port) this.#port.onmidimessage = null;
    this.#port = null;
  }

  #save() {
    const name = this.inputName;
    if (name !== null) remember(midiStorageKey(name), Object.keys(this.mapping).length ? serializeMapping(this.mapping) : null);
  }

  learn(id) {
    this.cancelLearn();
    this.learning = id;
  }

  cancelLearn() {
    clearTimeout(this.#learned?.timer);
    this.#learned = null;
    this.learning = null;
  }

  clear(id) {
    const mapping = { ...this.mapping };
    delete mapping[id];
    this.mapping = mapping;
    this.#save();
  }

  clearAll() {
    this.mapping = {};
    this.#save();
  }

  setMode(id, mode) {
    const binding = this.mapping[id];
    if (!binding) return;
    this.mapping = { ...this.mapping, [id]: { ...binding, mode } };
    this.#save();
  }

  #message(data) {
    const message = parseMidi(data);
    if (!message) return;
    if (this.learning !== null) {
      this.#collect(message);
      return;
    }
    for (const bound of boundControls(this.mapping, message)) this.#apply(bound, message);
  }

  #collect(message) {
    const control = this.controls.find((item) => item.id === this.learning);
    if (!control) return;
    if (!this.#learned) {
      if (!learnBinding(control, [message])) return;
      this.#learned = { messages: [], timer: setTimeout(() => this.#finishLearn(control), LEARN_WINDOW_MS) };
    }
    this.#learned.messages.push(message);
  }

  #finishLearn(control) {
    const binding = learnBinding(control, this.#learned?.messages ?? []);
    this.#learned = null;
    this.learning = null;
    if (!binding) return;
    // One physical control drives one page control.
    const mapping = {};
    for (const [id, other] of Object.entries(this.mapping)) {
      if (other.type !== binding.type || other.channel !== binding.channel || other.number !== binding.number) mapping[id] = other;
    }
    mapping[control.id] = binding;
    this.mapping = mapping;
    this.#save();
  }

  #apply({ id, binding, lsb }, message) {
    const player = store.selected;
    const state = store.snapshot?.players[player]?.dj;
    if (!state) return;
    const control = this.controls.find((item) => item.id === id);
    if (control.kind === "button") {
      if (midiPressed(message)) this.#press(player, state, id);
      return;
    }
    const key = `${binding.channel}:${binding.number}`;
    let fraction;
    if (binding.mode === "fine") {
      if (!lsb) {
        this.#msb.set(key, message.value);
        fraction = midiFraction(message.value, 0);
      } else {
        if (!this.#msb.has(key)) return;
        fraction = midiFraction(this.#msb.get(key), message.value);
      }
    } else if (binding.mode === "relative") {
      const delta = relativeDelta(message.value);
      if (delta === 0) return;
      fraction = controlFraction(id, this.#current(player, state, id), this.#range(state, id)) + delta / 127;
    } else {
      fraction = midiFraction(message.value);
    }
    this.#set(player, state, id, controlValue(id, fraction, this.#range(state, id)));
  }

  #range(state, id) {
    return id.endsWith("tempo") ? state.decks[Number(id[0])].tempo_range : 8;
  }

  /** The value a range control shows now, for relative encoders. */
  #current(player, state, id) {
    if (id === "crossfader") return dj.value(`${player}:crossfader`, state.crossfader);
    const deck = Number(id[0]);
    const data = state.decks[deck];
    const name = id.slice(2);
    if (name.startsWith("eq.")) return dj.value(`${player}:${deck}:eq:${name.slice(3)}`, data.eq[name.slice(3)]);
    return dj.value(`${player}:${deck}:${name}`, data[name]);
  }

  #set(player, state, id, value) {
    if (id === "crossfader") return dj.crossfader(player, value);
    const deck = Number(id[0]);
    const name = id.slice(2);
    if (name === "volume") dj.volume(player, deck, value);
    else if (name === "filter") dj.filter(player, deck, value);
    else if (name === "tempo") dj.tempo(player, deck, value, state.decks[deck].tempo_range);
    else if (name.startsWith("eq.")) dj.eq(player, deck, name.slice(3), value);
  }

  #press(player, state, id) {
    if (id === "curve") return dj.curve(player, state.curve === "smooth" ? "cut" : "smooth");
    if (id === "transition") return dj.transition(player, dj.fadeSeconds);
    if (id === "autoLoad") return dj.autoLoad(player, !state.auto_load);
    const deck = Number(id[0]);
    const data = state.decks[deck];
    const name = id.slice(2);
    if (name.startsWith("beatLoop.")) return data.entry ? dj.beatLoop(player, deck, Number(name.slice(9))) : null;
    switch (name) {
      case "play": return data.entry ? dj.play(player, deck, !data.playing) : null;
      case "cue": return data.entry ? dj.jumpToCue(player, deck) : null;
      case "setCue": return data.entry ? dj.setCue(player, deck) : null;
      case "loadNext": return dj.loadNext(player, deck);
      case "eject": return data.entry ? dj.eject(player, deck) : null;
      case "range": return dj.tempoRange(player, deck, data.tempo_range === 8 ? 16 : 8);
      case "sync": return data.bpm ? dj.sync(player, deck) : null;
      case "loopIn": return data.entry ? dj.loopIn(player, deck) : null;
      case "loopOut": return data.entry ? dj.loopOut(player, deck) : null;
      case "loopToggle": return data.entry ? dj.loopToggle(player, deck) : null;
      case "loopHalve": return data.entry ? dj.loopResize(player, deck, 0.5) : null;
      case "loopDouble": return data.entry ? dj.loopResize(player, deck, 2) : null;
      case "loopBack": return data.entry ? dj.loopMove(player, deck, "loop", -1) : null;
      case "loopForward": return data.entry ? dj.loopMove(player, deck, "loop", 1) : null;
      case "quantize": return dj.quantize(player, deck, !data.quantize);
      default: return null;
    }
  }
}

export const midi = new Midi();
