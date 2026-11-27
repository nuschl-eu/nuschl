// SPDX-License-Identifier: EUPL-1.2
// Keyboard shortcuts for the DJ view: bindings of DJ actions to physical
// keys kept in this browser's localStorage, a learn mode that binds an
// action to the next key pressed, and the dispatch of presses to the
// selected queue's decks. Continuous controls move one step per press and
// glide while the key stays down (rates in logic.js, DJ_KEY_GLIDES).
import { store } from "./store.svelte.js";
import { dj } from "./dj.svelte.js";
import { cueListening } from "./listening.svelte.js";
import {
  DJ_EQ_MIN, DJ_SEND_MS, bindKey, clamp, deckPosition, djKeyActions, eqKillTarget, glideValue, keyFromEvent,
  keyLabel, keyMatches, parseKeyBindings, serializeKeyBindings, typingTarget,
} from "./logic.js";

const STORAGE_KEY = "nuschl-musicbot:keys";
const SEEK_MS = 4000;

function stored() {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function remember(value) {
  try {
    if (value === null) localStorage.removeItem(STORAGE_KEY);
    else localStorage.setItem(STORAGE_KEY, value);
  } catch {
    // Storage may be disabled; the bindings then last for this page only.
  }
}

/** An action's name with its group, such as "Play or pause, deck A". */
export function actionTitle(action) {
  return `${action.label}, ${action.group === "Mixer" ? "mixer" : action.group.replace("Deck", "deck")}`;
}

class Keys {
  actions = djKeyActions();
  /** `{ actionId: { code, shift } | null }` for every action. */
  bindings = $state.raw(parseKeyBindings(stored(), this.actions));
  /** The action id the next key press is bound to, or null. */
  learning = $state(null);
  /** What the last change of a binding did, for the panel's status line. */
  note = $state("");
  /** True while the shortcuts panel is open; the DJ shortcuts rest meanwhile. */
  open = $state(false);
  /** This keyboard's printed characters by code, where the browser tells them. */
  layout = $state.raw(null);
  /** Running glides by the `code` of the key that holds them. */
  #glides = new Map();
  #timer = null;
  /** EQ values before a kill, by `player:deck:eq:band`. */
  #killed = new Map();

  async loadLayout() {
    try {
      const map = await navigator.keyboard?.getLayoutMap?.();
      if (map) this.layout = map;
    } catch {
      // The US names of the keys stay.
    }
  }

  label(id) {
    return keyLabel(this.bindings[id], this.layout);
  }

  learn(id) {
    this.stop();
    this.note = "";
    this.learning = id;
  }

  cancelLearn() {
    this.learning = null;
  }

  bind(id, key) {
    const { bindings, moved } = bindKey(this.bindings, id, key);
    this.bindings = bindings;
    this.learning = null;
    const action = this.actions.find((item) => item.id === id);
    const others = moved.map((other) => actionTitle(this.actions.find((item) => item.id === other)));
    this.note = others.length
      ? `${keyLabel(key, this.layout)} moved from ${others.join(" and ")} to ${actionTitle(action)}; ${others.join(" and ")} ${others.length > 1 ? "are" : "is"} unbound now.`
      : `${actionTitle(action)}: ${keyLabel(key, this.layout)}.`;
    remember(serializeKeyBindings(this.bindings));
  }

  clear(id) {
    this.bindings = { ...this.bindings, [id]: null };
    this.note = "";
    remember(serializeKeyBindings(this.bindings));
  }

  reset() {
    this.bindings = parseKeyBindings(null, this.actions);
    this.learning = null;
    this.note = "Every action has its default key again.";
    remember(null);
  }

  /** The window's capture-phase keydown on the DJ view of queue `player` with DJ state `mix`. */
  keydown(event, player, mix) {
    if (this.learning !== null) {
      // Escape reaches the panel's dialog, which ends learning.
      const key = keyFromEvent(event);
      if (!key) return;
      event.preventDefault();
      event.stopPropagation();
      if (!event.repeat) this.bind(this.learning, key);
      return;
    }
    if (this.open || !mix || typingTarget(event.target)) return;
    const key = keyFromEvent(event);
    if (!key) return;
    const action = this.actions.find((item) => keyMatches(this.bindings[item.id], key));
    if (!action) return;
    // The binding takes the key from the page: Space from play and pause, arrows from a focused slider.
    event.preventDefault();
    event.stopPropagation();
    if (!event.repeat) this.#run(action, event.code, player, mix);
  }

  keyup(event) {
    if (this.#glides.delete(event.code) && this.#glides.size === 0) this.stop();
  }

  /** Ends every glide: on key release, blur, a queue change and when the view goes. */
  stop() {
    this.#glides.clear();
    clearInterval(this.#timer);
    this.#timer = null;
  }

  #run(action, code, player, mix) {
    if (action.glide) return this.#startGlide(action, code, player, mix);
    switch (action.id) {
      case "help":
        this.open = true;
        return null;
      case "transition": return dj.transition(player, dj.fadeSeconds);
      case "autoLoad": return dj.autoLoad(player, !mix.auto_load);
      case "crossfaderA":
      case "crossfaderB":
        this.#stopControl(`${player}:crossfader`);
        return dj.crossfader(player, action.id === "crossfaderA" ? -1 : 1);
      default: break;
    }
    const { deck } = action;
    const data = mix.decks[deck];
    if (action.band) return this.#kill(player, deck, data, action.band);
    switch (action.name) {
      // Runs inside the key event: browser audio starts only from a user gesture.
      case "headphones": return cueListening.toggle(player, deck);
      case "loadNext": return dj.loadNext(player, deck);
      case "sync": return data.bpm ? dj.sync(player, deck) : null;
      default: break;
    }
    if (!data.entry) return null;
    if (action.beats) return dj.beatLoop(player, deck, action.beats);
    switch (action.name) {
      case "play": return dj.play(player, deck, !data.playing);
      case "cue": return dj.jumpToCue(player, deck);
      case "setCue": return dj.setCue(player, deck);
      case "seekBack": return this.#seek(player, deck, data, -1);
      case "seekForward": return this.#seek(player, deck, data, 1);
      case "loopIn": return dj.loopIn(player, deck);
      case "loopOut": return dj.loopOut(player, deck);
      case "loopToggle": return dj.loopToggle(player, deck);
      case "loopHalve": return dj.loopResize(player, deck, 0.5);
      case "loopDouble": return dj.loopResize(player, deck, 2);
      case "loopBack": return dj.loopMove(player, deck, "loop", -1);
      case "loopForward": return dj.loopMove(player, deck, "loop", 1);
      default: return null;
    }
  }

  #seek(player, deck, data, direction) {
    const tempo = dj.value(`${player}:${deck}:tempo`, data.tempo);
    const duration = data.entry.track?.duration ?? null;
    const position = deckPosition(data.position_ms, data.advancing, tempo, store.receivedAt, performance.now(), duration);
    const end = typeof duration === "number" && duration > 0 ? duration * 1000 : Infinity;
    return dj.seek(player, deck, clamp(position + direction * SEEK_MS, 0, end));
  }

  #kill(player, deck, data, band) {
    const key = `${player}:${deck}:eq:${band}`;
    const current = dj.value(key, data.eq[band]);
    const target = eqKillTarget(current, this.#killed.get(key));
    if (target === DJ_EQ_MIN) this.#killed.set(key, current);
    else this.#killed.delete(key);
    dj.eq(player, deck, band, target);
  }

  /** The control a glide moves: its `dj.local` key, current value, tempo range and setter. */
  #target(control, deck, player, mix) {
    if (control === "crossfader") {
      const key = `${player}:crossfader`;
      return { key, value: dj.value(key, mix.crossfader), range: 1, set: (value) => dj.crossfader(player, value) };
    }
    const data = mix.decks[deck];
    const key = `${player}:${deck}:${control}`;
    const range = data.tempo_range;
    const set = control === "volume" ? (value) => dj.volume(player, deck, value)
      : control === "filter" ? (value) => dj.filter(player, deck, value)
      : (value) => dj.tempo(player, deck, value, range);
    return { key, value: dj.value(key, data[control]), range, set };
  }

  #startGlide(action, code, player, mix) {
    const { control, direction } = action.glide;
    const target = this.#target(control, action.deck, player, mix);
    this.#stopControl(target.key);
    const glide = { ...target, control, direction, start: performance.now(), last: null };
    this.#glides.set(code, glide);
    this.#move(glide, glide.start);
    this.#timer ??= setInterval(() => {
      const now = performance.now();
      for (const running of this.#glides.values()) this.#move(running, now);
    }, DJ_SEND_MS);
  }

  #move(glide, now) {
    const value = glideValue(glide.control, glide.value, glide.direction, now - glide.start, glide.range);
    if (value === glide.last) return;
    glide.last = value;
    glide.set(value);
  }

  #stopControl(key) {
    for (const [code, glide] of this.#glides) if (glide.key === key) this.#glides.delete(code);
    if (this.#glides.size === 0) this.stop();
  }
}

export const keys = new Keys();
