<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import Icon from "./Icon.svelte";

  const channels = $derived(store.snapshot?.channels ?? []);
  const players = $derived(store.snapshot?.players ?? []);
  let pending = $state({});

  async function attach(event, index) {
    const control = event.currentTarget;
    const player = Number(control.value);
    pending[index] = true;
    await store.attach(index, player);
    control.value = String(store.snapshot?.channels[index]?.player ?? 0);
    delete pending[index];
  }
</script>

<section aria-labelledby="channels-title">
  <h1 id="channels-title">Channels</h1>
  <p class="muted">Choose which queue each Mumble channel receives. Several channels can listen to the same queue.</p>
  <div class="channel-list">
    {#each channels as channel, i (i)}
      <article class="channel card">
        <div class="channel-heading"><Icon name="channel" /><h2>{channel.name}</h2><span class="badge connection" class:success={channel.connected} class:warning={!channel.connected}>{channel.connected ? "Connected" : "Disconnected"}</span></div>
        <p class="muted">{channel.server}{channel.channel ? ` · ${channel.channel}` : ""}</p>
        <p>{channel.listeners} {channel.listeners === 1 ? "listener" : "listeners"}</p>
        {#if channel.error}<p class="notice error" role="alert">{channel.error}</p>{/if}
        <label class="attachment"><span class="eyebrow">Playback queue</span>
          <select value={channel.player} disabled={!players.length || Boolean(pending[i])} onchange={(event) => attach(event, i)}>
            {#each players as player, index (index)}<option value={index}>{player.name || `Queue ${index + 1}`}</option>{/each}
          </select>
        </label>
      </article>
    {/each}
  </div>
  {#if !channels.length}<p class="notice muted">No Mumble channels are configured.</p>{/if}
</section>

<style>
  .channel-list { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 20rem), 1fr)); gap: 1rem; margin-top: 1.5rem; }
  .channel { padding: 1rem 1.1rem; }
  .channel-heading { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; color: var(--muted); }
  h2 { margin: 0; color: var(--fg); font-size: 1.05rem; overflow-wrap: anywhere; }
  .connection { margin-left: auto; }
  p { overflow-wrap: anywhere; }
  .attachment { display: grid; gap: 0.35rem; margin-top: 1rem; }
  .attachment select { width: 100%; }
</style>
