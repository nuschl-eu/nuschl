<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { tick } from "svelte";
  import { store } from "../store.svelte.js";
  import Icon from "./Icon.svelte";

  /** Rename, share and delete for the owner of a personal playlist (`playlist` from `api/playlist`). */
  let { playlist, namespace, current } = $props();
  const id = $props.id();
  let mode = $state(null);
  let name = $state("");
  let busy = $state(false);
  let renameButton = $state();
  let deleteButton = $state();
  const target = $derived({ library: playlist.library, id: playlist.id });

  async function change(path, body) {
    busy = true;
    const done = await store.playlistChange(path, { ...target, ...body }, namespace);
    busy = false;
    return done !== null;
  }

  async function startRename() {
    name = playlist.name;
    mode = "rename";
    await tick();
    document.getElementById(`${id}-name`)?.select();
  }

  async function finish(opener) {
    mode = null;
    await tick();
    opener?.focus();
  }

  async function rename(event) {
    event.preventDefault();
    if (busy || !name.trim()) return;
    if (await change("api/playlists/rename", { name })) await finish(renameButton);
  }

  async function remove() {
    if (busy) return;
    if (await change("api/playlists/delete", {})) {
      store.toast(`Deleted ${playlist.name}.`);
      store.navigate({ view: "playlists", library: playlist.library });
    }
  }

  function escape(event, opener) {
    if (event.key !== "Escape") return;
    event.preventDefault();
    finish(opener);
  }

  async function confirmDelete() {
    mode = "delete";
    await tick();
    document.getElementById(`${id}-keep`)?.focus();
  }
</script>

<div class="tools">
  <button class="btn" bind:this={renameButton} disabled={!current || busy} aria-expanded={mode === "rename"} onclick={startRename}><Icon name="edit" size={18} /> Rename</button>
  <button
    class="btn"
    disabled={!current || busy}
    aria-pressed={playlist.shared}
    title={playlist.shared ? "Everyone with a Mumble certificate adds and removes songs. Click to keep the songs to yourself." : "Let everyone with a Mumble certificate add and remove songs"}
    onclick={() => change("api/playlists/share", { shared: !playlist.shared })}
  ><Icon name="shared" size={18} /> {playlist.shared ? "Shared" : "Share"}</button>
  <button class="btn" bind:this={deleteButton} disabled={!current || busy} aria-expanded={mode === "delete"} onclick={confirmDelete}><Icon name="delete" size={18} /> Delete</button>
</div>
{#if mode === "rename"}
  <form class="inline-form" onsubmit={rename}>
    <label class="visually-hidden" for="{id}-name">New name of {playlist.name}</label>
    <input id="{id}-name" type="text" maxlength="100" autocomplete="off" bind:value={name} onkeydown={(event) => escape(event, renameButton)} />
    <button class="btn primary" type="submit" disabled={busy || !name.trim()}>Save</button>
    <button class="btn" type="button" onclick={() => finish(renameButton)}>Cancel</button>
  </form>
{:else if mode === "delete"}
  <div class="inline-form" role="group" aria-labelledby="{id}-delete">
    <p id="{id}-delete">Delete {playlist.name} from the library for everyone?</p>
    <button class="btn primary" disabled={busy} onclick={remove} onkeydown={(event) => escape(event, deleteButton)}>Delete</button>
    <button class="btn" id="{id}-keep" onclick={() => finish(deleteButton)} onkeydown={(event) => escape(event, deleteButton)}>Keep it</button>
  </div>
{/if}

<style>
  .tools { display: flex; flex-wrap: wrap; gap: 0.5rem; }
  .inline-form { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem; }
  .inline-form p { margin: 0; }
  .inline-form input { flex: 1; min-width: 12rem; }
</style>
