<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { buildRoute, formatDuration, similarityPercent, sonicPathStart } from "../logic.js";
  import { startItemDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";
  import SongRating from "./SongRating.svelte";
  import AddToPlaylist from "./AddToPlaylist.svelte";
  import DeckQueueAdd from "./DeckQueueAdd.svelte";

  /**
   * `similarities` is optional and parallel to `songs`: OpenSubsonic scores, null when unknown.
   * `onremove(index, song)` adds a remove button to every row, for playlists the viewer edits.
   * `positions` numbers the rows by their place in the list (playlists, pages of songs) from `first`, instead of track numbers.
   */
  let { songs, namespace, covers = false, similarities = null, onremove = null, positions = false, first = 1 } = $props();
  const current = $derived(namespace !== null && namespace === store.browseKey && store.connection === "open" && !store.invalid);
  const playing = $derived(store.player?.current?.entry.track ?? null);
  const sonic = (song) => current && store.supportsSonic(song.library) && store.browseItemCurrent({ library: song.library, namespace });
</script>

<ol class="rows card" aria-label="Tracks">
  {#each songs as song, i (`${song.library}:${song.id}:${i}`)}
    <li draggable={current} ondragstart={(event) => startItemDrag(event, "song", song.library, song.id, song.title, namespace)}>
      <span class="row-number">{positions ? first + i : song.track ?? i + 1}</span>
      {#if covers}<Cover library={song.library} art={song.cover_art} size={96} px={44} />{/if}
      <div class="row-text">
        <div class="row-title" title={song.title}><a href={buildRoute({ view: "song", library: song.library, id: song.id, namespace })} draggable="false">{song.title}</a></div>
        <div class="row-sub">
          {#if song.artist_id}<a href={buildRoute({ view: "artist", library: song.library, id: song.artist_id, namespace })}>{song.artist || "Unknown artist"}</a>
          {:else if song.artist}<span>{song.artist}</span>{/if}
          {#if covers && song.album}
            {#if song.album_id}<a href={buildRoute({ view: "album", library: song.library, id: song.album_id, namespace })}>{song.album}</a>
            {:else}<span>{song.album}</span>{/if}
          {/if}
        </div>
      </div>
      {#if similarities}
        {@const percent = similarityPercent(similarities[i])}
        {#if percent !== null}<span class="similarity muted" title="Sonic similarity">{percent}%<span class="visually-hidden"> similar</span></span>{/if}
      {/if}
      <span class="row-duration">{formatDuration(song.duration)}</span>
      <div class="actions">
        {#if sonic(song)}
          {@const from = sonicPathStart(playing, song)}
          <a class="icon-btn" draggable="false" href={buildRoute({ view: "similar", library: song.library, id: song.id, namespace })} aria-label="Songs similar to {song.title}" title="Similar songs"><Icon name="similar" /></a>
          {#if from !== null}
            <a class="icon-btn" draggable="false" href={buildRoute({ view: "path", library: song.library, from, to: song.id, namespace })} aria-label="Blend from now playing into {song.title}" title="Blend into this song"><Icon name="path" /></a>
          {/if}
        {/if}
        <button class="icon-btn" disabled={!store.player || !current} aria-label="Add {song.title} to queue" title="Add to queue" onclick={() => store.add(store.selected, { kind: "song", library: song.library, id: song.id, namespace })}><Icon name="add" /></button>
        <button class="icon-btn" disabled={!store.player || !current} aria-label="Play {song.title} next" title="Play next" onclick={() => store.add(store.selected, { kind: "song", library: song.library, id: song.id, namespace }, 0)}><Icon name="next" /></button>
        <DeckQueueAdd item={{ kind: "song", library: song.library, id: song.id, namespace }} title={song.title} disabled={!current} />
        <AddToPlaylist library={song.library} songs={[song.id]} title={song.title} {namespace} />
        {#if onremove}<button class="icon-btn" disabled={!current} aria-label="Remove {song.title} from this playlist" title="Remove from playlist" onclick={() => onremove(i, song)}><Icon name="remove" /></button>{/if}
        <SongRating library={song.library} songId={song.id} title={song.title} {namespace} />
      </div>
    </li>
  {/each}
</ol>

<style>
  li { cursor: grab; }
  .similarity { flex: none; color: var(--muted); font-size: 0.8rem; font-variant-numeric: tabular-nums; }
  .actions { display: flex; align-items: center; }
  @media (max-width: 480px) { .row-number, .row-duration { display: none; } .actions { flex-wrap: wrap; justify-content: flex-end; max-width: 8.5rem; } }
</style>
