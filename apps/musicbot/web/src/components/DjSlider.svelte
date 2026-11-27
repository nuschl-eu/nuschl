<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  /**
   * A labelled DJ fader or knob as a range input. `text` is the value spoken
   * (aria-valuetext) and shown unless `shown` gives a shorter form; a double
   * click returns to `reset`. `control` names it for MIDI learn, and
   * `learning` marks the control MIDI learn waits for.
   */
  let { label, value, min, max, step, text, shown = null, vertical = false, reset = null, disabled = false, control = null, learning = false, onchange } = $props();
</script>

<label class="dj-slider" class:vertical class:learning data-control={control}>
  <span class="slider-label">{label}</span>
  <input
    type="range"
    {min}
    {max}
    {step}
    {value}
    {disabled}
    aria-valuetext={text}
    oninput={(event) => onchange(Number(event.currentTarget.value))}
    ondblclick={() => { if (reset !== null && !disabled) onchange(reset); }}
  />
  <span class="slider-value num" aria-hidden="true">{shown ?? text}</span>
</label>

<style>
  .dj-slider { display: grid; justify-items: center; gap: 0.25rem; min-width: 0; color: var(--muted); font-size: 0.75rem; }
  .slider-label { font-size: 0.68rem; font-weight: 600; letter-spacing: 0.04em; text-transform: uppercase; white-space: nowrap; }
  .slider-value { color: var(--fg); white-space: nowrap; font-size: 0.72rem; }
  input { width: 100%; margin: 0; }
  .vertical input { writing-mode: vertical-lr; direction: rtl; width: 1.6rem; height: 8.5rem; }
  .learning { outline: 2px dashed var(--accent); outline-offset: 4px; border-radius: var(--radius); }
</style>
