<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { untrack } from "svelte";
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { query } from "../api.js";
  import { buildRoute, releaseTypeLabel } from "../logic.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";

  let { route } = $props();
  const namespace = store.browseKey;
  const current = $derived(namespace !== null && namespace === store.browseKey && store.connection === "open" && !store.invalid);
  const available = $derived(store.snapshot?.import != null);
  let search = $state(untrack(() => route.q));
  const result = resource(() => (current && available && route.q.trim() ? query("api/import/search", { q: route.q }) : null), () => store.browseSignal);
  const albums = $derived(current && Array.isArray(result.data?.albums) ? result.data.albums : []);

  function submit(event) {
    event.preventDefault();
    store.navigate({ view: "add", q: search.trim() });
  }
</script>

<section aria-labelledby="add-title" aria-busy={result.loading}>
  <div class="section-heading">
    <h1 id="add-title">Add music</h1>
    <a class="heading-link" href={buildRoute({ view: "requests" })}><Icon name="queue" size={18} /> Requests</a>
  </div>
  {#if !available}
    <p class="notice muted">Adding music is not available on this bot.</p>
  {:else}
    <form class="add-search" role="search" onsubmit={submit}>
      <label for="add-search-input" class="visually-hidden">Search for albums to add</label>
      <input id="add-search-input" type="search" placeholder="Artist or album" maxlength="200" bind:value={search} autocomplete="off" />
      <button class="btn primary" type="submit" disabled={!current}><Icon name="search" size={18} /> Search</button>
    </form>
    {#if !current}
      <p class="notice muted" role="status">Searching will resume after the connection is restored.</p>
    {:else if !route.q.trim()}
      <p class="notice muted">Search by artist, album title or both.</p>
    {:else if result.loading}
      <p class="notice" role="status">Searching…</p>
    {:else if result.error}
      <div class="notice error" role="alert"><p>{result.error}</p><button class="btn" onclick={() => result.reload()}>Try again</button></div>
    {:else if result.data}
      <p class="muted">Results for “{route.q}”</p>
      {#if albums.length}
        <div class="media-grid">
          {#each albums as album (album.id)}
            {@const href = buildRoute({ view: "addAlbum", id: album.id })}
            <article class="tile card">
              <a class="art-link" {href} aria-label="Open {album.title}"><Cover importId={album.has_cover ? album.id : null} /></a>
              <a class="tile-title" {href} title={album.title}>{album.title}</a>
              <div class="muted tile-sub" title={album.artist}>{album.artist}</div>
              <div class="muted tile-meta">
                {#if album.year}<span>{album.year}</span>{/if}
                {#if album.track_count != null}<span>{album.track_count} {album.track_count === 1 ? "track" : "tracks"}</span>{/if}
                {#if releaseTypeLabel(album.release_type)}<span>{releaseTypeLabel(album.release_type)}</span>{/if}
              </div>
            </article>
          {/each}
        </div>
      {:else}
        <p class="notice muted">No albums found.</p>
      {/if}
    {/if}
  {/if}
</section>

<style>
  .add-search { display: flex; gap: 0.5rem; margin: 0 0 1.25rem; max-width: 36rem; }
  .add-search input { flex: 1; }
</style>
