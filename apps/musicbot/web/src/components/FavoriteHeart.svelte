<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { query } from "../api.js";
  import Icon from "./Icon.svelte";

  /** The heart for one song: whether it is in the viewer's Favorites of its library, and a toggle. */
  let { library, songId, title, namespace, size = 20 } = $props();
  let pending = $state(null);
  const canOwn = $derived(store.canOwnPlaylists);
  const current = $derived(typeof songId === "string" && songId.length > 0 && store.browseItemCurrent({ library, namespace }));
  const revision = $derived(store.playlistRevision);
  const result = resource(() => {
    if (!current || !canOwn) return null;
    revision;
    return query("api/favorites", { library, id: songId });
  }, () => store.browseSignal, undefined, { keep: true });
  // The answer belongs to the song it was asked for; a reload of the same song keeps it showing.
  const loaded = $derived(result.data !== null && result.error === null ? result.data.favorite === true : null);
  const favorite = $derived(pending?.song === songId ? pending.value : loaded);
  const label = $derived(!canOwn
    ? "Favorites need a Mumble certificate. Set one up in Mumble, then send !web for a new link."
    : favorite ? "Remove from your Favorites" : "Add to your Favorites");

  async function toggle() {
    if (!canOwn || !current || favorite === null || (pending !== null && pending.revision === undefined)) return;
    const song = songId;
    const value = !favorite;
    pending = { song, value };
    const reply = await store.playlistChange("api/favorites", { library, id: song, favorite: value }, namespace, { reply: true });
    if (pending?.song !== song) return;
    // The reply stands until the next playlist revision has been loaded.
    if (reply === null) pending = null;
    else pending = { song, value: reply.favorite === true, revision: store.playlistRevision };
  }

  $effect(() => {
    if (pending === null) return;
    if (pending.song !== songId || (pending.revision !== undefined && pending.revision !== revision && !result.loading)) pending = null;
  });
</script>

<button
  class="icon-btn heart"
  class:active={favorite === true}
  disabled={!current || !canOwn || favorite === null || (pending !== null && pending.revision === undefined)}
  aria-pressed={favorite === true}
  aria-label="Favorite {title}"
  title={label}
  onclick={toggle}
><Icon name={favorite ? "heart" : "heart-outline"} {size} /></button>

<style>
  .heart.active, .heart.active:hover:not(:disabled) { color: var(--error); }
</style>
