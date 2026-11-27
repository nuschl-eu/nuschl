<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { dj } from "../dj.svelte.js";
  import { cueListening } from "../listening.svelte.js";
  import { DJ_BEAT_LOOPS, DJ_EQ_MAX, DJ_EQ_MIN, DJ_TEMPO_RANGES, beatsText, buildRoute, clamp, deckBpm, deckPosition, decibelText, filterText, formatDuration, gridTicks, pointerFraction, tempoText } from "../logic.js";
  import { carriesEntry, carriesItem, droppedEntry, droppedItem, leftElement } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";
  import DjSlider from "./DjSlider.svelte";
  import DeckWave from "./DeckWave.svelte";

  /** Beat-grid ticks drawn on the position bar at most; longer songs get bar lines only. */
  const MAX_TICKS = 200;
  /** Shortest loop the handles make, in milliseconds. */
  const MIN_LOOP_MS = 10;
  const beatName = (beats) => (beats === 0.25 ? "¼" : beats === 0.5 ? "½" : String(beats));
  const preciseTime = (ms) => `${formatDuration(ms / 1000) || "0:00"}.${Math.floor((ms % 1000) / 100)}`;

  /** `data` is the deck from the state; `learning` names the control MIDI learn waits for. */
  let { player, deck, data, learning = null } = $props();
  const letter = $derived(deck === 0 ? "A" : "B");
  const id = $props.id();
  const track = $derived(data.entry?.track ?? null);
  const key = (name) => `${player}:${deck}:${name}`;
  const volume = $derived(dj.value(key("volume"), data.volume));
  const tempo = $derived(dj.value(key("tempo"), data.tempo));
  const filter = $derived(dj.value(key("filter"), data.filter));
  const bands = $derived([
    { band: "high", label: "High", value: dj.value(key("eq:high"), data.eq.high) },
    { band: "mid", label: "Mid", value: dj.value(key("eq:mid"), data.eq.mid) },
    { band: "low", label: "Low", value: dj.value(key("eq:low"), data.eq.low) },
  ]);
  const bpm = $derived(deckBpm(data.bpm, tempo));
  const estimate = $derived(data.bpm_source === "estimate");
  const bpmTitle = $derived(data.bpm
    ? estimate ? `About ${data.bpm} BPM, estimated from the audio` : `${data.bpm} BPM from the library`
    : data.analysing ? "Analysing the song's tempo" : "BPM unknown: sync needs the BPM of both songs");
  const duration = $derived(track?.duration ?? null);
  const max = $derived(typeof duration === "number" && duration > 0 ? duration * 1000 : 0);
  const cueing = $derived(cueListening.deck === deck && cueListening.player === player);
  /** Share of the song decoded in memory in percent, while the deck streams the rest. */
  const loadedPercent = $derived(track && max && !data.preloaded && data.loaded_ms != null ? clamp(data.loaded_ms / max, 0, 1) * 100 : null);
  const percent = (ms) => (max ? clamp(ms / max, 0, 1) * 100 : 0);

  const beatMs = $derived(data.grid?.bpm > 0 ? 60000 / data.grid.bpm : null);
  const ticks = $derived(max ? gridTicks(data.grid, 0, max, MAX_TICKS) : []);
  /** True when the ticks show bars only, so none stands out. */
  const thinned = $derived(beatMs !== null && ticks.length > 1 && ticks[1].ms - ticks[0].ms > beatMs * 1.5);
  /** The loop ends shown: a handle's own values while it moves, otherwise the bot's. */
  const loopEnds = $derived(data.loop ? dj.value(key("loop"), { start_ms: data.loop.start_ms, end_ms: data.loop.end_ms }) : null);
  const loopClosed = $derived(loopEnds !== null && loopEnds.end_ms != null);

  let now = $state(performance.now());
  let seeking = $state(null);
  $effect(() => {
    if (!data.advancing) return;
    let frame = requestAnimationFrame(function tick() {
      now = performance.now();
      frame = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(frame);
  });
  const position = $derived(seeking ?? deckPosition(data.position_ms, data.advancing, tempo, store.receivedAt, now, duration));
  const learnClass = (control) => learning === control;

  function seek(ms) {
    seeking = null;
    if (track) dj.seek(player, deck, ms);
  }

  let card = $state();
  let dropping = $state(false);

  function dragover(event) {
    if (!carriesItem(event) && !carriesEntry(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = carriesItem(event) ? "copy" : "move";
    dropping = true;
  }

  // A song or an entry loads onto the deck; an album or playlist loads its
  // first song and puts the rest at the front of the deck's queue.
  function drop(event) {
    dropping = false;
    if (carriesItem(event)) {
      const item = droppedItem(event);
      if (!item) return;
      event.preventDefault();
      dj.load(player, deck, item);
    } else if (carriesEntry(event)) {
      const entry = droppedEntry(event);
      event.preventDefault();
      if (entry !== null) dj.load(player, deck, { entry });
    }
  }

  let handles = $state();
  let dragging = $state(null);

  /** Moves one loop end (`end` true: the end, false: the start) to `ms`, keeping the loop at least MIN_LOOP_MS long. */
  function moveLoopEnd(end, ms) {
    const { start_ms, end_ms } = loopEnds;
    if (end) dj.loopSet(player, deck, start_ms, clamp(ms, start_ms + MIN_LOOP_MS, max));
    else dj.loopSet(player, deck, clamp(ms, 0, end_ms - MIN_LOOP_MS), end_ms);
  }

  // Arrow keys move a handle by one beat (Shift: four), or by 0.1 s (Shift: 1 s) without a beat grid.
  function nudgeLoop(event, end) {
    const steps = { ArrowLeft: -1, ArrowDown: -1, ArrowRight: 1, ArrowUp: 1 };
    if (!(event.key in steps) || !loopClosed) return;
    event.preventDefault();
    const stepMs = beatMs === null ? (event.shiftKey ? 1000 : 100) : beatMs * (event.shiftKey ? 4 : 1);
    moveLoopEnd(end, (end ? loopEnds.end_ms : loopEnds.start_ms) + steps[event.key] * stepMs);
  }

  function grab(event, end) {
    if (!loopClosed) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    event.currentTarget.focus();
    dragging = end;
  }

  function pull(event, end) {
    if (dragging !== end || !loopClosed) return;
    const rect = handles.getBoundingClientRect();
    moveLoopEnd(end, pointerFraction(event.clientX, rect.left, rect.width) * max);
  }
</script>

<article class="deck card" class:dropping bind:this={card} aria-labelledby="{id}-title" ondragover={dragover} ondrop={drop} ondragleave={(event) => { if (leftElement(event, card)) dropping = false; }}>
  {#if dropping}<p class="drop-label" aria-hidden="true">Drop to load onto deck {letter}</p>{/if}
  <header class="deck-head">
    <span class="deck-letter" aria-hidden="true">{letter}</span>
    <Cover library={track?.library ?? 0} art={track?.cover_art ?? null} size={128} px={56} />
    <div class="deck-text">
      <h2 id="{id}-title" class="visually-hidden">Deck {letter}</h2>
      {#if track}
        <div class="deck-title" title={track.title}><a href={buildRoute({ view: "song", library: track.library, id: track.id, namespace: store.browseKey })}>{track.title}</a></div>
        <div class="deck-sub muted">{track.artist ?? "Unknown artist"}</div>
        <div class="deck-sub muted small">Added by {data.entry.added_by}</div>
      {:else}
        <div class="deck-sub muted">Deck {letter} is empty. Drop a song here, or load the next one from its queue.</div>
      {/if}
    </div>
    <div class="bpm" title={bpmTitle}>
      <span class="bpm-value num">{#if estimate}<span class="approx" aria-hidden="true">≈</span>{/if}{bpm === null ? "—" : bpm.toFixed(1)}</span>
      <span class="eyebrow">{estimate ? "BPM est." : "BPM"}</span>
      {#if data.analysing}<span class="analysing small muted">Analysing…</span>{/if}
    </div>
  </header>

  {#if data.error}<p class="deck-error small" role="alert">{data.error}</p>{/if}

  <DeckWave {player} {deck} {data} {duration} {loopEnds} onmoveloop={moveLoopEnd} />

  <div class="time">
    <span class="num">{formatDuration(position / 1000) || "0:00"}</span>
    <div class="bar">
      <div class="bar-layer" aria-hidden="true">
        {#if loadedPercent !== null}<span class="loaded" style:width="{loadedPercent}%"></span>{/if}
        {#each ticks as tick, i (i)}<span class="tick" class:bar-line={tick.bar && !thinned} style:left="{percent(tick.ms)}%"></span>{/each}
        {#if loopEnds && max}
          {#if loopClosed}
            <span class="loop-region" class:active={data.loop.active} style:left="{percent(loopEnds.start_ms)}%" style:width="{percent(loopEnds.end_ms) - percent(loopEnds.start_ms)}%"></span>
          {:else}
            <span class="loop-mark" style:left="{percent(loopEnds.start_ms)}%"></span>
          {/if}
        {/if}
      </div>
      <input
        type="range"
        min="0"
        max={max || 1}
        step="100"
        value={Math.min(position, max || 1)}
        disabled={!track || !max}
        aria-label="Position on deck {letter}"
        aria-valuetext="{formatDuration(position / 1000) || '0:00'} of {formatDuration(duration) || 'unknown length'}"
        oninput={(event) => (seeking = Number(event.currentTarget.value))}
        onchange={(event) => seek(Number(event.currentTarget.value))}
        onblur={() => (seeking = null)}
      />
      {#if loopClosed && max}
        <div class="handles" bind:this={handles}>
          {#each [false, true] as end (end)}
            {@const ms = end ? loopEnds.end_ms : loopEnds.start_ms}
            <div
              class="handle"
              class:end
              role="slider"
              tabindex="0"
              style:left="{percent(ms)}%"
              aria-label="Loop {end ? 'end' : 'start'} on deck {letter}"
              aria-valuemin="0"
              aria-valuemax={Math.round(max)}
              aria-valuenow={Math.round(ms)}
              aria-valuetext={preciseTime(ms)}
              title="Loop {end ? 'end' : 'start'} {preciseTime(ms)}: drag, or use the arrow keys (one beat per step, Shift for four)"
              onpointerdown={(event) => grab(event, end)}
              onpointermove={(event) => pull(event, end)}
              onpointerup={() => (dragging = null)}
              onpointercancel={() => (dragging = null)}
              onkeydown={(event) => nudgeLoop(event, end)}
            ></div>
          {/each}
        </div>
      {/if}
    </div>
    <span class="num muted">{max ? `−${formatDuration(Math.max(0, max - position) / 1000) || "0:00"}` : "—"}</span>
  </div>
  <div class="deck-status small muted">
    <span role="status">{#if data.buffering && data.playing}Loading…{:else if track}Cue at {formatDuration(data.cue_ms / 1000) || "0:00"}{/if}</span>
    {#if loadedPercent !== null}<span title="The decoded part of the song held in memory">In memory {formatDuration(data.loaded_ms / 1000) || "0:00"} of {formatDuration(duration)}</span>{/if}
  </div>

  <div class="transport">
    <button class="play" class:learning={learnClass(`${deck}.play`)} data-control="{deck}.play" disabled={!track} aria-label="{data.playing ? 'Pause' : 'Play'} deck {letter}" aria-pressed={data.playing} onclick={() => dj.play(player, deck, !data.playing)}>
      <Icon name={data.playing ? "pause" : "play"} size={22} />
    </button>
    <button class="btn" class:learning={learnClass(`${deck}.cue`)} data-control="{deck}.cue" disabled={!track} title="Go to the cue point" onclick={() => dj.jumpToCue(player, deck)}><Icon name="cue" size={16} />Cue</button>
    <button class="btn" class:learning={learnClass(`${deck}.setCue`)} data-control="{deck}.setCue" disabled={!track} title="Set the cue point here" onclick={() => dj.setCue(player, deck)}>Set cue</button>
    <button class="icon-btn" class:active={cueing} disabled={!track && !cueing} aria-pressed={cueing} aria-label="Hear deck {letter} on headphones in this browser" title={cueing ? "Stop the headphone cue" : "Hear this deck before its fader, only in this browser"} onclick={() => cueListening.toggle(player, deck)}><Icon name="headphones" /></button>
    <button class="icon-btn" class:learning={learnClass(`${deck}.loadNext`)} data-control="{deck}.loadNext" aria-label="Load the next song onto deck {letter} from its queue or the main queue" title="Load next from the deck queue or the main queue" onclick={() => dj.loadNext(player, deck)}><Icon name="next" /></button>
    <button class="icon-btn" class:learning={learnClass(`${deck}.eject`)} data-control="{deck}.eject" disabled={!track} aria-label="Eject deck {letter}" title="Eject" onclick={() => dj.eject(player, deck)}><Icon name="eject" /></button>
  </div>

  <div class="loop" role="group" aria-label="Loop on deck {letter}">
    <div class="loop-row">
      <button class="btn loop-btn" class:learning={learnClass(`${deck}.loopIn`)} data-control="{deck}.loopIn" disabled={!track} aria-label="Set loop in on deck {letter}" title="Set the loop start here" onclick={() => dj.loopIn(player, deck)}>In</button>
      <button class="btn loop-btn" class:learning={learnClass(`${deck}.loopOut`)} data-control="{deck}.loopOut" disabled={!track || !loopEnds} aria-label="Set loop out on deck {letter}" title="Set the loop end here and start looping" onclick={() => dj.loopOut(player, deck)}>Out</button>
      <button class="btn loop-btn" class:on={data.loop?.active} class:learning={learnClass(`${deck}.loopToggle`)} data-control="{deck}.loopToggle" disabled={!track || !loopClosed} aria-pressed={Boolean(data.loop?.active)} aria-label="Loop deck {letter}" title={data.loop?.active ? "Leave the loop" : "Jump back into the loop"} onclick={() => dj.loopToggle(player, deck)}>Loop</button>
      <button class="btn loop-btn" class:on={data.quantize} class:learning={learnClass(`${deck}.quantize`)} data-control="{deck}.quantize" aria-pressed={Boolean(data.quantize)} aria-label="Quantise loops on deck {letter}" title="Snap loop ends to the beat grid" onclick={() => dj.quantize(player, deck, !data.quantize)}>Quantise</button>
    </div>
    <div class="loop-row" role="group" aria-label="Beat loops on deck {letter}">
      {#each DJ_BEAT_LOOPS as beats (beats)}
        <button class="chip" class:learning={learnClass(`${deck}.beatLoop.${beats}`)} data-control="{deck}.beatLoop.{beats}" disabled={!track} aria-label="Loop {beatsText(beats)} on deck {letter}" title="Loop {beatsText(beats)} from here" onclick={() => dj.beatLoop(player, deck, beats)}>{beatName(beats)}</button>
      {/each}
    </div>
    <div class="loop-row">
      <button class="chip" class:learning={learnClass(`${deck}.loopHalve`)} data-control="{deck}.loopHalve" disabled={!track || !loopClosed} aria-label="Halve the loop on deck {letter}" title="Halve the loop" onclick={() => dj.loopResize(player, deck, 0.5)}>½</button>
      <button class="chip" class:learning={learnClass(`${deck}.loopDouble`)} data-control="{deck}.loopDouble" disabled={!track || !loopClosed} aria-label="Double the loop on deck {letter}" title="Double the loop" onclick={() => dj.loopResize(player, deck, 2)}>×2</button>
      <button class="chip" class:learning={learnClass(`${deck}.loopBack`)} data-control="{deck}.loopBack" disabled={!track || !loopClosed} aria-label="Move the loop on deck {letter} back by its length" title="Move back by one loop" onclick={() => dj.loopMove(player, deck, "loop", -1)}>◀ Loop</button>
      <button class="chip" class:learning={learnClass(`${deck}.loopForward`)} data-control="{deck}.loopForward" disabled={!track || !loopClosed} aria-label="Move the loop on deck {letter} forward by its length" title="Move forward by one loop" onclick={() => dj.loopMove(player, deck, "loop", 1)}>Loop ▶</button>
      <button class="chip" disabled={!track || !loopClosed} aria-label="Move the loop on deck {letter} back by one beat" title="Move back by one beat" onclick={() => dj.loopMove(player, deck, "beat", -1)}>◀ Beat</button>
      <button class="chip" disabled={!track || !loopClosed} aria-label="Move the loop on deck {letter} forward by one beat" title="Move forward by one beat" onclick={() => dj.loopMove(player, deck, "beat", 1)}>Beat ▶</button>
    </div>
    {#if track && !data.grid}<p class="free small muted">Free loop: the beat grid is unknown, so loop ends stay exactly where you set them.</p>{/if}
  </div>

  <div class="strip">
    <div class="eq" role="group" aria-label="EQ and filter of deck {letter}">
      {#each bands as band (band.band)}
        <DjSlider vertical label={band.label} value={band.value} min={DJ_EQ_MIN} max={DJ_EQ_MAX} step="0.5" reset={0} text={decibelText(band.value)} shown={decibelText(band.value).replace(" dB", "").replace(", cut", "")}
          control="{deck}.eq.{band.band}" learning={learnClass(`${deck}.eq.${band.band}`)} onchange={(db) => dj.eq(player, deck, band.band, db)} />
      {/each}
      <DjSlider vertical label="Filter" value={filter} min={-1} max={1} step="0.01" reset={0} text={filterText(filter)} shown={filterText(filter).replace("Low-pass", "LP").replace("High-pass", "HP").replace(" %", "")}
        control="{deck}.filter" learning={learnClass(`${deck}.filter`)} onchange={(value) => dj.filter(player, deck, value)} />
    </div>
    <div class="fader">
      <DjSlider vertical label="Volume" value={volume} min={0} max={100} step="1" text="{volume} %"
        control="{deck}.volume" learning={learnClass(`${deck}.volume`)} onchange={(value) => dj.volume(player, deck, value)} />
    </div>
    <div class="tempo" role="group" aria-label="Tempo of deck {letter}">
      <DjSlider vertical label="Tempo" value={tempo} min={-data.tempo_range} max={data.tempo_range} step="0.05" reset={0} text={tempoText(tempo)}
        control="{deck}.tempo" learning={learnClass(`${deck}.tempo`)} onchange={(value) => dj.tempo(player, deck, value, data.tempo_range)} />
      <div class="range" class:learning={learnClass(`${deck}.range`)} data-control="{deck}.range" role="radiogroup" aria-label="Tempo range of deck {letter}">
        {#each DJ_TEMPO_RANGES as range (range)}
          <button class="chip" role="radio" aria-checked={data.tempo_range === range} aria-label="±{range} %" onclick={() => dj.tempoRange(player, deck, range)}>±{range}</button>
        {/each}
      </div>
      <button class="btn sync" class:learning={learnClass(`${deck}.sync`)} data-control="{deck}.sync" disabled={!data.bpm} title="Match the other deck's BPM" onclick={() => dj.sync(player, deck)}><Icon name="sync" size={16} />Sync</button>
    </div>
  </div>
</article>

<style>
  .deck { position: relative; display: grid; gap: 0.6rem; min-width: 0; padding: 0.9rem; }
  .deck.dropping { border-color: var(--accent); background: color-mix(in srgb, var(--accent-soft) 55%, var(--surface)); box-shadow: 0 0 0 2px var(--accent); }
  .drop-label { position: absolute; top: 0.5rem; left: 50%; z-index: 3; transform: translateX(-50%); margin: 0; padding: 0.3rem 0.8rem; border-radius: 999px; background: var(--accent); color: var(--accent-fg); font-size: 0.85rem; font-weight: 600; white-space: nowrap; pointer-events: none; }
  .deck-head { display: flex; align-items: center; gap: 0.75rem; min-width: 0; }
  .deck-letter { display: grid; place-items: center; flex: none; width: 1.8rem; height: 1.8rem; border-radius: 50%; background: var(--accent-soft); color: var(--accent); font-weight: 700; }
  .deck-text { flex: 1; min-width: 0; line-height: 1.3; }
  .deck-title { font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .deck-sub { font-size: 0.85rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .deck-sub.small { font-size: 0.75rem; }
  .bpm { display: grid; justify-items: end; flex: none; }
  .bpm-value { font-size: 1.35rem; font-weight: 700; line-height: 1.1; }
  .approx { margin-right: 0.1em; color: var(--muted); font-size: 0.9em; }
  .analysing { font-size: 0.7rem; }
  .deck-error { margin: 0; color: var(--error); overflow-wrap: anywhere; }
  .time { display: grid; grid-template-columns: auto minmax(0, 1fr) auto; align-items: center; gap: 0.6rem; font-size: 0.85rem; }
  .bar { position: relative; display: grid; align-items: center; min-width: 0; padding-top: 0.75rem; }
  .bar input { position: relative; z-index: 1; width: 100%; margin: 0; }
  .bar-layer, .handles { position: absolute; left: 8px; right: 8px; bottom: 0; pointer-events: none; }
  .bar-layer { top: 0.75rem; }
  .handles { top: 0; z-index: 2; }
  .loaded { position: absolute; left: 0; bottom: 1px; height: 3px; border-radius: 999px; background: color-mix(in srgb, var(--accent) 35%, transparent); }
  .tick { position: absolute; top: 30%; bottom: 30%; width: 1px; background: color-mix(in srgb, var(--muted) 35%, transparent); }
  .tick.bar-line { top: 12%; bottom: 12%; background: color-mix(in srgb, var(--muted) 75%, transparent); }
  .loop-region { position: absolute; top: 0; bottom: 0; border-inline: 2px solid color-mix(in srgb, var(--accent) 55%, transparent); border-radius: 3px; background: color-mix(in srgb, var(--accent) 12%, transparent); }
  .loop-region.active { border-color: var(--accent); background: color-mix(in srgb, var(--accent) 28%, transparent); }
  .loop-mark { position: absolute; top: 0; bottom: 0; width: 2px; background: var(--accent); }
  .handle { position: absolute; top: 0; width: 0.8rem; height: 0.75rem; transform: translateX(-100%); border-radius: 3px 0 0 3px; background: var(--accent); cursor: ew-resize; pointer-events: auto; touch-action: none; }
  .handle.end { transform: none; border-radius: 0 3px 3px 0; }
  .handle::after { content: ""; position: absolute; top: 100%; right: -1px; width: 2px; height: 1.2rem; background: var(--accent); pointer-events: none; }
  .handle.end::after { right: auto; left: -1px; }
  .deck-status { display: flex; flex-wrap: wrap; justify-content: center; gap: 0 0.75rem; min-height: 1.1rem; margin-top: -0.35rem; text-align: center; }
  .transport { display: flex; flex-wrap: wrap; align-items: center; justify-content: center; gap: 0.4rem; }
  .play { width: 2.8rem; height: 2.8rem; display: grid; place-items: center; border: none; border-radius: 50%; background: var(--accent); color: var(--accent-fg); }
  .play:hover:not(:disabled) { filter: brightness(1.08); }
  .play:disabled { opacity: 0.4; }
  .transport .btn { padding: 0.4rem 0.75rem; }
  .strip { display: grid; grid-template-columns: minmax(0, 4fr) minmax(3rem, 1fr) minmax(5.5rem, 1.5fr); gap: 0.5rem; align-items: start; padding-top: 0.5rem; border-top: 1px solid var(--border); }
  .eq { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 0.15rem; }
  .fader { display: grid; justify-items: center; }
  .tempo { display: grid; justify-items: center; gap: 0.4rem; }
  .range { display: flex; flex-wrap: wrap; justify-content: center; gap: 0.25rem; }
  .chip { padding: 0.15rem 0.45rem; border: 1px solid var(--border); border-radius: 999px; background: var(--surface); color: var(--muted); font-size: 0.75rem; white-space: nowrap; }
  .chip[aria-checked="true"] { border-color: var(--accent); background: var(--accent-soft); color: var(--accent); }
  .chip:disabled { opacity: 0.45; }
  .loop { display: grid; gap: 0.35rem; padding-top: 0.5rem; border-top: 1px solid var(--border); }
  .loop-row { display: flex; flex-wrap: wrap; justify-content: center; gap: 0.3rem; }
  .loop-row .chip { min-width: 2rem; padding: 0.2rem 0.5rem; }
  .loop-btn { padding: 0.3rem 0.65rem; font-size: 0.85rem; }
  .loop-btn.on { border-color: var(--accent); background: var(--accent-soft); color: var(--accent); }
  .free { margin: 0; text-align: center; }
  .sync { padding: 0.3rem 0.6rem; font-size: 0.85rem; }
  .learning { outline: 2px dashed var(--accent); outline-offset: 3px; }
  @media (max-width: 480px) {
    .strip { grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); }
    .eq { grid-column: 1 / -1; }
  }
</style>
