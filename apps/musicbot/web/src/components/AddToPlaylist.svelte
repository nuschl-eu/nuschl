<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { tick } from "svelte";
  import { store } from "../store.svelte.js";
  import { getJSON, query } from "../api.js";
  import { editablePlaylists } from "../logic.js";
  import Icon from "./Icon.svelte";

  /**
   * "Add to playlist" for songs of `library`: `songs` (song ids) or `album` (an
   * album id). The menu lists the caller's playlists and shared ones, plus "New
   * playlist". `up` opens it above the button, extending to the right (the player bar).
   */
  let { library, songs = null, album = null, title, namespace, up = false, size = 20 } = $props();
  const id = $props.id();
  let open = $state(false);
  let root = $state();
  let button = $state();
  let menu = $state();
  let playlists = $state.raw(null);
  let loadError = $state("");
  let creating = $state(false);
  let name = $state("");
  let busy = $state(false);
  let request = null;
  const current = $derived(store.browseItemCurrent({ library, namespace }));
  const canOwn = $derived(store.canOwnPlaylists);
  const choices = $derived(editablePlaylists(playlists));
  const source = $derived(album !== null ? { album } : { songs });
  const explanation = "Owning and editing playlists needs a Mumble certificate. Set one up in Mumble, then send !web for a new link.";

  async function load() {
    request?.abort();
    const controller = new AbortController();
    request = controller;
    const signal = store.browseSignal;
    const abort = () => controller.abort();
    signal?.addEventListener("abort", abort, { once: true });
    playlists = null;
    loadError = "";
    try {
      const reply = await getJSON(query("api/playlists", { library }), controller.signal);
      if (request === controller) playlists = reply.playlists;
    } catch (error) {
      if (request === controller && !controller.signal.aborted) loadError = error instanceof Error ? error.message : String(error);
    } finally {
      signal?.removeEventListener("abort", abort);
    }
  }

  async function toggle() {
    if (open) return close();
    open = true;
    creating = false;
    name = "";
    load();
    await tick();
    menu?.querySelector("button:not(:disabled)")?.focus();
  }

  function close(focus = true) {
    open = false;
    request?.abort();
    request = null;
    if (focus) button?.focus();
  }

  async function addTo(playlist) {
    if (busy) return;
    busy = true;
    const reply = await store.playlistChange("api/playlists/add", { library, id: playlist.id, ...source }, namespace, { reply: true });
    busy = false;
    if (reply === null) return;
    close();
    store.toast(reply.added === 1 ? `Added to ${playlist.name}.` : `Added ${reply.added} songs to ${playlist.name}.`);
  }

  async function create(event) {
    event.preventDefault();
    if (busy || !name.trim()) return;
    busy = true;
    const made = await store.playlistChange("api/playlists/create", { library, name }, namespace, { reply: true });
    busy = false;
    if (made !== null) await addTo(made);
  }

  async function startCreating() {
    creating = true;
    await tick();
    document.getElementById(`${id}-name`)?.focus();
  }

  /** Escape closes; arrow keys move between the items. Only for keys pressed inside this menu. */
  function keydown(event) {
    if (!root.contains(event.target)) return;
    if (event.key === "Escape") {
      event.preventDefault();
      close();
      return;
    }
    if ((event.key !== "ArrowDown" && event.key !== "ArrowUp") || !menu || event.target instanceof HTMLInputElement) return;
    const items = [...menu.querySelectorAll(".menu-item:not(:disabled)")];
    if (!items.length) return;
    event.preventDefault();
    const at = items.indexOf(document.activeElement);
    const next = event.key === "ArrowDown" ? (at + 1) % items.length : (at <= 0 ? items.length : at) - 1;
    items[next].focus();
  }

  $effect(() => {
    if (!open) return;
    const pointerdown = (event) => {
      if (!root.contains(event.target)) close(false);
    };
    document.addEventListener("pointerdown", pointerdown);
    document.addEventListener("keydown", keydown, true);
    return () => {
      document.removeEventListener("pointerdown", pointerdown);
      document.removeEventListener("keydown", keydown, true);
    };
  });

  $effect(() => {
    // A library change or reconnect closes the menu with its namespace.
    if (open && !current) close(false);
  });
</script>

<div class="add-to-playlist" bind:this={root}>
  <button
    class="icon-btn"
    bind:this={button}
    disabled={!current}
    aria-label="Add {title} to a playlist"
    aria-expanded={open}
    aria-controls={open ? `${id}-menu` : undefined}
    title="Add to playlist"
    onclick={toggle}
  ><Icon name="playlist-add" {size} /></button>
  {#if open}
    <div class="menu card" class:up id="{id}-menu" role="group" aria-label="Add {title} to a playlist" bind:this={menu}>
      {#if !canOwn}
        <p class="menu-note">{explanation}</p>
      {:else if loadError}
        <p class="menu-note error" role="alert">{loadError}</p>
        <button class="menu-item" onclick={load}>Try again</button>
      {:else if playlists === null}
        <p class="menu-note" role="status">Loading playlists…</p>
      {:else}
        {#each choices as playlist (playlist.id)}
          <button class="menu-item" disabled={busy} onclick={() => addTo(playlist)}>
            <Icon name={playlist.favorites ? "heart" : playlist.mine ? "playlist" : "shared"} size={18} />
            <span class="menu-label">{playlist.name}</span>
            {#if !playlist.mine}<span class="muted small">{playlist.owner_name}</span>{/if}
          </button>
        {:else}
          <p class="menu-note">Your playlists and shared ones show up here.</p>
        {/each}
        <hr />
        {#if creating}
          <form class="new" onsubmit={create}>
            <label class="visually-hidden" for="{id}-name">Name of the new playlist</label>
            <input id="{id}-name" type="text" maxlength="100" placeholder="Playlist name" autocomplete="off" bind:value={name} />
            <button class="btn primary" type="submit" disabled={busy || !name.trim()}>Create and add</button>
          </form>
        {:else}
          <button class="menu-item" disabled={busy} onclick={startCreating}><Icon name="add" size={18} /><span class="menu-label">New playlist…</span></button>
        {/if}
      {/if}
    </div>
  {/if}
</div>

<style>
  .add-to-playlist { position: relative; flex: none; }
  .menu { right: 0; top: calc(100% + 0.35rem); }
  .menu.up { top: auto; bottom: calc(100% + 0.35rem); left: 0; right: auto; }
  .new { display: grid; gap: 0.4rem; padding: 0.35rem; }
  .new input { width: 100%; }
</style>
