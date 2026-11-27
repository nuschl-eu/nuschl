<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount, tick } from "svelte";
  import { store } from "./store.svelte.js";
  import { postJSON } from "./api.js";
  import { cueListening, listening } from "./listening.svelte.js";
  import { buildRoute, handlesSpace, routeForLibrary } from "./logic.js";
  import Browse from "./components/Browse.svelte";
  import Channels from "./components/Channels.svelte";
  import QueuePanel from "./components/QueuePanel.svelte";
  import HistoryPanel from "./components/HistoryPanel.svelte";
  import DjPanel from "./components/DjPanel.svelte";
  import PlayerBar from "./components/PlayerBar.svelte";
  import BigScreen from "./components/BigScreen.svelte";
  import Icon from "./components/Icon.svelte";
  import AddMusic from "./components/AddMusic.svelte";
  import AddAlbum from "./components/AddAlbum.svelte";
  import Requests from "./components/Requests.svelte";
  import SearchField from "./components/SearchField.svelte";

  let bigScreen = $state(false);
  let bigScreenOpener = null;
  let confirmForget = $state(false);
  let forgetting = $state(false);
  let forgetOpener = $state();
  const route = $derived(store.route);
  const browseRouteKey = $derived(`${store.browseEpoch}:${store.browseKey}:${buildRoute(route)}`);
  const libraries = $derived(store.snapshot?.libraries ?? []);
  const links = $derived([
    { name: "Queue", icon: "queue", route: { view: "queue" }, views: ["queue"] },
    { name: "History", icon: "queue", route: { view: "history" }, views: ["history"] },
    { name: "DJ", icon: "decks", route: { view: "dj" }, views: ["dj"] },
    { name: "Albums", icon: "album", route: { view: "albums", library: store.library, list: "newest" }, views: ["albums", "album", "song"] },
    { name: "Artists", icon: "artist", route: { view: "artists", library: store.library }, views: ["artists", "artist"] },
    { name: "Genres", icon: "genre", route: { view: "genres", library: store.library }, views: ["genres", "genre"] },
    { name: "Playlists", icon: "playlist", route: { view: "playlists", library: store.library }, views: ["playlists", "playlist"] },
    { name: "Channels", icon: "channel", route: { view: "channels" }, views: ["channels"] },
    ...(store.snapshot?.import ? [{ name: "Add music", icon: "add", route: { view: "add", q: "" }, views: ["add", "addAlbum", "requests"] }] : []),
  ]);

  onMount(() => store.start());
  listening.attach();
  cueListening.attach();
  $effect(() => {
    if (store.invalid || !store.snapshot) bigScreen = false;
  });

  function changeLibrary(event) {
    if (store.browseKey === null) return;
    const library = Number(event.currentTarget.value);
    store.library = library;
    store.navigate(routeForLibrary(route, library));
  }

  function openBigScreen(event) {
    bigScreenOpener = event.currentTarget;
    bigScreen = true;
  }

  // After a link inside the overlay, focus goes to the new page; otherwise back to the opener.
  async function closeBigScreen(navigated) {
    if (!bigScreen) return;
    bigScreen = false;
    await tick();
    const target = navigated || !bigScreenOpener?.isConnected ? document.getElementById("main-content") : bigScreenOpener;
    bigScreenOpener = null;
    target?.focus();
  }

  function keydown(event) {
    if (event.code !== "Space" || event.repeat || event.altKey || event.ctrlKey || event.metaKey || handlesSpace(event.target) || store.invalid || !store.player) return;
    event.preventDefault();
    store.togglePause(store.selected);
  }

  // Clears the remembered link; the bot's root then asks for a new `!web` link.
  async function forgetBrowser() {
    if (forgetting) return;
    forgetting = true;
    try {
      await postJSON("api/forget", {});
      location.assign("./");
    } catch (error) {
      forgetting = false;
      store.toast(`Could not forget this browser: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function cancelForget() {
    confirmForget = false;
    await tick();
    forgetOpener?.focus();
  }

  function focusOnMount(node) {
    node.focus();
  }
</script>

<svelte:window onkeydown={keydown} />

<div class="app-shell" inert={bigScreen}>
  <a class="skip-link" href="#main-content" onclick={(event) => { event.preventDefault(); document.getElementById("main-content")?.focus(); }}>Skip to content</a>
  <header class="app-header">
    <div class="bar">
      <a class="brand" href={buildRoute({ view: "queue" })}><Icon name="note" size={22} /><span class="brand-name">nuschl musicbot</span></a>
      {#if !store.invalid}<SearchField />{/if}
      <div class="status-pills">
        {#if store.snapshot && !store.invalid}<span class="pill viewer"><Icon name="artist" size={15} />Listening as {store.snapshot.you}</span>{/if}
        <span class="pill live-status" class:online={store.connection === "open" && !store.invalid} role="status">{store.invalid ? "Link expired" : store.connection === "open" ? "Live" : store.connection === "reconnecting" ? "Reconnecting…" : "Connecting…"}</span>
      </div>
    </div>
    {#if !store.invalid}
      <div class="bar nav-bar">
        <nav aria-label="Music">
          {#each links as link (link.name)}
            <a href={buildRoute(link.route)} class:active={link.views.includes(route.view)} aria-current={link.views.includes(route.view) ? "page" : undefined}><Icon name={link.icon} size={18} /><span class="nav-label">{link.name}</span></a>
          {/each}
        </nav>
        <label class="library-choice"><span class="eyebrow">Library</span>
          <select value={store.library} disabled={store.browseKey === null || !libraries.length} onchange={changeLibrary}>
            {#each libraries as name, index (index)}<option value={index}>{name}</option>{/each}
          </select>
        </label>
      </div>
    {/if}
  </header>

  {#if store.invalid}
    <main id="main-content" class="invalid-link" tabindex="-1">
      <h1>This link is not valid (any more).</h1>
      <p>Send <code>!web</code> to the music bot in Mumble for a new one.</p>
    </main>
  {:else}
    {#if store.connection === "reconnecting"}
      <div class="connection-banner" role="status">Connection interrupted. Reconnecting automatically; displayed playback state may be out of date.</div>
    {/if}
    <div class="workspace">
      <main id="main-content" class="main-content" tabindex="-1">
        <div class="page">
          {#if !store.snapshot}
            <h1>Music</h1><p class="notice muted" role="status">Connecting to the music bot…</p>
          {:else if route.view === "queue"}
            <QueuePanel />
          {:else if route.view === "history"}
            <HistoryPanel />
          {:else if route.view === "dj"}
            <DjPanel />
          {:else if route.view === "channels"}
            <Channels />
          {:else if route.view === "add"}
            {#key browseRouteKey}<AddMusic {route} />{/key}
          {:else if route.view === "addAlbum"}
            {#key browseRouteKey}<AddAlbum {route} />{/key}
          {:else if route.view === "requests"}
            {#key browseRouteKey}<Requests />{/key}
          {:else}
            {#key browseRouteKey}<Browse {route} />{/key}
          {/if}
        </div>
        <footer class="app-notes">
          <p class="muted">Music plays in Mumble, or use the headphones button to hear it in this browser. Changes here are live for everyone sharing the queue.</p>
          <div class="note-links">
            <a href="licenses">Licences</a>
            {#if !confirmForget}<button class="link-btn" bind:this={forgetOpener} onclick={() => (confirmForget = true)}>Forget this browser</button>{/if}
          </div>
          {#if confirmForget}
            <div class="forget">
              <p class="muted" id="forget-explanation">This browser will drop its link to the bot. To come back, send <code>!web</code> again.</p>
              <div class="forget-actions">
                <button class="btn" aria-describedby="forget-explanation" disabled={forgetting} onclick={forgetBrowser}>Forget</button>
                <button class="btn" disabled={forgetting} use:focusOnMount onclick={cancelForget}>Cancel</button>
              </div>
            </div>
          {/if}
        </footer>
      </main>
      {#if store.snapshot && route.view !== "queue"}
        <aside class="queue-aside card" aria-label="Live queue"><QueuePanel compact /></aside>
      {/if}
    </div>
    {#if store.snapshot}<PlayerBar onexpand={openBigScreen} />{/if}
  {/if}
</div>

{#if bigScreen}<BigScreen onclose={closeBigScreen} />{/if}

<div class="toast-list" aria-live="polite" aria-atomic="false">
  {#each store.toasts as toast (toast.id)}
    <div class="toast card"><p>{toast.text}</p><button class="icon-btn" aria-label="Dismiss notification" onclick={() => store.dismiss(toast.id)}><Icon name="close" size={15} /></button></div>
  {/each}
</div>

<style>
  .app-shell { height: 100dvh; min-height: 0; display: flex; flex-direction: column; }
  .app-header { position: relative; z-index: 10; flex: none; background: color-mix(in srgb, var(--bg) 85%, transparent); backdrop-filter: blur(10px); border-bottom: 1px solid var(--border); }
  .bar { display: flex; align-items: center; gap: 1rem; max-width: 1600px; margin: 0 auto; padding: 0.6rem 1rem; }
  .nav-bar { padding-top: 0; }
  .brand { display: flex; flex: none; align-items: center; gap: 0.5rem; font-size: 1.1rem; font-weight: 700; white-space: nowrap; }
  .brand :global(svg) { color: var(--accent); }
  .status-pills { display: flex; align-items: center; gap: 0.5rem; min-width: 0; margin-left: auto; }
  .viewer { min-width: 0; max-width: 16rem; overflow: hidden; text-overflow: ellipsis; }
  .live-status { flex: none; color: var(--warning); background: color-mix(in srgb, var(--warning) 12%, transparent); border-color: transparent; font-weight: 600; }
  .live-status::before { content: ""; width: 0.45rem; height: 0.45rem; border-radius: 50%; background: currentColor; }
  .live-status.online { color: var(--accent); background: var(--accent-soft); }
  nav { display: flex; flex: 1; gap: 0.25rem; min-width: 0; overflow-x: auto; scrollbar-width: none; }
  nav a { display: flex; flex: none; align-items: center; gap: 0.45rem; padding: 0.4rem 0.85rem; border-radius: 999px; color: var(--muted); font-weight: 500; white-space: nowrap; }
  nav a:hover { background: var(--surface-2); color: var(--fg); }
  nav a.active { background: var(--accent-soft); color: var(--accent); }
  .library-choice { display: flex; flex: none; align-items: center; gap: 0.5rem; }
  .library-choice select { max-width: 12rem; padding-block: 0.35rem; border-radius: 999px; }
  .connection-banner { flex: none; padding: 0.5rem 1rem; text-align: center; color: var(--warning); background: color-mix(in srgb, var(--warning) 12%, transparent); font-size: 0.9rem; }
  .workspace { flex: 1; min-height: 0; display: flex; gap: 1rem; width: 100%; max-width: 1600px; margin: 0 auto; }
  .main-content { flex: 1; min-width: 0; overflow: auto; scroll-behavior: smooth; }
  .page { padding: 1.5rem 1rem 2rem; }
  .app-notes { margin: 0 1rem; padding: 1.25rem 0 2rem; border-top: 1px solid var(--border); font-size: 0.8rem; }
  .app-notes p { margin: 0 0 0.5rem; max-width: 40rem; }
  .note-links { display: flex; flex-wrap: wrap; gap: 1rem; }
  .forget { margin-top: 0.75rem; }
  .forget-actions { display: flex; gap: 0.5rem; }
  .queue-aside { width: 22rem; flex: none; overflow: auto; margin: 1rem 1rem 1rem 0; padding: 1rem; }
  .invalid-link { padding: 2rem 1rem; max-width: 50rem; margin: auto; }
  .toast-list { position: fixed; z-index: 30; right: 1rem; top: 7rem; width: min(26rem, calc(100vw - 2rem)); display: grid; gap: 0.5rem; pointer-events: none; }
  .toast { pointer-events: auto; display: flex; align-items: center; gap: 0.5rem; padding: 0.5rem 0.5rem 0.5rem 0.9rem; border-color: color-mix(in srgb, var(--warning) 40%, var(--border)); animation: toast-in 0.15s ease-out; }
  .toast p { margin: 0; flex: 1; overflow-wrap: anywhere; }
  @keyframes toast-in { from { opacity: 0; transform: translateY(-6px); } }
  @media (max-width: 1100px) { .queue-aside { display: none; } }
  @media (max-width: 900px) {
    .nav-label { position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
    nav a { padding: 0.45rem 0.6rem; }
  }
  @media (max-width: 760px) {
    .bar { gap: 0.6rem; padding: 0.5rem 0.75rem; }
    .nav-bar { padding-top: 0; }
    .brand-name { position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
    .viewer { display: none; }
    .library-choice .eyebrow { display: none; }
    .library-choice select { max-width: 8rem; }
    .page { padding: 1rem 0.75rem 1.5rem; }
    .app-notes { margin: 0 0.75rem; padding: 1rem 0 1.5rem; }
    .toast-list { top: 6rem; }
  }
  @media (max-width: 480px) {
    nav { gap: 0.1rem; }
    nav a { padding: 0.45rem 0.4rem; }
    .library-choice select { max-width: 6.5rem; }
  }
</style>
