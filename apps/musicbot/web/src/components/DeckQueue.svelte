<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { dj } from "../dj.svelte.js";
  import { buildRoute, formatDuration, gapIndex, stepTarget } from "../logic.js";
  import { carriesEntry, carriesItem, droppedEntry, droppedItem, entryDropIndex, leftElement, startEntryDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";

  /** The queue of `deck` on `player`: its songs load onto the deck before the main queue's. */
  let { player, deck, entries } = $props();
  const letter = $derived(deck === 0 ? "A" : "B");
  const other = $derived(deck === 0 ? "B" : "A");
  const id = $props.id();
  let root = $state();
  let gap = $state(null);
  let dragged = $state(null);

  function dropGap(event) {
    const row = event.target.closest?.("[data-deck-queue-index]");
    if (row && root.contains(row)) {
      const rect = row.getBoundingClientRect();
      return gapIndex(Number(row.dataset.deckQueueIndex), event.clientY - rect.top, rect.height);
    }
    return entries.length;
  }

  function dragover(event) {
    if (!carriesItem(event) && !carriesEntry(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = carriesItem(event) ? "copy" : "move";
    gap = dropGap(event);
  }

  function drop(event) {
    const target = dropGap(event);
    gap = null;
    dragged = null;
    if (carriesItem(event)) {
      const item = droppedItem(event);
      if (!item) return;
      event.preventDefault();
      dj.queueAdd(player, deck, item, target);
    } else if (carriesEntry(event)) {
      const entry = droppedEntry(event);
      event.preventDefault();
      if (entry === null) return;
      const to = entryDropIndex(entries, entry, target);
      if (to !== null) dj.queueMove(player, entry, deck, to);
    }
  }

  function reorder(event, entry) {
    if (!event.altKey || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) return;
    event.preventDefault();
    event.stopPropagation();
    const from = entries.findIndex((item) => item.id === entry);
    const to = stepTarget(from, event.key === "ArrowUp" ? -1 : 1, entries.length);
    if (to !== null) dj.queueMove(player, entry, deck, to);
  }

  const toOtherDeck = (entry) => dj.queueMove(player, entry, 1 - deck, store.snapshot?.players[player]?.deck_queues?.[1 - deck]?.length ?? 0);
  const toMainQueue = (entry) => dj.queueMove(player, entry, null, store.queue(player).length);
</script>

<section class="deck-queue card" bind:this={root} aria-labelledby="{id}-title" ondragover={dragover} ondrop={drop} ondragleave={(event) => { if (leftElement(event, root)) gap = null; }}>
  <div class="queue-head">
    <h2 id="{id}-title" class="eyebrow">Deck {letter} queue <span class="badge num">{entries.length}</span></h2>
    {#if entries.length}<button class="btn small" aria-label="Clear the deck {letter} queue" onclick={() => dj.queueClear(player, deck)}>Clear</button>{/if}
  </div>
  {#if entries.length}
    <p class="visually-hidden" id="{id}-help">Use the move handle and Alt plus Up or Down arrow to reorder a track.</p>
    <ol class="rows" aria-labelledby="{id}-title">
      {#each entries as entry, i (entry.id)}
        {@const track = entry.track}
        <li data-deck-queue-index={i} draggable="true" class:drop-before={gap === i} class:drop-after={gap === entries.length && i === entries.length - 1} class:dragged={dragged === entry.id} ondragstart={(event) => { dragged = entry.id; startEntryDrag(event, entry.id); }} ondragend={() => { dragged = null; gap = null; }}>
          <button class="icon-btn grip" draggable="true" aria-label="Move {track.title}" aria-describedby="{id}-help" title="Drag to reorder, or Alt + ↑ / ↓" onkeydown={(event) => reorder(event, entry.id)}><Icon name="grip" /></button>
          <Cover library={track.library} art={track.cover_art} size={96} px={40} />
          <div class="row-text">
            <div class="row-title" title={track.title}>{#if track.library != null && track.id}<a href={buildRoute({ view: "song", library: track.library, id: track.id, namespace: store.browseKey })} draggable="false">{track.title}</a>{:else}{track.title}{/if}</div>
            <div class="muted secondary" title={track.artist ?? ""}>{track.artist ?? "Unknown artist"}</div>
          </div>
          <span class="row-duration">{formatDuration(track.duration)}</span>
          <div class="row-actions">
            <button class="icon-btn" aria-label="Load {track.title} onto deck {letter} now" title="Load now" onclick={() => dj.load(player, deck, { entry: entry.id })}><Icon name="play" size={18} /></button>
            <button class="icon-btn deck-move" aria-label="Move {track.title} to the deck {other} queue" title="Move to the deck {other} queue" onclick={() => toOtherDeck(entry.id)}>{other}</button>
            <button class="icon-btn" aria-label="Move {track.title} to the main queue" title="Move to the main queue" onclick={() => toMainQueue(entry.id)}><Icon name="queue" size={18} /></button>
            <button class="icon-btn" aria-label="Remove {track.title} from the deck {letter} queue" title="Remove" onclick={() => dj.queueRemove(player, entry.id)}><Icon name="remove" size={18} /></button>
          </div>
        </li>
      {/each}
    </ol>
  {/if}
  <p class="drop-end muted small" class:target={gap === entries.length}>
    {#if gap === entries.length}Drop here to add at the end
    {:else if entries.length}Drop music here to add it to this queue
    {:else}Deck {letter}'s queue is empty. Drop songs, albums or playlists here, or use the {letter} buttons in the library and the queue.{/if}
  </p>
</section>

<style>
  .deck-queue { min-width: 0; padding: 0.6rem 0.75rem; }
  .queue-head { display: flex; align-items: center; justify-content: space-between; gap: 0.5rem; min-height: 2rem; }
  h2 { display: flex; align-items: center; gap: 0.4rem; margin: 0; }
  .btn.small { padding: 0.25rem 0.7rem; }
  .rows { margin: 0.35rem 0 0; padding: 0; }
  li { position: relative; gap: 0.5rem; min-height: 3rem; cursor: grab; }
  li.drop-before::before, li.drop-after::after { content: ""; position: absolute; left: 0.5rem; right: 0.5rem; height: 3px; border-radius: 999px; background: var(--accent); z-index: 1; }
  li.drop-before::before { top: -2px; }
  li.drop-after::after { bottom: -2px; }
  li.dragged { opacity: 0.45; }
  .grip { width: 1.6rem; cursor: grab; }
  .secondary { font-size: 0.85rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .row-actions { display: flex; }
  .row-actions .icon-btn { width: 1.9rem; height: 1.9rem; }
  .deck-move { font-weight: 700; font-size: 0.85rem; }
  .drop-end { margin: 0.4rem 0 0; padding: 0.6rem; text-align: center; border: 2px dashed transparent; border-radius: calc(var(--radius) + 4px); }
  .drop-end.target { border-color: var(--accent); background: var(--accent-soft); }
  @media (max-width: 480px) { .row-duration { display: none; } li { gap: 0.35rem; } }
</style>
