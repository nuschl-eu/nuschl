<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script module>
  /** What the selected queue is doing, when that is worth a line; shared with the big screen. */
  export function playerStatus(player) {
    const current = player?.current ?? null;
    if (!player) return null;
    if (player.error) return `Playback error: ${player.error}`;
    if (player.dj) return player.paused ? "DJ mode, paused" : "DJ mode";
    if (!current) return null;
    if (player.paused) return "Paused";
    if (player.listeners === 0) return "Waiting for listeners";
    if (current.buffering) return "Buffering…";
    if (player.ducked) return "Mumble quieter while someone talks";
    return null;
  }
</script>

<script>
  import { store } from "../store.svelte.js";
  import { buildRoute, leadDeck } from "../logic.js";
  import { carriesItem, droppedItem } from "../dnd.js";
  import { listening } from "../listening.svelte.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";
  import Progress from "./Progress.svelte";
  import DuckingPanel from "./DuckingPanel.svelte";
  import SongRating from "./SongRating.svelte";
  import BrowserListen from "./BrowserListen.svelte";
  import FavoriteHeart from "./FavoriteHeart.svelte";
  import AddToPlaylist from "./AddToPlaylist.svelte";

  let { onexpand } = $props();
  const index = $derived(store.selected);
  const player = $derived(store.player);
  const players = $derived(store.snapshot?.players ?? []);
  const current = $derived(player?.current ?? null);
  const djState = $derived(player?.dj ?? null);
  const lead = $derived(leadDeck(djState));
  const track = $derived(current?.entry.track ?? (lead === null ? null : djState.decks[lead].entry.track));
  const volume = $derived(store.volume(index));
  const queueStatus = $derived(playerStatus(player));
  // Browser listening only gets the line when the queue has nothing to report.
  const status = $derived(queueStatus ?? listening.status);
  const statusError = $derived(queueStatus === null && !!listening.error);
  let dropping = $state(false);

  function dragover(event) {
    if (!carriesItem(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
    dropping = true;
  }

  function drop(event) {
    dropping = false;
    const item = droppedItem(event);
    if (!item) return;
    event.preventDefault();
    store.add(index, item, null);
  }
</script>

<footer
  class="player card"
  class:dropping
  aria-label="Player"
  ondragover={dragover}
  ondragleave={(event) => {
    if (!event.currentTarget.contains(event.relatedTarget)) dropping = false;
  }}
  ondrop={drop}
>
  <div class="song">
    {#if track}
      <Cover library={track.library} art={track.cover_art} size={128} px={56} />
      <div class="text">
        <div class="title" title={track.title}><a href={buildRoute({ view: "song", library: track.library, id: track.id, namespace: store.browseKey })}>{track.title}</a></div>
        {#if track.artist}
          <div class="sub">
            {#if track.artist_id}
              <a href={buildRoute({ view: "artist", library: track.library, id: track.artist_id, namespace: store.browseKey })}>{track.artist}</a>
            {:else}{track.artist}{/if}
          </div>
        {/if}
        {#if track.album}
          <div class="sub">
            {#if track.album_id}
              <a href={buildRoute({ view: "album", library: track.library, id: track.album_id, namespace: store.browseKey })}>{track.album}</a>
            {:else}{track.album}{/if}
          </div>
        {/if}
      </div>
      <div class="song-actions">
        <FavoriteHeart library={track.library} songId={track.id} title={track.title} namespace={store.browseKey} />
        <AddToPlaylist library={track.library} songs={[track.id]} title={track.title} namespace={store.browseKey} up />
      </div>
    {:else}
      <Cover library={0} art={null} px={56} />
      <div class="text"><div class="sub">{dropping ? "Drop to add to the queue" : "Nothing is playing"}</div></div>
    {/if}
  </div>

  <div class="center">
    <div class="controls">
      <div class="controls-start"><BrowserListen player={index} /></div>
      <button
        class="play"
        disabled={!player}
        aria-label={player?.paused ? "Play" : "Pause"}
        title={player?.paused ? "Play (Space)" : "Pause (Space)"}
        onclick={() => store.togglePause(index)}
      >
        <Icon name={player?.paused ? "play" : "pause"} size={22} />
      </button>
      <div class="controls-end">
        {#if djState}
          <a class="icon-btn" href={buildRoute({ view: "dj" })} aria-label="Open the decks" title="DJ decks"><Icon name="decks" /></a>
        {:else}
          <button class="icon-btn" disabled={!current} aria-label="Skip" title="Skip" onclick={() => store.skip(index)}>
            <Icon name="skip" />
          </button>
        {/if}
        <button class="icon-btn" aria-label="Open big screen" aria-haspopup="dialog" title="Big screen" onclick={(event) => { store.bigScreenLyrics = false; onexpand(event); }}>
          <Icon name="expand" />
        </button>
        <button class="icon-btn" disabled={!track} aria-label="Lyrics" aria-haspopup="dialog" title="Lyrics on the big screen" onclick={(event) => { store.bigScreenLyrics = true; onexpand(event); }}>
          <Icon name="lyrics" />
        </button>
      </div>
    </div>
    {#if !djState}<Progress player={index} {current} />{/if}
    <span class="status" class:error={statusError} role="status" title={status ?? ""}>{status ?? ""}</span>
  </div>

  <div class="side">
    {#if players.length > 1}
      <label class="queue-select">
        <span class="visually-hidden">Queue</span>
        <select value={index} onchange={(event) => store.selectQueue(Number(event.currentTarget.value))}>
          {#each players as p, i (i)}
            <option value={i}>{p.name || `Queue ${i + 1}`}</option>
          {/each}
        </select>
      </label>
    {/if}
    {#if player}
      <label class="volume" title="Mumble volume {volume}%; does not change browser volume">
        <Icon name="volume" />
        <span>Mumble <span class="visually-hidden">volume</span></span>
        <input type="range" min="0" max="100" value={volume} oninput={(event) => store.setVolume(index, Number(event.currentTarget.value))} />
      </label>
      <DuckingPanel player={index} />
    {/if}
    {#if track}<SongRating library={track.library} songId={track.id} title={track.title} namespace={store.browseKey} />{/if}
  </div>
</footer>

<style>
  .player {
    flex: none;
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 2fr) minmax(0, 1fr);
    align-items: center;
    gap: 0.9rem;
    width: min(1568px, calc(100% - 2rem));
    margin: 0 auto 0.75rem;
    padding: 0.5rem 0.75rem;
  }
  .player.dropping { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft), var(--shadow); }
  .song { display: flex; align-items: center; gap: 0.75rem; min-width: 0; }
  .text { min-width: 0; line-height: 1.3; }
  .song-actions { display: flex; flex: none; align-items: center; margin-left: auto; }
  .title { font-weight: 600; }
  .title, .sub { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .sub { font-size: 0.85rem; color: var(--muted); }
  .sub a:not(:hover) { color: inherit; }
  .center { display: grid; justify-items: center; gap: 0.1rem; min-width: 0; }
  /* Play stays centred; the side groups use the otherwise empty width of the row. */
  .controls { position: relative; display: grid; grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr); align-items: center; gap: 0.5rem; width: 100%; max-width: 32rem; }
  .controls-start { display: flex; justify-content: flex-end; min-width: 0; }
  .controls-end { display: flex; align-items: center; gap: 0.25rem; min-width: 0; }
  .play {
    width: 2.6rem;
    height: 2.6rem;
    flex: none;
    display: grid;
    place-items: center;
    border: none;
    border-radius: 50%;
    background: var(--accent);
    color: var(--accent-fg);
  }
  .play:hover:not(:disabled) { filter: brightness(1.08); }
  .play:disabled { opacity: 0.4; }
  .status { width: 100%; height: 1.2rem; line-height: 1.2rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; text-align: center; font-size: 0.8rem; color: var(--muted); }
  .status.error { color: var(--error); }
  .side { display: flex; align-items: center; justify-content: flex-end; flex-wrap: wrap; gap: 0.25rem 0.5rem; min-width: 0; }
  .queue-select select { max-width: 10rem; padding: 0.3rem 0.7rem; border-radius: 999px; font-size: 0.85rem; }
  .volume { display: flex; align-items: center; gap: 0.35rem; color: var(--muted); font-size: 0.85rem; }
  .volume input { width: 6rem; }

  @media (max-width: 760px) {
    .player {
      grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
      grid-template-areas: "song side" "center center";
      gap: 0.3rem 0.75rem;
      width: calc(100% - 1rem);
      margin-bottom: 0.5rem;
    }
    .song { grid-area: song; }
    .center { grid-area: center; }
    .side { grid-area: side; }
    .volume input { width: 5rem; }
    .queue-select select { max-width: 8rem; }
  }
  @media (max-width: 480px) {
    .player { grid-template-columns: minmax(0, 1fr); grid-template-areas: "song" "center" "side"; }
    .side { justify-content: flex-start; }
  }
</style>
