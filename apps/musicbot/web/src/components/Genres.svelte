<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { buildRoute, filterGenres } from "../logic.js";

  /** The Genres page below its heading: `genres` and `truncated` from `api/genres` for `library`. */
  let { genres, truncated, library } = $props();
  let text = $state("");
  let order = $state("name");
  const shown = $derived(filterGenres(genres, text, order));
  const amount = (value, one, many) => (Number.isInteger(value) ? `${value.toLocaleString()} ${value === 1 ? one : many}` : null);
  const counts = (genre) => [amount(genre.albums, "album", "albums"), amount(genre.songs, "song", "songs")].filter(Boolean).join(" · ");
</script>

{#if genres.length}
  <div class="tools">
    <label class="filter"><span class="visually-hidden">Filter genres</span>
      <input type="search" placeholder="Filter genres" autocomplete="off" spellcheck="false" bind:value={text} />
    </label>
    <label><span class="visually-hidden">Sort genres</span>
      <select bind:value={order}>
        <option value="name">By name</option>
        <option value="size">By size</option>
      </select>
    </label>
    <span class="muted small" role="status">{shown.length === genres.length ? `${genres.length.toLocaleString()} genres` : `${shown.length.toLocaleString()} of ${genres.length.toLocaleString()} genres`}</span>
  </div>
  {#if truncated}<p class="muted small">The list shows the first {genres.length.toLocaleString()} genres by name.</p>{/if}
  <ul class="genre-list">
    {#each shown as genre (genre.name)}
      <li>
        <a class="genre card" href={buildRoute({ view: "genre", library, name: genre.name })}>
          <span class="genre-name" title={genre.name}>{genre.name}</span>
          {#if counts(genre)}<span class="muted small">{counts(genre)}</span>{/if}
        </a>
      </li>
    {/each}
  </ul>
  {#if !shown.length}<p class="notice muted">No genres match “{text.trim()}”.</p>{/if}
{:else}
  <p class="notice muted">No genres in this library.</p>
{/if}

<style>
  .tools { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem 0.75rem; margin: 0 0 1rem; }
  .filter { flex: 1 1 14rem; max-width: 24rem; }
  .filter input { width: 100%; }
  .genre-list { display: grid; grid-template-columns: repeat(auto-fill, minmax(13rem, 1fr)); gap: 0.6rem; margin: 0; padding: 0; list-style: none; }
  .genre { display: grid; gap: 0.15rem; height: 100%; padding: 0.65rem 0.85rem; box-shadow: none; }
  .genre:hover, .genre:focus-visible { border-color: var(--accent); }
  .genre-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 600; }
  .genre .small { font-variant-numeric: tabular-nums; }
  @media (max-width: 480px) { .genre-list { grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 0.45rem; } }
</style>
