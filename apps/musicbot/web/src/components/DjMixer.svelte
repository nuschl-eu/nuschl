<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { dj } from "../dj.svelte.js";
  import { cueListening } from "../listening.svelte.js";
  import { DJ_TRANSITION_SECONDS, crossfaderText, transitionPosition } from "../logic.js";
  import Icon from "./Icon.svelte";
  import DjSlider from "./DjSlider.svelte";

  let { player, mix, learning = null } = $props();
  const id = $props.id();
  let now = $state(performance.now());
  const transition = $derived(mix.transition);
  $effect(() => {
    if (!transition) return;
    let frame = requestAnimationFrame(function tick() {
      now = performance.now();
      frame = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(frame);
  });
  const since = $derived(Math.max(0, now - store.receivedAt));
  const position = $derived(transition
    ? transitionPosition(mix.crossfader, transition.to, transition.elapsed_ms, transition.duration_ms, since)
    : dj.value(`${player}:crossfader`, mix.crossfader));
  const progress = $derived(transition ? Math.min(1, (transition.elapsed_ms + since) / transition.duration_ms) : 0);
  const target = $derived(transition ? transition.to : position <= 0 ? 1 : 0);
  const targetLetter = $derived(target === 0 ? "A" : "B");
</script>

<section class="mixer card" aria-labelledby="{id}-title">
  <h2 id="{id}-title" class="eyebrow">Mixer</h2>

  <div class="crossfader">
    <DjSlider label="Crossfader" value={position} min={-1} max={1} step="0.01" reset={0} text={crossfaderText(position)}
      control="crossfader" learning={learning === "crossfader"} onchange={(value) => dj.crossfader(player, value)} />
    <div class="ends muted small" aria-hidden="true"><span>A</span><span>B</span></div>
  </div>

  <div class="curve" class:learning={learning === "curve"} data-control="curve" role="radiogroup" aria-label="Crossfader curve">
    <button class="chip" role="radio" aria-checked={mix.curve === "smooth"} title="Constant power: both decks at −3 dB in the middle" onclick={() => dj.curve(player, "smooth")}>Smooth</button>
    <button class="chip" role="radio" aria-checked={mix.curve === "cut"} title="Both decks at full level, cut at the ends" onclick={() => dj.curve(player, "cut")}>Cut</button>
  </div>

  <div class="transition">
    <label class="seconds">
      <span>Fade time</span>
      <select bind:value={dj.fadeSeconds}>
        {#each DJ_TRANSITION_SECONDS as choice (choice)}<option value={choice}>{choice} s</option>{/each}
      </select>
    </label>
    <button class="btn primary" class:learning={learning === "transition"} data-control="transition" disabled={!mix.decks[target].entry} onclick={() => dj.transition(player, dj.fadeSeconds)}>
      <Icon name="decks" size={18} />Fade to {targetLetter}
    </button>
    {#if transition}
      <progress max="1" value={progress} aria-label="Fade to deck {targetLetter}"></progress>
    {/if}
  </div>

  <label class="auto-load" class:learning={learning === "autoLoad"} data-control="autoLoad">
    <input type="checkbox" checked={mix.auto_load} onchange={(event) => dj.autoLoad(player, event.currentTarget.checked)} />
    <span>Auto load: after a fade or at the end of a song, the silent deck takes the next song from its deck queue, else from the main queue</span>
  </label>

  {#if cueListening.deck !== null}
    <label class="cue-volume">
      <span><Icon name="headphones" size={16} /> Headphone cue, deck {cueListening.deck === 0 ? "A" : "B"}</span>
      <input type="range" min="0" max="100" value={cueListening.volume} aria-valuetext="{cueListening.volume} %" oninput={(event) => cueListening.setVolume(Number(event.currentTarget.value))} />
    </label>
  {/if}
</section>

<style>
  .mixer { display: grid; align-content: start; gap: 0.9rem; min-width: 0; padding: 0.9rem; }
  h2 { margin: 0; }
  .crossfader :global(.dj-slider) { justify-items: stretch; text-align: center; }
  .ends { display: flex; justify-content: space-between; margin-top: -0.2rem; font-weight: 700; }
  .curve { display: flex; justify-content: center; gap: 0.35rem; }
  .chip { padding: 0.2rem 0.7rem; border: 1px solid var(--border); border-radius: 999px; background: var(--surface); color: var(--muted); font-size: 0.85rem; }
  .chip[aria-checked="true"] { border-color: var(--accent); background: var(--accent-soft); color: var(--accent); }
  .transition { display: grid; gap: 0.5rem; }
  .seconds { display: flex; align-items: center; justify-content: space-between; gap: 0.5rem; font-size: 0.85rem; color: var(--muted); }
  .seconds select { padding: 0.3rem 0.6rem; }
  progress { width: 100%; }
  .auto-load { display: flex; align-items: flex-start; gap: 0.5rem; font-size: 0.85rem; }
  .auto-load input { margin-top: 0.2rem; flex: none; }
  .cue-volume { display: grid; gap: 0.3rem; font-size: 0.85rem; color: var(--muted); }
  .cue-volume span { display: flex; align-items: center; gap: 0.35rem; }
  .cue-volume input { width: 100%; }
  .learning { outline: 2px dashed var(--accent); outline-offset: 3px; }
</style>
