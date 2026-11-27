<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { buildRoute, formatDuration } from "../logic.js";
  import { startItemDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";
  import DeckQueueAdd from "./DeckQueueAdd.svelte";

  let { item, namespace, kind = "album" } = $props();
  const current = $derived(store.browseItemCurrent({ library: item.library, namespace }));
  const addable = $derived(kind !== "artist" && current);
  const href = $derived(buildRoute({ view: kind, library: item.library, id: item.id, namespace }));
  const subtitle = $derived(kind === "album" ? item.artist
    : kind === "playlist" ? (item.personal ? (item.mine ? "Yours" : `By ${item.owner_name}`) : item.owner)
    : item.album_count == null ? null : `${item.album_count} albums`);
</script>

<article class="tile card" class:artist={kind === "artist"} draggable={addable} ondragstart={(event) => {
  if (addable) startItemDrag(event, kind, item.library, item.id, item.name, namespace);
  else event.preventDefault();
}}>
  <a class="art-link" {href} draggable="false" aria-label="Open {item.name}">
    <Cover library={item.library} art={item.cover_art} round={kind === "artist"} />
  </a>
  <a class="tile-title" {href} draggable="false" title={item.name}>{item.name}</a>
  {#if subtitle}<div class="muted tile-sub" title={subtitle}>{subtitle}</div>{/if}
  {#if kind === "playlist" && (item.favorites || item.shared)}
    <div class="tile-badges">
      {#if item.favorites}<span class="badge accent">Favorites</span>{/if}
      {#if item.shared}<span class="badge success">Shared</span>{/if}
    </div>
  {/if}
  {#if item.year || item.song_count != null}
    <div class="muted tile-meta">
      {#if item.year}<span>{item.year}</span>{/if}
      {#if item.song_count != null}<span>{item.song_count} tracks</span>{/if}
      {#if item.duration != null}<span>{formatDuration(item.duration)}</span>{/if}
    </div>
  {/if}
  {#if addable}
    <div class="tile-actions">
      <button class="icon-btn" disabled={!store.player || !current} aria-label="Add {item.name} to queue" title="Add to queue" onclick={() => store.add(store.selected, { kind, library: item.library, id: item.id, namespace })}><Icon name="add" /></button>
      <button class="icon-btn" disabled={!store.player || !current} aria-label="Play {item.name} next" title="Play next" onclick={() => store.add(store.selected, { kind, library: item.library, id: item.id, namespace }, 0)}><Icon name="next" /></button>
      <DeckQueueAdd item={{ kind, library: item.library, id: item.id, namespace }} title={item.name} disabled={!current} />
    </div>
  {/if}
</article>

