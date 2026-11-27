<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { query } from "../api.js";
  import {
    DJ_WAVE_ZOOMS, SPECTRUM_COLUMN_MS, deckPosition, findRange, gridTicks, heatColour, insertRange, levelHeight,
    parseColour, pruneRanges, rangesToFetch, timeX, waveWindow, xTime,
  } from "../logic.js";

  /** Spectrum columns kept per song and view: within 60 s of the playhead, 30,000 at most. */
  const KEEP_COLUMNS = 6000;
  const CAP_COLUMNS = 30000;
  /** Spectrum requests in flight per song and view at most. */
  const PARALLEL = 2;
  /** Wait before asking again for columns the bot is still computing, or after a failed request. */
  const RETRY_MS = 1000;
  /** Wait before asking again while the deck streams its song. */
  const STREAM_RETRY_MS = 3000;
  /** Overview refresh while the song decodes. */
  const OVERVIEW_REFRESH_MS = 3000;
  /** Pointer distance in CSS pixels that grabs a loop end on the waveform. */
  const GRAB_PX = 8;
  /** Beat-grid ticks drawn on the waveform at most. */
  const MAX_TICKS = 400;
  const FINE_BANDS = 24;
  /** Bar height of the low, mid and high band relative to the canvas: the higher bands draw inside the lower ones. */
  const BAND_SCALE = [1, 0.85, 0.7];

  /**
   * The waveform of a deck around its playhead and an overview of the whole
   * song. `loopEnds` are the loop ends the deck shows; `onmoveloop(end, ms)`
   * moves the loop start (`end` false) or end (`end` true).
   */
  let { player, deck, data, duration, loopEnds, onmoveloop } = $props();
  const letter = $derived(deck === 0 ? "A" : "B");
  const entry = $derived(data.entry?.id ?? null);
  const durationMs = $derived(typeof duration === "number" && duration > 0 ? duration * 1000 : 0);
  let zoom = $state(6);
  let spectrogram = $state(false);
  let streaming = $state(false);
  let wave = $state();
  let strip = $state();
  let grabbing = $state(null);
  let near = $state(false);

  // Drawing state outside Svelte's reactivity: the canvases redraw from it.
  let palette = null;
  let caches = new Map();
  let overview = null;
  let frame = 0;
  let image = null;

  const position = () => deckPosition(data.position_ms, data.advancing, data.tempo, store.receivedAt, performance.now(), duration);

  function readPalette() {
    const style = getComputedStyle(wave);
    const token = (name) => style.getPropertyValue(name).trim();
    const stops = ["--surface-2", "--accent-soft", "--accent", "--warning", "--error"].map((name) => parseColour(token(name)) ?? [128, 128, 128]);
    const heat = new Uint8ClampedArray(256 * 3);
    for (let byte = 0; byte < 256; byte++) heat.set(heatColour(byte, stops), byte * 3);
    palette = { back: token("--surface-2"), bands: [token("--accent"), token("--warning"), token("--fg")], fg: token("--fg"), muted: token("--muted"), accent: token("--accent"), cue: token("--error"), heat };
  }

  function requestDraw() {
    if (!frame) frame = requestAnimationFrame(render);
  }

  function render() {
    frame = 0;
    if (!wave || !strip || !palette) return;
    const positionMs = position();
    drawWave(positionMs);
    drawStrip(positionMs);
    if (data.advancing && document.visibilityState === "visible") frame = requestAnimationFrame(render);
  }

  function fit(canvas) {
    const ratio = window.devicePixelRatio || 1;
    const width = Math.round(canvas.clientWidth * ratio);
    const height = Math.round(canvas.clientHeight * ratio);
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
    return ratio;
  }

  // Fetching: per song and view, chunks of columns near the playhead.

  function cacheFor(view) {
    const key = `${player}:${deck}:${entry}:${view}`;
    let cache = caches.get(key);
    if (!cache) {
      cache = { key, player, deck, entry, view, chunks: [], inflight: [], computed: null, total: 0, retryAt: 0, timer: 0, streaming: false, controller: new AbortController() };
      caches.set(key, cache);
    }
    return cache;
  }

  function abortAll() {
    for (const cache of caches.values()) {
      cache.controller.abort();
      clearTimeout(cache.timer);
    }
    caches = new Map();
  }

  function retry(cache, ms) {
    cache.retryAt = performance.now() + ms;
    clearTimeout(cache.timer);
    cache.timer = setTimeout(() => { if (!cache.controller.signal.aborted) requestDraw(); }, ms);
  }

  /** Requests the next missing columns of the window plus a margin of one window on each side. */
  function ensure(cache, positionMs) {
    if (cache.inflight.length >= PARALLEL || performance.now() < cache.retryAt) return;
    const wanted = waveWindow(positionMs, zoom * 2);
    if (cache.total > 0) wanted.to = Math.min(wanted.to, cache.total);
    const busy = [...cache.chunks, ...cache.inflight];
    const [next] = rangesToFetch(busy, wanted, cache.computed);
    if (next) {
      request(cache, next);
      return;
    }
    // Columns the bot is still computing; a short answer delays the next try by RETRY_MS.
    const complete = cache.total > 0 && cache.computed >= cache.total;
    if (cache.computed !== null && wanted.to > cache.computed && !complete) {
      const [later] = rangesToFetch(busy, wanted, null);
      if (later) request(cache, later);
    }
  }

  async function request(cache, range) {
    cache.inflight.push(range);
    try {
      const response = await fetch(query("api/spectrum", { player: cache.player, deck: cache.deck, entry: cache.entry, view: cache.view, from: range.from, count: range.to - range.from }), { signal: cache.controller.signal });
      if (response.status === 404) {
        cache.streaming = true;
        retry(cache, STREAM_RETRY_MS);
        return;
      }
      // 409: the deck holds another song now, and its state event replaces this cache.
      if (!response.ok) {
        retry(cache, RETRY_MS);
        return;
      }
      const bands = Number(response.headers.get("X-Spectrum-Bands")) || (cache.view === "coarse" ? 3 : FINE_BANDS);
      const from = Number(response.headers.get("X-Spectrum-From"));
      const computed = Number(response.headers.get("X-Spectrum-Columns"));
      const total = Number(response.headers.get("X-Spectrum-Total"));
      if (!Number.isInteger(bands) || bands < 1 || bands > 64 || !Number.isInteger(from) || from < 0) {
        retry(cache, RETRY_MS);
        return;
      }
      const bytes = new Uint8Array(await response.arrayBuffer());
      cache.streaming = false;
      if (Number.isFinite(computed)) cache.computed = computed;
      cache.total = Number.isFinite(total) && total > 0 ? total : 0;
      const columns = Math.min(Math.floor(bytes.length / bands), range.to - range.from);
      if (columns > 0) {
        const chunk = { from, to: from + columns, bytes, bands };
        cache.chunks = pruneRanges(insertRange(cache.chunks, chunk), Math.round(position() / SPECTRUM_COLUMN_MS), KEEP_COLUMNS, CAP_COLUMNS);
      }
      if (columns < range.to - range.from) retry(cache, RETRY_MS);
    } catch {
      if (!cache.controller.signal.aborted) retry(cache, RETRY_MS);
    } finally {
      cache.inflight = cache.inflight.filter((item) => item !== range);
      if (!cache.controller.signal.aborted) requestDraw();
    }
  }

  /** Copies the levels of `column` into `out`; false when the column is missing. */
  function readColumn(cache, column, out) {
    const index = findRange(cache.chunks, column);
    if (index < 0) return false;
    const chunk = cache.chunks[index];
    const at = (column - chunk.from) * chunk.bands;
    for (let band = 0; band < out.length; band++) out[band] = chunk.bytes[at + band] ?? 0;
    return true;
  }

  // Drawing.

  /** Mirrored low, mid and high bars, one per column, or one per pixel holding the loudest of its columns. */
  function drawBands(ctx, cache, positionMs, width, height) {
    const { from, to } = waveWindow(positionMs, zoom);
    const first = Math.max(0, from);
    const last = Math.min(to, Math.ceil(durationMs / SPECTRUM_COLUMN_MS));
    const mid = height / 2;
    const paths = [new Path2D(), new Path2D(), new Path2D()];
    const levels = [0, 0, 0];
    const pooled = [0, 0, 0];
    const add = (x, w, values) => {
      for (let band = 0; band < 3; band++) {
        const half = levelHeight(values[band]) * mid * BAND_SCALE[band];
        if (half > 0) paths[band].rect(x, mid - half, w, half * 2);
      }
    };
    const columnWidth = width / ((zoom * 2000) / SPECTRUM_COLUMN_MS);
    if (columnWidth >= 1) {
      for (let column = first; column < last; column++) {
        if (readColumn(cache, column, levels)) add(timeX(column * SPECTRUM_COLUMN_MS, positionMs, zoom, width), columnWidth + 0.5, levels);
      }
    } else {
      for (let x = 0; x < width; x++) {
        const c0 = Math.max(first, Math.floor(xTime(x, positionMs, zoom, width) / SPECTRUM_COLUMN_MS));
        const c1 = Math.min(last, Math.max(c0 + 1, Math.floor(xTime(x + 1, positionMs, zoom, width) / SPECTRUM_COLUMN_MS)));
        let found = false;
        pooled.fill(0);
        for (let column = c0; column < c1; column++) {
          if (!readColumn(cache, column, levels)) continue;
          found = true;
          for (let band = 0; band < 3; band++) pooled[band] = Math.max(pooled[band], levels[band]);
        }
        if (found) add(x, 1, pooled);
      }
    }
    ctx.globalAlpha = 0.85;
    for (let band = 0; band < 3; band++) {
      ctx.fillStyle = palette.bands[band];
      ctx.fill(paths[band]);
    }
    ctx.globalAlpha = 1;
  }

  /** The fine bands as a heat map, lowest band at the bottom, scaled from one pixel per column and band. */
  function drawSpectrogram(ctx, cache, positionMs, width, height) {
    const { from, to } = waveWindow(positionMs, zoom);
    const first = Math.max(0, from);
    const last = Math.min(to, Math.ceil(durationMs / SPECTRUM_COLUMN_MS));
    const count = last - first;
    if (count <= 0) return;
    const capacity = Math.ceil((zoom * 2000) / SPECTRUM_COLUMN_MS) + 2;
    if (!image || image.canvas.width !== capacity) {
      const canvas = document.createElement("canvas");
      canvas.width = capacity;
      canvas.height = FINE_BANDS;
      const context = canvas.getContext("2d");
      image = { canvas, context, pixels: context.createImageData(capacity, FINE_BANDS) };
    }
    const pixels = image.pixels.data;
    pixels.fill(0);
    for (let column = first; column < last; column++) {
      const index = findRange(cache.chunks, column);
      if (index < 0) continue;
      const chunk = cache.chunks[index];
      const at = (column - chunk.from) * chunk.bands;
      for (let band = 0; band < Math.min(FINE_BANDS, chunk.bands); band++) {
        const value = chunk.bytes[at + band] ?? 0;
        const offset = ((FINE_BANDS - 1 - band) * capacity + (column - first)) * 4;
        pixels[offset] = palette.heat[value * 3];
        pixels[offset + 1] = palette.heat[value * 3 + 1];
        pixels[offset + 2] = palette.heat[value * 3 + 2];
        pixels[offset + 3] = 255;
      }
    }
    image.context.putImageData(image.pixels, 0, 0);
    const x0 = timeX(first * SPECTRUM_COLUMN_MS, positionMs, zoom, width);
    const x1 = timeX(last * SPECTRUM_COLUMN_MS, positionMs, zoom, width);
    ctx.imageSmoothingEnabled = true;
    ctx.drawImage(image.canvas, 0, 0, count, FINE_BANDS, x0, 0, x1 - x0, height);
  }

  function drawWave(positionMs) {
    const ratio = fit(wave);
    const ctx = wave.getContext("2d");
    const { width, height } = wave;
    const toX = (ms) => timeX(ms, positionMs, zoom, width);
    ctx.fillStyle = palette.back;
    ctx.fillRect(0, 0, width, height);
    ctx.globalAlpha = 0.5;
    ctx.fillStyle = palette.muted;
    ctx.fillRect(0, height / 2 - ratio / 2, width, ratio);
    ctx.globalAlpha = 1;
    if (entry === null || !durationMs) return;

    const cache = cacheFor(spectrogram ? "fine" : "coarse");
    ensure(cache, positionMs);
    if (streaming !== cache.streaming) streaming = cache.streaming;
    if (spectrogram) drawSpectrogram(ctx, cache, positionMs, width, height);
    else drawBands(ctx, cache, positionMs, width, height);

    // Beat grid: bars as full-height lines, beats as short ticks at the edges.
    ctx.fillStyle = palette.fg;
    for (const tick of gridTicks(data.grid, Math.max(0, positionMs - zoom * 1000), Math.min(durationMs, positionMs + zoom * 1000), MAX_TICKS)) {
      const x = toX(tick.ms) - ratio / 2;
      if (tick.bar) {
        ctx.globalAlpha = 0.35;
        ctx.fillRect(x, 0, ratio, height);
      } else {
        ctx.globalAlpha = 0.45;
        ctx.fillRect(x, 0, ratio, height * 0.15);
        ctx.fillRect(x, height * 0.85, ratio, height * 0.15);
      }
    }
    ctx.globalAlpha = 1;

    if (loopEnds) {
      const start = toX(loopEnds.start_ms);
      ctx.fillStyle = palette.accent;
      if (loopEnds.end_ms != null) {
        const end = toX(loopEnds.end_ms);
        ctx.globalAlpha = data.loop?.active ? 0.28 : 0.12;
        ctx.fillRect(start, 0, end - start, height);
        ctx.globalAlpha = 1;
        ctx.fillRect(end - ratio, 0, 2 * ratio, height);
        ctx.fillRect(end, 0, 8 * ratio, 7 * ratio);
        ctx.fillRect(start - 8 * ratio, 0, 8 * ratio, 7 * ratio);
      }
      ctx.fillRect(start - ratio, 0, 2 * ratio, height);
    }

    // Cue point: a line with a triangle at the bottom.
    const cue = toX(data.cue_ms);
    ctx.fillStyle = palette.cue;
    ctx.fillRect(cue - ratio / 2, 0, ratio, height);
    ctx.beginPath();
    ctx.moveTo(cue - 5 * ratio, height);
    ctx.lineTo(cue + 5 * ratio, height);
    ctx.lineTo(cue, height - 6 * ratio);
    ctx.closePath();
    ctx.fill();

    ctx.fillStyle = palette.fg;
    ctx.fillRect(width / 2 - ratio, 0, 2 * ratio, height);
  }

  function drawStrip(positionMs) {
    const ratio = fit(strip);
    const ctx = strip.getContext("2d");
    const { width, height } = strip;
    ctx.fillStyle = palette.back;
    ctx.fillRect(0, 0, width, height);
    if (entry === null || !durationMs) return;
    const toX = (ms) => (ms / durationMs) * width;
    const mid = height / 2;

    if (overview && overview.entry === entry) {
      const bytes = overview.bytes;
      const count = Math.floor(bytes.length / 3);
      const paths = [new Path2D(), new Path2D(), new Path2D()];
      for (let x = 0; x < width && count > 0; x++) {
        const c0 = Math.floor((x * count) / width);
        const c1 = Math.max(c0 + 1, Math.floor(((x + 1) * count) / width));
        for (let band = 0; band < 3; band++) {
          let level = 0;
          for (let column = c0; column < c1 && column < count; column++) level = Math.max(level, bytes[column * 3 + band]);
          const half = levelHeight(level) * mid * BAND_SCALE[band];
          if (half > 0) paths[band].rect(x, mid - half, 1, half * 2);
        }
      }
      ctx.globalAlpha = 0.85;
      for (let band = 0; band < 3; band++) {
        ctx.fillStyle = palette.bands[band];
        ctx.fill(paths[band]);
      }
      ctx.globalAlpha = 1;
    }

    if (loopEnds && loopEnds.end_ms != null) {
      ctx.fillStyle = palette.accent;
      ctx.globalAlpha = data.loop?.active ? 0.4 : 0.2;
      ctx.fillRect(toX(loopEnds.start_ms), 0, Math.max(ratio, toX(loopEnds.end_ms - loopEnds.start_ms)), height);
      ctx.globalAlpha = 1;
    }
    // The waveform's window.
    ctx.strokeStyle = palette.fg;
    ctx.lineWidth = ratio;
    ctx.globalAlpha = 0.7;
    ctx.strokeRect(toX(positionMs - zoom * 1000), ratio / 2, toX(zoom * 2000), height - ratio);
    ctx.globalAlpha = 1;
    ctx.fillStyle = palette.fg;
    ctx.fillRect(toX(positionMs) - ratio, 0, 2 * ratio, height);
  }

  // Loop ends dragged on the waveform.

  function pointerAt(event) {
    const rect = wave.getBoundingClientRect();
    return { x: event.clientX - rect.left, width: rect.width };
  }

  /** The loop end within GRAB_PX of `x` (false: the start, true: the end), or null. */
  function loopEndNear(x, width) {
    if (!loopEnds || loopEnds.end_ms == null || !durationMs) return null;
    const positionMs = position();
    const start = Math.abs(timeX(loopEnds.start_ms, positionMs, zoom, width) - x);
    const end = Math.abs(timeX(loopEnds.end_ms, positionMs, zoom, width) - x);
    if (Math.min(start, end) > GRAB_PX) return null;
    return end < start;
  }

  function grab(event) {
    const { x, width } = pointerAt(event);
    const end = loopEndNear(x, width);
    if (end === null) return;
    event.preventDefault();
    wave.setPointerCapture(event.pointerId);
    grabbing = end;
  }

  function pull(event) {
    const { x, width } = pointerAt(event);
    if (grabbing === null) near = loopEndNear(x, width) !== null;
    else onmoveloop(grabbing, xTime(x, position(), zoom, width));
  }

  $effect(() => {
    readPalette();
    const scheme = matchMedia("(prefers-color-scheme: dark)");
    const recolour = () => {
      readPalette();
      requestDraw();
    };
    scheme.addEventListener("change", recolour);
    document.addEventListener("visibilitychange", requestDraw);
    const observer = new ResizeObserver(requestDraw);
    observer.observe(wave);
    observer.observe(strip);
    return () => {
      scheme.removeEventListener("change", recolour);
      document.removeEventListener("visibilitychange", requestDraw);
      observer.disconnect();
      cancelAnimationFrame(frame);
      frame = 0;
      abortAll();
    };
  });

  // A new song on the deck drops the cached columns and their requests.
  $effect(() => {
    player;
    deck;
    entry;
    streaming = false;
    return abortAll;
  });

  // The overview: refreshed while the song decodes, and once more when the deck has all of it.
  $effect(() => {
    const target = { player, deck, entry };
    const preloaded = data.preloaded;
    if (entry === null) return;
    const controller = new AbortController();
    let timer = 0;
    async function load() {
      let done = false;
      try {
        const response = await fetch(query("api/spectrum/overview", target), { signal: controller.signal });
        if (response.status === 409) return;
        if (response.ok) {
          const columns = Number(response.headers.get("X-Spectrum-Columns"));
          const total = Number(response.headers.get("X-Spectrum-Total"));
          overview = { entry: target.entry, bytes: new Uint8Array(await response.arrayBuffer()) };
          done = total > 0 ? columns >= total : preloaded;
          requestDraw();
        }
      } catch {
        if (controller.signal.aborted) return;
      }
      if (!done) timer = setTimeout(load, OVERVIEW_REFRESH_MS);
    }
    load();
    return () => {
      controller.abort();
      clearTimeout(timer);
    };
  });

  $effect(() => {
    data.position_ms;
    data.advancing;
    data.tempo;
    data.cue_ms;
    data.grid;
    data.loop;
    store.receivedAt;
    loopEnds;
    zoom;
    spectrogram;
    entry;
    durationMs;
    requestDraw();
  });
