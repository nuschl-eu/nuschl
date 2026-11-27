<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { getJSON, query } from "../api.js";
  import { debounce, suggestionQuery, suggestionStep } from "../logic.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";

  /** Most suggestions shown; the bot sends no more than this. */
  const MAX_ITEMS = 10;
  const KINDS = { artist: "Artist", album: "Album", song: "Song" };

  let text = $state("");
  let focused = $state(false);
  let open = $state(false);
  let active = $state(-1);
  /** The latest suggestions with the text and namespace they were asked for. */
  let shown = $state.raw({ q: null, items: [] });
  /** The request whose answer may still be shown; every other answer is dropped. */
  let request = null;
  const items = $derived(shown.items);
  const libraries = $derived(store.snapshot?.libraries ?? []);
  const detail = (item) => [KINDS[item.kind], item.subtitle, libraries.length > 1 ? libraries[item.library] : null].filter(Boolean).join(" · ");

  $effect(() => {
    const route = store.route;
    if (route.view === "search") text = route.q;
  });
  // A new page or library closes the list and drops what it showed.
  $effect(() => {
    store.route;
    store.browseKey;
    close();
  });
  $effect(() => () => {
    load.cancel();
    request?.abort();
  });
  $effect(() => {
    if (open && active >= 0) document.getElementById(`search-suggestion-${active}`)?.scrollIntoView({ block: "nearest" });
  });

  function close() {
    load.cancel();
    request?.abort();
    request = null;
    shown = { q: null, items: [] };
    open = false;
    active = -1;
  }

  const load = debounce((q) => {
    const namespace = store.browseKey;
    if (namespace === null || store.connection !== "open" || store.invalid) return;
    const controller = new AbortController();
    request = controller;
    getJSON(query("api/suggest", { q }), controller.signal).then(
      (data) => {
        if (request !== controller) return;
        request = null;
        if (namespace !== store.browseKey || suggestionQuery(text) !== q) return;
        const found = Array.isArray(data?.items) ? data.items.filter((item) => Object.hasOwn(KINDS, item?.kind)).slice(0, MAX_ITEMS) : [];
        shown = { q, items: found.map((item) => ({ ...item, namespace })) };
        active = -1;
        open = focused && found.length > 0;
      },
      // Busy or failing libraries leave the list empty.
      () => {
        if (request === controller) request = null;
      },
    );
  }, 200);

  function input(event) {
    request?.abort();
    request = null;
    active = -1;
    const q = suggestionQuery(event.currentTarget.value);
    if (q === null) {
      close();
    } else {
      open = shown.q === q && items.length > 0;
      if (shown.q !== q) load(q);
    }
  }

  function choose(item) {
    close();
    store.navigate({ view: item.kind, library: item.library, id: item.id, namespace: item.namespace });
  }

  function keydown(event) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (!items.length || shown.q !== suggestionQuery(text)) return;
      event.preventDefault();
      open = true;
      active = suggestionStep(active, event.key === "ArrowDown" ? 1 : -1, items.length);
    } else if (event.key === "Enter" && open && active >= 0) {
      event.preventDefault();
      choose(items[active]);
    } else if (event.key === "Escape" && open) {
      event.preventDefault();
      open = false;
      active = -1;
    }
  }

  function submit(event) {
    event.preventDefault();
    close();
    store.navigate({ view: "search", q: text.trim() });
  }

  function focus() {
    focused = true;
    open = items.length > 0 && shown.q === suggestionQuery(text);
  }

  function blur() {
    focused = false;
    open = false;
    active = -1;
  }
</script>

<form class="search-field" role="search" onsubmit={submit}>
  <label for="library-search" class="visually-hidden">Search all libraries</label>
  <input id="library-search" type="search" placeholder="Search music" autocomplete="off" bind:value={text}
         role="combobox" aria-autocomplete="list" aria-controls="search-suggestions" aria-expanded={open}
         aria-activedescendant={open && active >= 0 ? `search-suggestion-${active}` : undefined}
         oninput={input} onkeydown={keydown} onfocus={focus} onblur={blur} />
  <button class="icon-btn" type="submit" aria-label="Search"><Icon name="search" size={18} /></button>
  <ul id="search-suggestions" class="suggestions card" role="listbox" aria-label="Suggestions" hidden={!open}>
    {#each items as item, index (`${item.library}:${item.kind}:${item.id}`)}
      <li id="search-suggestion-{index}" role="option" tabindex="-1" aria-selected={index === active} class:active={index === active}
          onmousedown={(event) => { event.preventDefault(); if (event.button === 0) choose(item); }}>
        <Cover library={item.library} art={item.cover_art} size={64} px={36} round={item.kind === "artist"} />
        <span class="text">
          <span class="title">{item.title}</span>
          <span class="sub muted">{detail(item)}</span>
        </span>
      </li>
    {/each}
  </ul>
</form>

<style>
  .search-field { position: relative; display: flex; flex: 1; align-items: center; gap: 0.4rem; max-width: 30rem; padding: 0 0.2rem 0 1rem; border: 1px solid var(--border); border-radius: 999px; background: var(--surface); color: var(--muted); }
  .search-field:focus-within { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
  .search-field input[type="search"] { flex: 1; width: 100%; padding: 0.45rem 0.2rem; border: none; border-radius: 0; background: transparent; box-shadow: none; }
  .search-field .icon-btn { width: 1.9rem; height: 1.9rem; }
  .suggestions { position: absolute; z-index: 30; top: calc(100% + 0.4rem); left: 0; right: 0; display: grid; gap: 0.1rem; max-height: min(34rem, calc(100dvh - 6rem)); overflow-y: auto; margin: 0; padding: 0.35rem; list-style: none; color: var(--fg); }
  .suggestions[hidden] { display: none; }
  li { display: flex; align-items: center; gap: 0.65rem; padding: 0.35rem 0.5rem; border-radius: var(--radius); cursor: pointer; }
  li:hover, li.active { background: var(--surface-2); }
  li.active { box-shadow: inset 3px 0 0 var(--accent); }
  .text { display: grid; min-width: 0; }
  .title, .sub { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .title { font-weight: 500; }
  .sub { font-size: 0.82rem; }
</style>
