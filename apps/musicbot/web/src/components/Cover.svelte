<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { artUrl, importArtUrl } from '../api.js';
  import { coverRetryDelay } from '../logic.js';
  import Icon from './Icon.svelte';
  import { store } from '../store.svelte.js';

  /** The bot answers with PNGs the content worker made; 400 pixels at most. */
  const MAX_EDGE = 400;

  /** `importId` names an import service album; its cover comes from `import-art/` instead of the library. */
  let { library, art, importId = null, size = 300, px = null, round = false } = $props();
  const bound = $derived(Math.max(32, Math.min(MAX_EDGE, Math.trunc(size))));
  // Covers load only while this browser's link works.
  const allowed = $derived(store.connection === 'open' && !store.invalid);
  const src = $derived(!allowed ? null : importId !== null ? importArtUrl(importId, bound)
    : art ? artUrl(library, art, bound, store.snapshot?.library_keys?.[library]) : null);
  let loaded = $state(null);
  const ready = $derived(src !== null && loaded === src);
  // Failed loads of `src` so far; a new image element loads it again after a
  // growing delay, since the bot refuses covers while its artwork worker is busy.
  let failures = $state({ src: null, count: 0 });
  const attempt = $derived(failures.src === src ? failures.count : 0);
  let retry = $state({ src: null, count: 0 });
  const shown = $derived(retry.src === src ? retry.count : 0);
  let timer;
  function failed() {
    loaded = null;
    const count = attempt + 1;
    failures = { src, count };
    const delay = coverRetryDelay(count);
    clearTimeout(timer);
    if (delay !== null) {
      const again = src;
      timer = setTimeout(() => { if (again === src) retry = { src: again, count }; }, delay);
    }
  }
  $effect(() => () => clearTimeout(timer));
</script>

<div class="cover" class:fill={px === null} class:round style:width={px === null ? null : `${px}px`} style:height={px === null ? null : `${px}px`}>
  {#if src}
    {#key shown}
      <img {src} alt="" aria-hidden="true" loading="lazy" decoding="async" class:ready
           onload={() => (loaded = src)} onerror={failed} />
    {/key}
  {/if}
  {#if !ready}<div class="placeholder"><Icon name="note" size={px ? Math.max(16, Math.round(px * 0.4)) : 48} /></div>{/if}
</div>

<style>
  .cover { position: relative; flex: none; overflow: hidden; border-radius: 6px; background: var(--surface-2); }
  .cover.fill { width: 100%; aspect-ratio: 1; }
  .cover.round { border-radius: 50%; }
  img { display: block; width: 100%; height: 100%; object-fit: cover; opacity: 0; }
  img.ready { opacity: 1; }
  .placeholder { position: absolute; inset: 0; display: grid; place-items: center; color: var(--muted); opacity: 0.6; }
</style>