</script>

<div class="deck-wave">
  <div class="wave-tools">
    <div class="zoom" role="radiogroup" aria-label="Waveform zoom of deck {letter}">
      {#each DJ_WAVE_ZOOMS as seconds (seconds)}
        <button class="chip" role="radio" aria-checked={zoom === seconds} aria-label="±{seconds} seconds around the playhead" onclick={() => (zoom = seconds)}>±{seconds} s</button>
      {/each}
    </div>
    <button class="chip" aria-pressed={spectrogram} title="Show the frequency bands as a heat map" onclick={() => (spectrogram = !spectrogram)}>Spectrogram</button>
  </div>
  <div class="wave-box">
    <div role="img" aria-label="{spectrogram ? 'Spectrogram' : 'Waveform'} of deck {letter}, ±{zoom} seconds around the playhead">
      <canvas
        class="wave"
        class:grab={near || grabbing !== null}
        bind:this={wave}
        onpointerdown={grab}
        onpointermove={pull}
        onpointerup={() => (grabbing = null)}
        onpointercancel={() => (grabbing = null)}
        onpointerleave={() => (near = false)}
      ></canvas>
    </div>
    {#if streaming}<p class="wave-note small muted">The deck streams this song; its waveform appears once the song is in memory.</p>{/if}
  </div>
  <div role="img" aria-label="Overview of the song on deck {letter}"><canvas class="strip" bind:this={strip}></canvas></div>
</div>

<style>
  .deck-wave { display: grid; gap: 0.35rem; min-width: 0; }
  .wave-tools { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: 0.3rem; }
  .zoom { display: flex; gap: 0.25rem; }
  .chip { padding: 0.15rem 0.5rem; border: 1px solid var(--border); border-radius: 999px; background: var(--surface); color: var(--muted); font-size: 0.75rem; white-space: nowrap; }
  .chip[aria-checked="true"], .chip[aria-pressed="true"] { border-color: var(--accent); background: var(--accent-soft); color: var(--accent); }
  .wave-box { position: relative; }
  canvas { display: block; width: 100%; border-radius: var(--radius); }
  .wave { height: 5.5rem; touch-action: pan-y; }
  .wave.grab { cursor: ew-resize; }
  .strip { height: 1.75rem; }
  .wave-note { position: absolute; inset: auto 0.5rem 0.4rem; margin: 0; text-align: center; pointer-events: none; }
</style>
