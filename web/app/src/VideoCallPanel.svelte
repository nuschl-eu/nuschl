<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount } from "svelte";
  import { Controls } from "../../call/controls.js";
  import { View } from "../../call/view.js";
  import "../../call/call.css";

  let { call, owner, url, minimized, ontoggle, onleave } = $props();
  let root = $state(), invitation = $state(null), ended = $state(false), copyStatus = $state("");

  onMount(() => {
    const changed = () => {
      invitation = call.opened?.invitation ?? url ?? null;
      ended = Boolean(call.ended);
    };
    changed();
    const view = new View(root, call, null);
    const controls = new Controls(root, call, false);
    call.addEventListener("change", changed);
    return () => {
      call.removeEventListener("change", changed);
      controls.close();
      view.close();
    };
  });

  async function copy() {
    try { await navigator.clipboard.writeText(invitation); copyStatus = "Link copied"; }
    catch { copyStatus = "Could not copy link"; }
  }
</script>

<section class="video-panel" aria-label="Video call">
  <header class="video-panel-toolbar">
    <strong>Video call</strong>
    <button type="button" onclick={ontoggle} aria-expanded={!minimized}>{minimized ? "Show video" : "Minimize"}</button>
    {#if invitation && !ended}<button type="button" onclick={copy}>Copy link</button>{/if}
    {#if !minimized && !ended && root?.requestFullscreen}<button type="button" onclick={() => root.requestFullscreen().catch(() => {})}>Fullscreen</button>{/if}
    <button type="button" class="leave" onclick={onleave}>{ended ? "Close" : owner ? "End call" : "Leave video"}</button>
    {#if copyStatus}<span role="status">{copyStatus}</span>{/if}
  </header>
  <div class="video-session" bind:this={root} hidden={minimized}>
    <div id="call">
      <header><span id="status" role="status">Connecting</span></header>
      <section id="stage" aria-label="Video">
        <div id="large" hidden></div>
        <div id="grid"></div>
        <div id="shared-audio" hidden></div>
        <p id="empty">No video yet</p>
      </section>
      <aside aria-labelledby="people">
        <h2 id="people">People <span id="count"></span></h2>
        <ul id="roster"></ul>
      </aside>
      <footer>
        <div id="controls">
          <button type="button" id="camera" aria-pressed="false" disabled>Start camera</button>
          <select id="camera-prefer" aria-label="Camera mode"></select>
          <label><input type="checkbox" id="camera-latency"> Low latency</label>
          <button type="button" id="screen" aria-pressed="false" disabled>Share screen</button>
          <select id="screen-prefer" aria-label="Screen mode"></select>
          <label><input type="checkbox" id="screen-latency"> Low latency</label>
          <label hidden><input type="checkbox" id="screen-audio"> Share browser tab audio</label>
          <details id="advanced">
            <summary>Advanced</summary>
            <label>Camera limit <select id="camera-quality"></select></label>
            <label>Screen limit <select id="screen-quality"></select></label>
          </details>
          <span id="notice" role="status" hidden></span>
        </div>
      </footer>
    </div>
    <section id="ending" role="alert" hidden></section>
  </div>
</section>

<style>
  .video-panel { flex: none; min-width: 0; background: var(--surface); border-bottom: 1px solid var(--border); }
  .video-panel-toolbar { display: flex; align-items: center; flex-wrap: wrap; gap: 4px; padding: 4px 12px; }
  .video-panel-toolbar strong { margin-right: auto; }
  .video-panel-toolbar button { min-height: var(--control-target); font-size: .8rem; }
  .video-panel-toolbar .leave { color: var(--bad); }
  .video-panel-toolbar span { color: var(--muted); font-size: .8rem; }
  .video-session { height: min(44dvh, 28rem); min-height: 6rem; overflow: auto; background: var(--background); }
  .video-session:fullscreen { height: 100%; width: 100%; }
  .video-session :global(#call) { grid-template-columns: minmax(0, 1fr) minmax(7rem, 12rem); }
  .video-session :global(#call > footer) { flex-wrap: wrap; }
  .video-session :global(#call > footer button) { min-height: var(--control-target); }
  @media (max-width: 760px) {
    .video-session :global(#call) { grid-template: "header" auto "stage" minmax(6rem, 1fr) "footer" auto / minmax(0, 1fr); }
    .video-session :global(#call > aside) { display: none; }
  }
</style>
