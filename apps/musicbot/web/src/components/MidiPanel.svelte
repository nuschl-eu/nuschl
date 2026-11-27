<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { midi } from "../midi.svelte.js";
  import { bindingText } from "../logic.js";
  import Icon from "./Icon.svelte";

  const id = $props.id();
  const learningLabel = $derived(midi.controls.find((control) => control.id === midi.learning)?.label ?? null);
  const mapped = $derived(Object.keys(midi.mapping).length);

  function keydown(event) {
    if (event.key === "Escape" && midi.learning !== null) {
      event.preventDefault();
      midi.cancelLearn();
    }
  }
</script>

<svelte:window onkeydown={keydown} />

<section class="midi card" aria-labelledby="{id}-title">
  <div class="midi-heading">
    <h2 id="{id}-title"><Icon name="midi" size={18} /> MIDI controller</h2>
    {#if midi.supported}
      <label class="enable">
        <input type="checkbox" checked={midi.enabled} onchange={(event) => (event.currentTarget.checked ? midi.enable() : midi.disable())} />
        <span>Use a MIDI controller</span>
      </label>
    {/if}
  </div>

  {#if !midi.supported}
    <p class="muted small">MIDI controllers work in browsers with Web MIDI, such as Chromium-based browsers, over HTTPS.</p>
  {:else if midi.error}
    <p class="error small" role="alert">{midi.error}</p>
  {:else if midi.enabled}
    {#if !midi.inputs.length}
      <p class="muted small" role="status">Connect a MIDI controller; it shows up here.</p>
    {:else}
      <div class="device">
        <label>
          <span>Device</span>
          <select value={midi.input} onchange={(event) => midi.select(event.currentTarget.value)}>
            {#each midi.inputs as input (input.id)}<option value={input.id}>{input.name}</option>{/each}
          </select>
        </label>
        <span class="muted small">{mapped} of {midi.controls.length} controls mapped; the mapping stays in this browser for this device.</span>
        <label class="learn-mode"><input type="checkbox" checked={midi.learnMode} onchange={(event) => { midi.learnMode = event.currentTarget.checked; if (!midi.learnMode) midi.cancelLearn(); }} /><span>Learn on the decks</span></label>
        <button class="btn small" disabled={!mapped} onclick={() => midi.clearAll()}>Clear mapping</button>
      </div>
      <p class="learn-status small" class:active={learningLabel !== null} role="status">
        {#if learningLabel !== null}
          Move a fader, knob or button on the controller for <strong>{learningLabel}</strong>. Escape cancels.
        {:else if midi.learnMode}
          Click a control on the decks or the mixer, then move the fader, knob or button that should drive it.
        {:else}
          Choose Learn beside a control, then move the fader, knob or button that should drive it.
        {/if}
      </p>
      <table class="mapping">
        <thead><tr><th scope="col">Control</th><th scope="col">Controller</th><th scope="col">Mode</th><th scope="col"><span class="visually-hidden">Actions</span></th></tr></thead>
        <tbody>
          {#each midi.controls as control (control.id)}
            {@const binding = midi.mapping[control.id]}
            <tr class:learning={midi.learning === control.id}>
              <th scope="row">{control.label}</th>
              <td class="num">{midi.learning === control.id ? "Waiting…" : bindingText(binding)}</td>
              <td>
                {#if binding && control.kind === "range" && binding.mode !== "fine"}
                  <select aria-label="Mode of {control.label}" value={binding.mode} onchange={(event) => midi.setMode(control.id, event.currentTarget.value)}>
                    <option value="absolute">Absolute</option>
                    <option value="relative">Relative encoder</option>
                  </select>
                {:else if binding}
                  <span class="muted small">{binding.mode === "fine" ? "14-bit" : "Press"}</span>
                {/if}
              </td>
              <td class="actions">
                {#if midi.learning === control.id}
                  <button class="btn small" onclick={() => midi.cancelLearn()}>Cancel</button>
                {:else}
                  <button class="btn small" aria-label="Learn {control.label}" onclick={() => midi.learn(control.id)}>Learn</button>
                {/if}
                <button class="icon-btn" disabled={!binding} aria-label="Clear {control.label}" title="Clear" onclick={() => midi.clear(control.id)}><Icon name="remove" size={16} /></button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      <p class="muted small">The headphone cue buttons start browser audio and need a click on the page.</p>
    {/if}
  {/if}
</section>

<style>
  .midi { margin-top: 1rem; padding: 0.9rem 1rem; }
  .midi-heading { display: flex; align-items: center; justify-content: space-between; gap: 0.75rem; flex-wrap: wrap; }
  h2 { display: flex; align-items: center; gap: 0.4rem; margin: 0; font-size: 1rem; }
  .enable { display: flex; align-items: center; gap: 0.5rem; font-size: 0.9rem; }
  .device { display: flex; align-items: center; flex-wrap: wrap; gap: 0.5rem 1rem; margin-top: 0.75rem; }
  .device label { display: flex; align-items: center; gap: 0.5rem; font-size: 0.9rem; }
  .device select { max-width: 16rem; padding: 0.3rem 0.6rem; }
  .btn.small { padding: 0.25rem 0.65rem; font-size: 0.85rem; }
  .learn-mode { display: flex; align-items: center; gap: 0.4rem; font-size: 0.9rem; }
  .learn-status { margin: 0.75rem 0 0.5rem; padding: 0.5rem 0.75rem; border-radius: var(--radius); background: var(--surface-2); }
  .learn-status.active { background: var(--accent-soft); color: var(--accent); }
  .mapping { width: 100%; border-collapse: collapse; font-size: 0.85rem; }
  .mapping th, .mapping td { padding: 0.3rem 0.5rem; border-bottom: 1px solid var(--border); text-align: left; }
  .mapping thead th { color: var(--muted); font-size: 0.75rem; font-weight: 600; letter-spacing: 0.04em; text-transform: uppercase; }
  .mapping tbody th { font-weight: 500; }
  .mapping select { padding: 0.2rem 0.5rem; font-size: 0.8rem; }
  .mapping tr.learning { background: var(--accent-soft); }
  .actions { display: flex; align-items: center; justify-content: flex-end; gap: 0.25rem; }
  @media (max-width: 600px) {
    .mapping thead { display: none; }
    .mapping tr { display: grid; grid-template-columns: minmax(0, 1fr) auto; padding: 0.35rem 0; border-bottom: 1px solid var(--border); }
    .mapping th, .mapping td { border: none; padding: 0.1rem 0.25rem; }
    .mapping tbody th { grid-column: 1 / -1; }
  }
</style>
