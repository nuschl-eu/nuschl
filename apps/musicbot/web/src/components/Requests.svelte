<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { budgetText, buildRoute, releaseTypeLabel, requestProgress, requestStatusText } from "../logic.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";

  const namespace = store.browseKey;
  const current = $derived(namespace !== null && namespace === store.browseKey && store.connection === "open" && !store.invalid);
  const revision = $derived(store.snapshot?.import?.revision ?? null);
  // Loads on mount and again whenever the import revision in the live state changes.
  const result = resource(() => (current && revision !== null ? "api/import/requests" : null), () => store.browseSignal, undefined, { keep: true });
  const data = $derived(current ? result.data : null);
  const requests = $derived(Array.isArray(data?.requests) ? data.requests : []);
  const budget = $derived(data ? budgetText(data.budget) : null);

  function localTime(seconds) { return new Date(seconds * 1000).toLocaleString(); }
</script>

<section aria-labelledby="requests-title" aria-busy={result.loading}>
  <div class="section-heading">
    <h1 id="requests-title">Requests</h1>
    {#if revision !== null}<a class="heading-link" href={buildRoute({ view: "add", q: "" })}><Icon name="add" size={18} /> Add music</a>{/if}
  </div>
  {#if revision === null}
    <p class="notice muted">Adding music is not available on this bot.</p>
  {:else if !current}
    <p class="notice muted" role="status">Requests will update after the connection is restored.</p>
  {:else}
    {#if budget}<p class="budget">{budget}</p>{/if}
    {#if result.error}<div class="notice error" role="alert"><p>{result.error}</p><button class="btn" onclick={() => result.reload()}>Try again</button></div>{/if}
    {#if result.loading && !data}<p class="notice" role="status">Loading requests…</p>{/if}
    {#if data && !requests.length}<p class="notice muted">No requests yet.</p>{/if}
    <ol class="rows card request-list" aria-label="Requests">
      {#each requests as request (request.id)}
        {@const progress = requestProgress(request)}
        <li>
          <Cover importId={request.has_cover ? request.album_id : null} size={128} px={56} />
          <div class="row-text">
            <div class="request-heading">
              <div class="row-title"><a href={buildRoute({ view: "addAlbum", id: request.album_id })}>{request.title}</a></div>
              {#if request.mine}<span class="badge accent">Yours</span>{/if}
            </div>
            <div class="muted">{request.artist}</div>
            <p class="metadata muted">
              {#if request.year}<span>{request.year}</span>{/if}
              {#if releaseTypeLabel(request.release_type)}<span>{releaseTypeLabel(request.release_type)}</span>{/if}
              {#if Number.isFinite(request.requested_at)}<span>Asked for <time datetime={new Date(request.requested_at * 1000).toISOString()}>{localTime(request.requested_at)}</time></span>{/if}
            </p>
            <p class="status" class:failed={request.status === "failed"} class:done={request.status === "handed_over"}>{requestStatusText(request)}</p>
            {#if progress}<progress value={progress.value} max={progress.max} aria-label="Tracks added for {request.title}"></progress>{/if}
          </div>
        </li>
      {/each}
    </ol>
  {/if}
</section>

<style>
  .budget { display: inline-flex; margin: 0 0 0.5rem; padding: 0.25rem 0.8rem; border-radius: 999px; background: var(--surface-2); font-size: 0.85rem; font-weight: 600; font-variant-numeric: tabular-nums; }
  .request-list { margin: 1rem 0; }
  .request-list > li { align-items: flex-start; padding-block: 0.75rem; }
  .row-text { overflow-wrap: anywhere; }
  .request-heading { display: flex; align-items: start; justify-content: space-between; gap: 0.5rem; }
  .request-heading .row-title { min-width: 0; white-space: normal; }
  .row-text > .muted:not(.metadata) { font-size: 0.9rem; }
  .metadata { display: flex; flex-wrap: wrap; gap: 0 0.75rem; font-size: 0.85rem; margin: 0.35rem 0; }
  .status { margin: 0.35rem 0 0; font-weight: 500; }
  .status.done { color: var(--success); }
  .status.failed { color: var(--error); }
  progress { width: 100%; max-width: 20rem; margin-top: 0.5rem; }
</style>
