<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { dj } from "../dj.svelte.js";
  import Icon from "./Icon.svelte";

  /**
   * While DJ mode is on: one button per deck that adds `item`
   * (`{ kind, library, id, namespace }`) to that deck's queue. `button` gives
   * the labelled button style of detail headers; the default is a compact
   * "A"/"B" icon button.
   */
  let { item, title, disabled = false, button = false } = $props();
  const on = $derived(Boolean(store.player?.dj));
</script>

{#if on}
  {#each ["A", "B"] as letter, deck (letter)}
    <button
      class={button ? "btn" : "icon-btn"}
      class:deck-add={!button}
      {disabled}
      aria-label="Add {title} to deck {letter} queue"
      title="Add to deck {letter} queue"
      onclick={() => dj.queueAdd(store.selected, deck, item)}
    >{#if button}<Icon name="add" />Deck {letter} queue{:else}{letter}{/if}</button>
  {/each}
{/if}

<style>
  .deck-add { font-weight: 700; font-size: 0.85rem; }
</style>
