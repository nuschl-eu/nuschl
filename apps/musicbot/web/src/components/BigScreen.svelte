<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount } from "svelte";
  import { store } from "../store.svelte.js";
  import { buildRoute, formatDuration } from "../logic.js";
  import { listening } from "../listening.svelte.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";
  import Progress from "./Progress.svelte";
  import SongRating from "./SongRating.svelte";
  import BrowserListen from "./BrowserListen.svelte";
  import FavoriteHeart from "./FavoriteHeart.svelte";
  import Lyrics from "./Lyrics.svelte";
  import { playerStatus } from "./PlayerBar.svelte";

  // `onclose(navigated)`: navigated is true when a link or history step moved to another page.
  let { onclose } = $props();
  let dialog = $state();
  const index = $derived(store.selected);
  const player = $derived(store.player);
  const players = $derived(store.snapshot?.players ?? []);
  const current = $derived(player?.current ?? null);
  const track = $derived(current?.entry.track ?? null);
  const volume = $derived(store.volume(index));
  const queue = $derived(store.queue(index));
  const upcoming = $derived(queue.slice(0, 3));
  const queueStatus = $derived(playerStatus(player));
  const status = $derived(queueStatus ?? listening.status);
  const statusError = $derived(queueStatus === null && !!listening.error);
  const canFullscreen = document.fullscreenEnabled === true && typeof document.documentElement.requestFullscreen === "function";
  let fullscreen = $state(!!document.fullscreenElement);
  let enteredFullscreen = false;

  onMount(() => {
    dialog.focus();
    return () => {
      if (enteredFullscreen && document.fullscreenElement) document.exitFullscreen().catch(() => {});
    };
  });

  async function toggleFullscreen() {
    try {
      if (document.fullscreenElement) {
        await document.exitFullscreen();
      } else {
        // The whole page, not just this overlay, so notifications stay visible.
        await document.documentElement.requestFullscreen();
        enteredFullscreen = true;
      }
    } catch (error) {
      store.toast(`Full screen is not available: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  function keydown(event) {
    if (event.key !== "Escape" || event.defaultPrevented) return;
    event.preventDefault();
    onclose(false);
  }

  // Close only after the browser has followed the link: a link removed from
  // the page during its own click is not followed. New-tab clicks keep the overlay.
  function followed(event) {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    setTimeout(() => onclose(true));
  }
</script>

<svelte:window onkeydown={keydown} onhashchange={() => onclose(true)} />
<svelte:document onfullscreenchange={() => (fullscreen = !!document.fullscreenElement)} />

<div class="big-screen" bind:this={dialog} role="dialog" aria-modal="true" aria-label="Big screen" tabindex="-1">
  <div class="top">
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
    <div class="top-actions">
      <button class="icon-btn" class:active={store.bigScreenLyrics} aria-label="Lyrics" aria-pressed={store.bigScreenLyrics} title={store.bigScreenLyrics ? "Show the next songs" : "Show the lyrics"} onclick={() => (store.bigScreenLyrics = !store.bigScreenLyrics)}>
        <Icon name="lyrics" size={28} />
      </button>
      {#if canFullscreen}
        <button class="icon-btn" aria-label="Full screen" aria-pressed={fullscreen} title={fullscreen ? "Leave full screen" : "Full screen"} onclick={toggleFullscreen}>
          <Icon name={fullscreen ? "fullscreen-exit" : "fullscreen"} size={28} />
        </button>
      {/if}
      <button class="icon-btn" aria-label="Close big screen" title="Close (Esc)" onclick={() => onclose(false)}>
        <Icon name="close" size={28} />
      </button>
    </div>
  </div>

  <div class="stage">
    <div class="art">
      {#if track}
        <Cover library={track.library} art={track.cover_art} size={400} />
      {:else}
        <Cover library={0} art={null} />
      {/if}
    </div>

    <div class="info">
      <div class="song">
        {#if track}
          <h2 class="title"><a href={buildRoute({ view: "song", library: track.library, id: track.id, namespace: store.browseKey })} onclick={followed}>{track.title}</a></h2>
          {#if track.artist}
            <div class="sub">
              {#if track.artist_id}
                <a href={buildRoute({ view: "artist", library: track.library, id: track.artist_id, namespace: store.browseKey })} onclick={followed}>{track.artist}</a>
              {:else}{track.artist}{/if}
            </div>
          {/if}
          {#if track.album}
            <div class="sub">
              {#if track.album_id}
                <a href={buildRoute({ view: "album", library: track.library, id: track.album_id, namespace: store.browseKey })} onclick={followed}>{track.album}</a>
              {:else}{track.album}{/if}
            </div>
          {/if}
          <div class="rating">
            <SongRating library={track.library} songId={track.id} title={track.title} namespace={store.browseKey} large />
            <FavoriteHeart library={track.library} songId={track.id} title={track.title} namespace={store.browseKey} size={32} />
          </div>
        {:else}
          <h2 class="title muted">Nothing is playing</h2>
        {/if}
      </div>

      <Progress player={index} {current} large />

      <div class="controls">
        <div class="controls-start"><BrowserListen player={index} large /></div>
        <button
          class="play"
          disabled={!player}
          aria-label={player?.paused ? "Play" : "Pause"}
          title={player?.paused ? "Play (Space)" : "Pause (Space)"}
          onclick={() => store.togglePause(index)}
        >
          <Icon name={player?.paused ? "play" : "pause"} size={40} />
        </button>
        <div class="controls-end">
          <button class="icon-btn" disabled={!current} aria-label="Skip" title="Skip" onclick={() => store.skip(index)}>
            <Icon name="skip" size={32} />
          </button>
        </div>
      </div>

      {#if player}
        <label class="volume" title="Mumble volume {volume}%; does not change browser volume">
          <Icon name="volume" size={24} />
          <span>Mumble <span class="visually-hidden">volume</span></span>
          <input type="range" min="0" max="100" value={volume} oninput={(event) => store.setVolume(index, Number(event.currentTarget.value))} />
        </label>
      {/if}

      <p class="status" class:error={statusError} role="status" title={status ?? ""}>{status ?? ""}</p>

      {#if store.bigScreenLyrics && track}
        <section class="lyrics-panel card" aria-label="Lyrics">
          <h3 class="eyebrow">Lyrics</h3>
          {#key `${track.library}:${track.id}`}
            <Lyrics library={track.library} songId={track.id} title={track.title} namespace={store.browseKey} large />
          {/key}
        </section>
      {:else if upcoming.length}
        <section class="up-next card" aria-label="Up next">
          <h3 class="eyebrow">Up next</h3>
          <ol>
            {#each upcoming as entry (entry.id)}
              <li>
                <Cover library={entry.track.library} art={entry.track.cover_art} size={96} px={48} />
                <div class="entry">
                  <div class="entry-title" title={entry.track.title}>{entry.track.title}</div>
                  <div class="entry-artist" title={entry.track.artist ?? ""}>{entry.track.artist ?? "Unknown artist"}</div>
                </div>
                <span class="duration">{formatDuration(entry.track.duration)}</span>
              </li>
            {/each}
          </ol>
          {#if queue.length > upcoming.length}<p class="more">and {queue.length - upcoming.length} more</p>{/if}
        </section>
      {/if}
    </div>
  </div>
</div>

<style>
  .big-screen {
    position: fixed;
    inset: 0;
    z-index: 40;
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    padding: clamp(0.75rem, 2.5vw, 2rem);
    background: var(--bg);
    outline: none;
  }
  .top { display: flex; flex: none; align-items: center; gap: 0.75rem; }
  .top-actions { display: flex; gap: 0.25rem; margin-left: auto; }
  .queue-select select { max-width: min(16rem, 60vw); }

  /* Landscape (TV, desktop): cover left, everything else right. */
  .stage {
    flex: 1;
    display: grid;
    grid-template-columns: auto minmax(0, 38rem);
    justify-content: center;
    align-items: center;
    gap: clamp(1.5rem, 5vw, 5rem);
    padding: 1rem 0;
  }
  .art { width: min(68dvh, 42vw); }
  .art :global(.cover) { border-radius: calc(var(--radius) + 4px); box-shadow: var(--shadow); }
  .info { display: grid; gap: 1.1rem; min-width: 0; }
  .song { display: grid; gap: 0.35rem; min-width: 0; }
  .title { margin: 0; font-size: clamp(1.6rem, 3.5vw, 3.25rem); line-height: 1.15; overflow-wrap: anywhere; }
  .title a:not(:hover), .sub a:not(:hover) { color: inherit; }
  .sub { font-size: clamp(1rem, 1.8vw, 1.5rem); color: var(--muted); overflow-wrap: anywhere; }
  .rating { display: flex; align-items: center; gap: 0.5rem; margin-top: 0.4rem; }
  .rating :global(.heart) { width: 3rem; height: 3rem; }

  .controls { display: grid; grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr); align-items: center; gap: 1rem; }
  .controls-start { display: flex; justify-content: flex-end; min-width: 0; }
  .controls-end { display: flex; align-items: center; min-width: 0; }
  .play {
    width: 72px;
    height: 72px;
    border-radius: 50%;
    border: none;
    display: grid;
    place-items: center;
    background: var(--accent);
    color: var(--accent-fg);
    box-shadow: var(--shadow);
    flex: none;
  }
  .play:hover:not(:disabled) { filter: brightness(1.08); }
  .play:disabled { opacity: 0.4; }
  .top .icon-btn, .controls-end .icon-btn { width: 3rem; height: 3rem; }

  .volume { display: flex; align-items: center; justify-content: center; gap: 0.6rem; color: var(--muted); }
  .volume input { flex: 1; max-width: 16rem; min-width: 0; }
  .status { min-height: 1.5rem; margin: 0; overflow-wrap: anywhere; text-align: center; color: var(--muted); }
  .status.error { color: var(--error); }

  .up-next, .lyrics-panel { min-width: 0; padding: 0.75rem; }
  h3 { margin: 0 0 0.5rem 0.25rem; }
  ol { list-style: none; margin: 0; padding: 0; display: grid; gap: 0.25rem; }
  li { display: flex; align-items: center; gap: 0.75rem; min-width: 0; padding: 0.3rem 0.25rem; }
  .entry { flex: 1; min-width: 0; }
  .entry-title, .entry-artist { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .entry-title { font-weight: 500; }
  .entry-artist, .duration, .more { color: var(--muted); font-size: 0.85rem; }
  .duration { flex: none; font-variant-numeric: tabular-nums; }
  .more { margin: 0.5rem 0 0 0.25rem; }

  /* Portrait (phone, rotated screen) or narrow windows: stacked and centred. */
  @media (orientation: portrait), (max-width: 760px) {
    .stage { grid-template-columns: minmax(0, 1fr); justify-items: center; align-items: start; align-content: start; gap: 1.25rem; }
    .art { width: min(80vw, 42dvh); }
    .info { width: 100%; max-width: 36rem; }
    .song { justify-items: center; text-align: center; }
  }
</style>
