<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { tick } from "svelte";
  import { store } from "../store.svelte.js";
  import { groupPlaylists } from "../logic.js";
  import MediaCard from "./MediaCard.svelte";
  import Icon from "./Icon.svelte";

  /** The Playlists page below its heading: `playlists` from `api/playlists` for `library`. */
  let { playlists, library, namespace, current } = $props();
  const id = $props.id();
  const groups = $derived(groupPlaylists(playlists));
  let creating = $state(false);
  let name = $state("");
  let shared = $state(false);
  let busy = $state(false);
  let opener = $state();

  async function startCreating() {
    creating = true;
    await tick();
    document.getElementById(`${id}-name`)?.focus();
  }

  async function cancel() {
    creating = false;
    name = "";
    shared = false;
    await tick();
    opener?.focus();
  }

  async function create(event) {
    event.preventDefault();
    if (busy || !name.trim()) return;
    busy = true;
    const made = await store.playlistChange("api/playlists/create", { library, name, shared }, namespace, { reply: true });
    busy = false;
    if (made === null) return;
    name = "";
    shared = false;
    store.navigate({ view: "playlist", library: made.library, id: made.id, namespace });
  }
</script>

<div class="create-bar">
  <button class="btn primary" bind:this={opener} disabled={!current || !store.canOwnPlaylists} aria-expanded={creating} aria-describedby={store.canOwnPlaylists ? undefined : `${id}-certificate`} onclick={startCreating}><Icon name="add" size={18} /> New playlist</button>
  {#if !store.canOwnPlaylists}
    <p id="{id}-certificate" class="muted small">Owning and editing playlists needs a Mumble certificate. Set one up in Mumble, then send <code>!web</code> for a new link. Viewing and queueing work as they are.</p>
  {/if}
</div>
{#if creating && store.canOwnPlaylists}
  <form class="create card" onsubmit={create} aria-label="New playlist">
    <label class="eyebrow" for="{id}-name">Name of the new playlist</label>
    <div class="create-row">
      <input id="{id}-name" type="text" maxlength="100" placeholder="Playlist name" autocomplete="off" bind:value={name} disabled={!current} onkeydown={(event) => { if (event.key === "Escape") { event.preventDefault(); cancel(); } }} />
      <label class="check"><input type="checkbox" bind:checked={shared} disabled={!current} /> Everyone may edit the songs</label>
      <button class="btn primary" type="submit" disabled={!current || busy || !name.trim()}>Create</button>
      <button class="btn" type="button" onclick={cancel}>Cancel</button>
    </div>
  </form>
{/if}

<section class="group" aria-labelledby="{id}-mine">
  <h2 id="{id}-mine">Your playlists</h2>
  {#if groups.mine.length}
    <div class="media-grid">{#each groups.mine as playlist (playlist.id)}<MediaCard item={playlist} kind="playlist" {namespace} />{/each}</div>
  {:else}
    <p class="muted">Your Favorites appear here after the first ♥ in the player, together with the playlists you make.</p>
  {/if}
</section>

{#if groups.others.length}
  <section class="group" aria-labelledby="{id}-others">
    <h2 id="{id}-others">From other people</h2>
    <div class="media-grid">{#each groups.others as playlist (playlist.id)}<MediaCard item={playlist} kind="playlist" {namespace} />{/each}</div>
  </section>
{/if}

<section class="group" aria-labelledby="{id}-library">
  <h2 id="{id}-library">From the library</h2>
  {#if groups.library.length}
    <div class="media-grid">{#each groups.library as playlist (playlist.id)}<MediaCard item={playlist} kind="playlist" {namespace} />{/each}</div>
  {:else}
    <p class="muted">Playlists made in the library itself appear here, read-only.</p>
  {/if}
</section>

<style>
  .create-bar { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem 1rem; margin: 0 0 1.25rem; }
  .create-bar p { margin: 0; max-width: 40rem; }
  .create { display: grid; gap: 0.5rem; margin: 0 0 1.5rem; padding: 0.9rem 1rem; }
  .create-row { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem 0.75rem; }
  .create-row input[type="text"] { flex: 1; min-width: 12rem; }
  .check { display: flex; align-items: center; gap: 0.4rem; color: var(--muted); font-size: 0.9rem; }
  .group { margin-bottom: 2rem; }
</style>
