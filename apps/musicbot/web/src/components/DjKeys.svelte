<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { keys, actionTitle } from "../keys.svelte.js";
  import Icon from "./Icon.svelte";

  /** `player` is the selected queue's index, `mix` its DJ state. */
  let { player, mix } = $props();
  const id = $props.id();
  const groups = ["Deck A", "Mixer", "Deck B"].map((name) => ({ name, actions: keys.actions.filter((action) => action.group === name) }));
  const learningAction = $derived(keys.actions.find((action) => action.id === keys.learning) ?? null);
  let dialog = $state(null);
  let opener = $state(null);
  let returnFocus = null;

  // A queue change ends the glides of the previous queue.
  $effect(() => {
    void player;
    return () => keys.stop();
  });

  $effect(() => () => {
    keys.stop();
    keys.cancelLearn();
    keys.open = false;
  });

  $effect(() => {
    if (!dialog) return;
    if (keys.open && !dialog.open) {
      const active = document.activeElement;
      returnFocus = active instanceof HTMLElement && active !== document.body ? active : opener;
      keys.note = "";
      keys.loadLayout();
      dialog.showModal();
    } else if (!keys.open && dialog.open) {
      dialog.close();
    }
  });

  function closed() {
    keys.open = false;
    keys.cancelLearn();
    const target = returnFocus?.isConnected ? returnFocus : opener;
    returnFocus = null;
    target?.focus();
  }

  // Escape while a key is awaited ends learning and keeps the panel open.
  function cancel(event) {
    if (keys.learning === null) return;
    event.preventDefault();
    keys.cancelLearn();
  }
</script>

<svelte:window
  onkeydowncapture={(event) => keys.keydown(event, player, mix)}
  onkeyupcapture={(event) => keys.keyup(event)}
  onblur={() => keys.stop()}
/>

<button class="btn small" bind:this={opener} aria-haspopup="dialog" title="Keyboard shortcuts ({keys.label('help')})" onclick={() => (keys.open = true)}>
  <span class="keycap" aria-hidden="true">⌨</span>Keyboard shortcuts
</button>

<dialog class="keys card" bind:this={dialog} aria-labelledby="{id}-title" aria-describedby="{id}-help" onclose={closed} oncancel={cancel}>
  <div class="keys-heading">
    <h2 id="{id}-title"><span class="keycap" aria-hidden="true">⌨</span> Keyboard shortcuts</h2>
    <button class="icon-btn" aria-label="Close" title="Close" onclick={() => (keys.open = false)}><Icon name="close" size={18} /></button>
  </div>
  <p id="{id}-help" class="muted small">
    Shortcuts drive the selected queue's decks while the DJ view is open and the focus is outside text fields. Hold a fader key to keep it moving.
    Keys keep their place on the keyboard in every layout. Space plays and pauses the queue until an action takes it.
  </p>
  <p class="learn-status small" class:active={learningAction !== null} role="status">
    {#if learningAction !== null}
      Press the key for <strong>{actionTitle(learningAction)}</strong>, with Shift if you like. Escape cancels.
    {:else if keys.note}
      {keys.note}
    {:else}
      Choose Change beside an action, then press its new key.
    {/if}
  </p>
  <div class="groups">
    {#each groups as group (group.name)}
      <table class="mapping">
        <caption class="eyebrow">{group.name}</caption>
        <thead class="visually-hidden"><tr><th scope="col">Action</th><th scope="col">Key</th><th scope="col">Change</th></tr></thead>
        <tbody>
          {#each group.actions as action (action.id)}
            {@const title = actionTitle(action)}
            <tr class:learning={keys.learning === action.id}>
              <th scope="row">{action.label}</th>
              <td>{#if keys.learning === action.id}<span class="muted">Waiting…</span>{:else if keys.bindings[action.id]}<kbd>{keys.label(action.id)}</kbd>{:else}<span class="muted">—</span>{/if}</td>
              <td class="actions">
                {#if keys.learning === action.id}
                  <button class="btn small" onclick={() => keys.cancelLearn()}>Cancel</button>
                {:else}
                  <button class="btn small" aria-label="Change key for {title}" onclick={() => keys.learn(action.id)}>Change</button>
                {/if}
                <button class="icon-btn" disabled={!keys.bindings[action.id]} aria-label="Unbind {title}" title="Unbind" onclick={() => keys.clear(action.id)}><Icon name="remove" size={16} /></button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/each}
  </div>
  <div class="keys-footer">
    <span class="muted small">The keys stay in this browser.</span>
    <button class="btn small" onclick={() => keys.reset()}>Reset to defaults</button>
  </div>
</dialog>

<style>
  .btn.small { padding: 0.25rem 0.65rem; font-size: 0.85rem; }
  .keycap { font-size: 1.1em; line-height: 1; }
  .keys { width: min(68rem, calc(100vw - 2rem)); max-height: calc(100vh - 2rem); padding: 0.9rem 1rem; color: var(--fg); overflow: auto; }
  .keys::backdrop { background: rgb(0 0 0 / 45%); }
  .keys-heading { display: flex; align-items: center; justify-content: space-between; gap: 0.75rem; }
  h2 { display: flex; align-items: center; gap: 0.4rem; margin: 0; font-size: 1rem; }
  .learn-status { margin: 0.75rem 0; padding: 0.5rem 0.75rem; border-radius: var(--radius); background: var(--surface-2); }
  .learn-status.active { background: var(--accent-soft); color: var(--accent); }
  .groups { display: grid; grid-template-columns: repeat(auto-fit, minmax(19rem, 1fr)); gap: 0.5rem 1.25rem; align-items: start; }
  .mapping { width: 100%; border-collapse: collapse; font-size: 0.85rem; }
  .mapping caption { padding: 0.3rem 0.5rem; text-align: left; }
  .mapping th, .mapping td { padding: 0.25rem 0.5rem; border-bottom: 1px solid var(--border); text-align: left; }
  .mapping tbody th { font-weight: 500; }
  .mapping tr.learning { background: var(--accent-soft); }
  kbd { display: inline-block; min-width: 1.6em; padding: 0.05rem 0.4rem; border: 1px solid var(--border); border-bottom-width: 2px; border-radius: 6px; background: var(--surface-2); font: inherit; font-size: 0.8rem; text-align: center; white-space: nowrap; }
  .actions { display: flex; align-items: center; justify-content: flex-end; gap: 0.25rem; }
  .keys-footer { display: flex; align-items: center; justify-content: space-between; gap: 0.75rem; flex-wrap: wrap; margin-top: 0.75rem; }
</style>
