<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { query } from "../api.js";
  import { formatDuration, totalDuration } from "../logic.js";
  import SongList from "./SongList.svelte";
  import Icon from "./Icon.svelte";

  /**
   * A genre's songs, one page of `PAGE` at a time. Add all and Play all next
   * queue the page on screen; the bot asks the library for that page again.
   */
  let { library, name, namespace, current } = $props();
  const PAGE = 50;
  let page = $state(0);
  const songs = resource(() => (current && store.browseItemCurrent({ library, namespace })
    ? query("api/genre/songs", { library, name, page }) : null), () => store.browseSignal);
  const list = $derived(songs.data?.songs ?? []);
  // `page` changes before its songs arrive; the buttons wait for them.
  const ready = $derived(current && store.player && !songs.loading && list.length > 0);
  const item = $derived({ kind: "genre", library, id: name, page, namespace });

  function changePage(next) {
    if (!current) return;
    page = next;
    document.getElementById("genre-songs")?.scrollIntoView({ block: "start" });
  }
</script>

<section class="group" aria-labelledby="genre-songs" aria-busy={songs.loading}>
  <div class="songs-heading">
    <h2 id="genre-songs">Songs</h2>
    <div class="detail-actions">
      <button class="btn primary" disabled={!ready} title="Add the songs on this page" onclick={() => store.add(store.selected, item)}><Icon name="add" /> Add all</button>
      <button class="btn" disabled={!ready} title="Play the songs on this page next" onclick={() => store.add(store.selected, item, 0)}><Icon name="next" /> Play all next</button>
    </div>
  </div>
  {#if songs.loading && !songs.data}
    <p class="notice" role="status">Loading songs…</p>
  {:else if songs.error}
    <div class="notice error" role="alert"><p>{songs.error}</p><button class="btn" onclick={() => songs.reload()}>Try again</button></div>
  {:else if songs.data}
    {#if list.length}
      <p class="muted small metadata"><span>Songs {page * PAGE + 1}–{page * PAGE + list.length}</span><span>{formatDuration(totalDuration(list))}</span></p>
      <SongList songs={list} {namespace} covers positions first={page * PAGE + 1} />
    {:else}
      <p class="notice muted">{page === 0 ? "No songs in this genre." : "No more songs in this genre."}</p>
    {/if}
    {#if page > 0 || songs.data.more}
      <div class="pagination bottom">
        <button class="btn" disabled={!current || page === 0 || songs.loading} onclick={() => changePage(page - 1)}>Previous</button>
        <span class="muted">Page {page + 1}</span>
        <button class="btn" disabled={!current || !songs.data.more || songs.loading} onclick={() => changePage(page + 1)}>Next</button>
      </div>
    {/if}
  {/if}
</section>

<style>
  .group { margin-bottom: 2rem; }
  .songs-heading { display: flex; align-items: center; justify-content: space-between; gap: 0.5rem 1rem; flex-wrap: wrap; margin: 1.5rem 0 0.5rem; }
  .songs-heading h2 { margin: 0; }
  .metadata { display: flex; flex-wrap: wrap; gap: 0 0.75rem; margin: 0 0 0.5rem; font-variant-numeric: tabular-nums; }
</style>
