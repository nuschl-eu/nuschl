<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { query } from "../api.js";
  import { formatDuration, interpolatePosition, lyricLineAt } from "../logic.js";

  /**
   * A song's lyrics from the library. While the selected queue plays this
   * song, synced lyrics follow the music and a line click seeks to it.
   */
  let { library, songId, title, namespace, large = false } = $props();
  let list = $state();
  let now = $state(performance.now());
  // The listener scrolled by hand: following waits until then.
  let followAfter = 0;
  const current = $derived(typeof songId === "string" && songId.length > 0 && store.browseItemCurrent({ library, namespace }));
  const result = resource(() => (current ? query("api/lyrics", { library, id: songId }) : null), () => store.browseSignal);
  const lyrics = $derived(current ? result.data?.lyrics ?? null : null);
  const playing = $derived.by(() => {
    const shown = store.player?.current ?? null;
    const track = shown?.entry.track;
    return track && track.library === library && track.id === songId ? shown : null;
  });
  const live = $derived(Boolean(lyrics?.synced && playing));
  const hold = $derived(store.holds[store.selected]);

  $effect(() => {
    if (!live || !playing.advancing) return;
    let frame = requestAnimationFrame(function tick() {
      now = performance.now();
      frame = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(frame);
  });

  const position = $derived.by(() => {
    if (!live) return NaN;
    if (hold && hold.entry === playing.entry.id) return store.holdPosition(hold, playing, now);
    return interpolatePosition(playing.position_ms, playing.advancing, store.receivedAt, now, playing.entry.track.duration);
  });
  const active = $derived(live ? lyricLineAt(lyrics.lines, position) : -1);

  $effect(() => {
    const index = active;
    if (index < 0 || !list || performance.now() < followAfter) return;
    const line = list.children[index];
    if (!line) return;
    const smooth = !window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    list.scrollTo({ top: line.offsetTop - list.clientHeight / 2 + line.offsetHeight / 2, behavior: smooth ? "smooth" : "auto" });
  });

  function scrolledByHand() {
    followAfter = performance.now() + 5000;
  }

  function seek(line) {
    if (live && Number.isFinite(line.start_ms)) store.seek(store.selected, line.start_ms, true);
  }
</script>

<div class="lyrics" class:large aria-busy={result.loading}>
  {#if !current}
    <p class="muted">Lyrics load once the library is connected.</p>
  {:else if result.loading && !result.data}
    <p class="muted" role="status">Loading lyrics…</p>
  {:else if result.error}
    <div class="notice error" role="alert"><p>{result.error}</p><button class="btn" onclick={() => result.reload()}>Try again</button></div>
  {:else if !lyrics}
    <p class="muted">The library has no lyrics for this song.</p>
  {:else}
    <ol
      class="lines"
      class:synced={lyrics.synced}
      lang={lyrics.lang ?? undefined}
      aria-label="Lyrics of {title}"
      bind:this={list}
      onwheel={scrolledByHand}
      ontouchmove={scrolledByHand}
    >
      {#each lyrics.lines as line, i (i)}
        <li class:active={i === active} class:past={live && i < active} aria-current={i === active ? "true" : undefined}>
          {#if live && line.text}
            <button class="line" title="Play from {formatDuration(line.start_ms / 1000)}" onclick={() => seek(line)}>{line.text}</button>
          {:else}
            <span class="line">{line.text || " "}</span>
          {/if}
        </li>
      {/each}
    </ol>
    {#if lyrics.synced && !playing}<p class="muted small hint">These lyrics follow the music while this song plays.</p>{/if}
  {/if}
</div>

<style>
  .lyrics { min-width: 0; }
  .lines { position: relative; list-style: none; margin: 0; padding: 0.25rem 0; max-height: 26rem; overflow-y: auto; overscroll-behavior: contain; }
  .lines li { min-height: 1.6em; }
  .line { display: block; width: 100%; padding: 0.15rem 0.5rem; border: none; border-radius: var(--radius); background: none; color: inherit; font: inherit; text-align: inherit; white-space: pre-wrap; overflow-wrap: anywhere; }
  button.line:hover { background: var(--surface-2); }
  .synced li { color: var(--muted); transition: color 0.2s; }
  .synced li.past { color: color-mix(in srgb, var(--muted) 70%, transparent); }
  .synced li.active { color: var(--fg); font-weight: 600; }
  .hint { margin: 0.5rem 0 0; }
  .large .lines { max-height: min(32dvh, 24rem); font-size: clamp(1.05rem, 1.6vw, 1.4rem); line-height: 1.5; }
  @media (prefers-reduced-motion: reduce) { .synced li { transition: none; } }
</style>
