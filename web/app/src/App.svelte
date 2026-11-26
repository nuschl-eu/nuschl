<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount, tick } from "svelte";
  import {actionProof,serverReference} from "./action-context.js";
  import { ContentBridge } from "./content-bridge.js";
  import { flip } from "svelte/animate";
  import { cubicOut } from "svelte/easing";
  import { startSwipe, moveSwipe, endSwipe, swipeExempt } from "./swipe.js";
  import { serverIndices, moveServer, sameOrder, edgeSpeed } from "./server-order.js";
  import { inviteFragment } from "./invite.js";
  import { channelRows, initials, avatarColor, conversationKey, filterMessages, appearance } from "./presentation.js";
  import {observeMessageTimeLocale} from "./message-time.js";
  import Message from "./Message.svelte";
  import VideoCallPanel from "./VideoCallPanel.svelte";
  import { configuredCallHost, configuredCallLink, createVideoCall } from "./video-call.js";
  import TypedDocument from "./TypedDocument.svelte";
  import EmojiPicker from "./EmojiPicker.svelte";
  import { loadEmoji } from "./emoji-state.svelte.js";
  import { insertAt } from "./emoji.js";
  import { welcomeDocuments } from "./welcome.js";
  import { SdkRpc, SdkUnavailableError } from "./sdk-rpc.js";
  import { BrowserAudio } from "./audio.js";
  import { CallSession } from "./call-session.js";
  import { ErrorLifetime } from "./error-lifetime.js";
  const link = inviteFragment(location.hash);
  let relay = $state(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/relay`);
  let host = $state(link.host), port = $state(link.port), username = $state(link.username), pin = $state(link.pin), serverName = $state(""), password = $state("");
  let loaded = $state(false), clientUnavailable = $state(false), error = $state(link.error), servers = $state([]), timeline = $state(null);
  const errorLifetime = new ErrorLifetime();
  function setError(message, conversation = null) {
    errorLifetime.replace(conversation);
    error = message;
  }
  let showAdd = $state(false), editingIndex = $state(null), showSettings = $state(false), drawer = $state(false), mobile = $state(false);
  let body = $state(""), expanded = $state(new Map()), search = $state(""), showSearch = $state(false);
  let emojiOpen = $state(false), emojiRecent = $state([]);
  let messagesElement = $state(), composerElement = $state(), navigationButton = $state(), searchElement = $state();
  let settings = $state(appearance(null)), voice = $state(null), audioState = $state(null), audioActionPending = $state(null), capabilities = $state([]);
  const videoHost = configuredCallHost();
  let video = $state.raw(null), videoMinimized = $state(false);
  // The requested voice bitrate (voice-bitrate.js); the SDK worker resolves it per server.
  import { VOICE_BITRATES, initialVoiceBitrate, storeVoiceBitrate, kbps } from "./voice-bitrate.js";
  let voiceBitrate = $state(initialVoiceBitrate()), sentVoiceBitrate = null;
  $effect(() => {
    // Once the SDK is ready, and after each choice; again after a restart.
    if (!loaded) { sentVoiceBitrate = null; return; }
    if (sentVoiceBitrate === voiceBitrate) return;
    sentVoiceBitrate = voiceBitrate;
    void act("set_voice_bitrate", voiceBitrate);
  });
  let serverScroll = $state(), channelScroll = $state(), serverUnread = $state({above:false,below:false}), channelUnread = $state({above:false,below:false});
  let welcomes = $state(new Map());
  const welcome = $derived(selected?.status === "connected" && selected.welcome_token ? welcomes.get(selected.id) : null);
  let membersOpen = $state(false), selectedFallback = $state(null);
  let arrangeServers = $state(false), orderPending = $state(false), previewIds = $state(null), draggingId = $state(null), pointerGhost = $state(null), orderStatus = $state(""), reducedMotion = $state(false);
  let orderDrag = null, orderFrame = 0, orderTime = 0, suppressBrowseUntil = 0;
  // Moving someone to another channel (Mumble's Move permission, 0x20).
  let userMove = $state(null), moveGhost = $state(null), moveChooser = $state(null), moveStatus = $state("");
  let moveFrame = 0, moveTime = 0, suppressDirectUntil = 0;
  const may = (server, channel, bits) => ((server?.permissions?.[channel] ?? 0) & bits) === bits;
  const mayMove = (server, user, channel) => !!user && user.session !== server.own_session && user.channel !== channel && may(server, user.channel, 0x20) && may(server, channel, 0x20);
  const railServers = $derived.by(() => {
    if (!previewIds) return servers;
    const byId = new Map(servers.map(server => [server.id, server]));
    return previewIds.map(id => byId.get(id)).filter(Boolean);
  });
  let lastChannelPointer = "";
  let initialState = true, follow = true, worker, rpc, audio, activateAudio = () => {}, disposeAudio = () => {};
  const drafts = new Map();
  let editingReference=null;
  const selected = $derived(servers.find((server) => server.id === timeline?.server) || servers.find(server => server.id === selectedFallback) || servers[0]);
  const selectedIndex = $derived(selected?.runtime_index ?? -1);
  const voiceServer = $derived(servers.find(server => server.id === voice?.server_id));
  const voiceIndex = $derived(voiceServer?.runtime_index ?? -1);
  const voiceChannel = $derived(voiceServer?.channels.find(channel => channel.id === voice?.channel_id));
  const microphoneMuted = $derived(voiceServer?.muted !== false || voiceServer?.deafened === true);
  const playbackDeafened = $derived(voiceServer?.deafened !== false);
  // The notification's Mute and Hang up; neither ever unmutes. When the browser
  // freezes the page, closing the SDK worker closes its connections at once.
  const callSession = new CallSession({onMute:() => { if (!microphoneMuted) toggleMicrophone(); }, onHangUp:() => stopAudio(),
    onEnd:() => { audio?.playDisconnected(); rpc?.close("Voice ended in the background. Reload to reconnect."); }});
  $effect(() => callSession.update(voice ? {channel:voiceChannel?.name || "Voice channel", server:voiceServer?.name || "", muted:microphoneMuted} : null));
  const voiceStatus = $derived(!voice?.joined ? "Joining…" : voice.deafened ? "Sound off"
    : microphoneMuted ? (audioState?.phase === "stopped" ? "Microphone stopped" : "Microphone muted") : audioState?.phase === "permission-pending" ? "Waiting for microphone permission"
    : audioState?.phase === "denied" ? "Microphone permission denied" : audioState?.phase === "unavailable" ? "Audio unavailable"
    : audioState?.microphone && voice.can_transmit ? "Microphone on" : voice.muted ? "Microphone blocked" : "Microphone off");
  const rows = $derived(selected ? channelRows(selected.channels, expanded, selected.id, selected.users) : []);
  const currentChannel = $derived(selected?.channels.find((channel) => channel.id === timeline?.channel));
  const ownUser = $derived(selected?.users.find((user) => user.session === selected.own_session));
  const canStartVideo = $derived(videoHost && capabilities.includes("video_call") && selected?.status === "connected" &&
    timeline && (timeline.peer
      ? selected.users.some(user => user.session === timeline.peer.session && user.instance === timeline.peer.instance)
      : ownUser?.channel === timeline.channel));
  const unmatchedVideoCalls = $derived.by(() => {
    if (!timeline?.video_calls?.length) return [];
    const matched = new Set(timeline.messages.filter(message => message.call && !message.call.ended)
      .map(message => message.call.link));
    return timeline.video_calls.filter(call => configuredCallLink(call.link, videoHost) && !matched.has(call.link));
  });
  // Current save availability, then this connection's rows that were not saved.
  const historyHealth = $derived.by(() => {
    const history = selected?.history;
    if (!history || !/^[0-9]{1,20}$/.test(history.not_saved)) return "";
    const current = history.saving === "available" ? "Local history is saving new messages again."
      : history.saving === "busy" ? "Local history is busy; new messages may not be saved."
      : history.saving === "failing" ? "Local history cannot save new messages in this browser." : "Local history may not save new messages.";
    return `${current} ${history.not_saved} ${history.not_saved === "1" ? "message" : "messages"} on this connection ${history.not_saved === "1" ? "was" : "were"} not saved.`;
  });
  import { voiceProcessing } from "./voice-processing.js";
  const processingRows = $derived(voiceProcessing(audioState?.processing));
  const selectedVoiceChannel = $derived(selected?.channels.find((channel) => channel.id === ownUser?.channel));
  const displayedMessages = $derived(filterMessages(timeline?.messages || [], search));
  const selectedUsers = $derived((selected?.users || []).filter((user) => user.channel === timeline?.channel));
  const canSend = $derived(loaded && !!timeline && selected?.status === "connected" && !timeline.sending);
  const usersByChannel = $derived.by(() => {
    const groups = new Map();
    const moved = userMove?.server === selected?.id && userMove.target != null ? userMove : null;
    for (const user of selected?.users || []) { const channel = moved?.session === user.session ? moved.target : user.channel; const group = groups.get(channel) || []; group.push(user); groups.set(channel, group); }
    return groups;
  });
  // One keyed list, so a person moved between channels slides to their place.
  const treeEntries = $derived(rows.flatMap(row => [{key:`c${row.id}`,row}, ...(usersByChannel.get(row.id) || []).map(user => ({key:`u${user.session}`,row,user}))]));
  // Whether people may be dragged: our permissions where they are.
  $effect(() => { if (selected?.status === "connected") askMovePermissions([...new Set(selected.users.map(user => user.channel))].map(id => ({id}))); });
  // A move ends when the server shows it, refuses it (a new notice), or the person leaves.
  $effect(() => {
    if (!userMove) return;
    const server = servers.find(server => server.id === userMove.server), user = server?.users.find(user => user.session === userMove.session);
    if (!user || user.channel !== userMove.source || (userMove.sent && server.notice !== userMove.notice)) cancelMove();
  });
  const labels = { connected:"Connected", connecting:"Connecting…", disconnected:"Disconnected", unavailable:"Unavailable" };
  const icons = {
    menu:"M4 6h16M4 12h16M4 18h16", close:"m6 6 12 12M6 18 18 6", plus:"M12 5v14M5 12h14", hash:"M10 3 8 21M17 3l-2 18M4 9h17M3 15h17",
    voice:"M11 5 6 9H3v6h3l5 4V5Zm4 3a6 6 0 0 1 0 8m3-11a10 10 0 0 1 0 14", mic:"M12 15a3 3 0 0 0 3-3V5a3 3 0 0 0-6 0v7a3 3 0 0 0 3 3Zm-6-4v1a6 6 0 0 0 12 0v-1M12 18v4M8 22h8",
    video:"M3 6h12v12H3zM15 10l6-3v10l-6-3",
    smile:"M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM8.5 14.5a4.5 4.5 0 0 0 7 0M9 9.5h.01M15 9.5h.01",
    headphones:"M4 14v-3a8 8 0 0 1 16 0v3M4 13h4v8H4v-8Zm12 0h4v8h-4v-8", search:"m21 21-5-5M10 18a8 8 0 1 0 0-16 8 8 0 0 0 0 16", user:"M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2M12 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8Z", people:"M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M16 3a4 4 0 0 1 0 8m6 10v-2a4 4 0 0 0-3-3.87M13 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0",
    settings:"m9 3-.7 3-2.8 1-2.5-1.5-2 3.5 2.3 2V14L1 16l2 3.5L5.5 18l2.8 1L9 22h4l.7-3 2.8-1 2.5 1.5 2-3.5-2.3-2v-3L21 9l-2-3.5L16.5 7l-2.8-1L13 3H9ZM15 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0", send:"m22 2-7 20-4-9-9-4 20-7ZM11 13 22 2", leave:"M10 17l5-5-5-5M15 12H3M15 3h5v18h-5", lock:"M5 10h14v11H5V10Zm3 0V6a4 4 0 0 1 8 0", unlock:"M5 10h14v11H5V10Zm3 0V6a4 4 0 0 1 7-3", chevron:"m9 5 7 7-7 7",
  };
  function commandFrom(reference,method, ...args) {
    if (!worker || !rpc || rpc.closed) return Promise.reject(clientUnavailable
      ? new SdkUnavailableError() : new Error("The client is still starting."));
    let proof;try{proof=actionProof(method,args,servers,timeline,voice,reference);}catch(reason){return Promise.reject(reason);}
    if (["connect","connect_with_password","disconnect","remove_server","edit_server"].includes(method) && servers.find(server => server.runtime_index === args[0])?.id === voice?.server_id) {
      audio?.syncVoice(null); voice = null; audioActionPending = null;
    }
    return rpc.request(method,args,proof);
  }
  function command(method,...args){return commandFrom(undefined,method,...args);}
  async function act(method, ...args) { try { return await command(method, ...args); } catch (reason) { setError(String(reason?.message || reason)); } }

  function openVideo(link) {
    const creating = link === undefined;
    if (!(creating ? canStartVideo : configuredCallLink(link, videoHost) && selected?.status === "connected")) return;
    if (video?.link === link && video && !video.call.ended) {
      videoMinimized = false;
      return;
    }
    let proof;
    try {
      if (creating) proof = actionProof("start_video_call", [""], servers, timeline, voice);
    } catch (reason) {
      setError(String(reason?.message || reason));
      return;
    }
    closeVideo();
    let call;
    try { call = createVideoCall(new URLSearchParams(location.search).has("relay")); }
    catch { setError("Video call could not start."); return; }
    const active = {
      call, link, creating, server: selected.id, generation: selected.generation,
      channel: ownUser?.channel, joined: false, announced: false,
    };
    video = active;
    videoMinimized = false;
    active.changed = () => {
      if (video !== active) return;
      if (call.ended) {
        if (active.link && active.creating) void command("end_video_call", active.link).catch(() => {});
        call.removeEventListener("change", active.changed);
        return;
      }
      if (!creating || active.joined || call.role !== "host") return;
      active.joined = true;
      active.link = call.opened.invitation;
      if (!configuredCallLink(active.link, videoHost)) return call.end("link");
      rpc.request("start_video_call", [active.link], proof).catch(reason => {
        if (video !== active) return;
        setError(String(reason?.message || reason));
        call.endForEveryone();
        call.end("ended");
      });
    };
    call.addEventListener("change", active.changed);
    if (!call.ended) void call.start({ link, host: videoHost });
  }

  function closeVideo() {
    const active = video;
    if (!active) return;
    video = null;
    active.call.removeEventListener("change", active.changed);
    if (active.creating) {
      active.call.endForEveryone();
      if (active.link) void command("end_video_call", active.link).catch(() => {});
    }
    active.call.end("ended");
  }

  $effect(() => {
    const active = video;
    if (!active) return;
    const hosted = timeline?.hosted_video_call;
    const server = servers.find(item => item.id === active.server);
    const own = server?.users.find(user => user.session === server.own_session);
    if (server?.status !== "connected" || server.generation !== active.generation || own?.channel !== active.channel) {
      closeVideo();
    } else if (active.creating && active.link && hosted === active.link) {
      active.announced = true;
    } else if (active.announced && timeline && hosted !== active.link) {
      closeVideo();
    }
  });
  function rememberDraft() {
    const key = conversationKey(timeline, selected);
    if (!key) return;
    if (body) { drafts.delete(key); drafts.set(key, body.slice(0, 65536)); } else drafts.delete(key);
    let total = 0;
    for (const value of drafts.values()) total += value.length;
    while (drafts.size > 30 || total > 131072) { const first = drafts.keys().next().value; total -= drafts.get(first).length; drafts.delete(first); }
  }
  // Inserts at the textarea's cursor (replacing a selection); the picker stays
  // open with Shift held. The composer stays a plain textarea: emoji show as
  // the browser's own glyphs while typing, and as Twemoji in messages.
  function insertEmoji(text, item, keepOpen) {
    const element = composerElement;
    if (!element || element.disabled || element.readOnly) return;
    const { value, cursor } = insertAt(body, element.selectionStart, element.selectionEnd, text);
    if (value.length > 65536) return;
    body = value;
    rememberDraft();
    emojiRecent = [item, ...emojiRecent.filter((recent) => recent !== item)].slice(0, 18);
    tick().then(() => {
      element.setSelectionRange(cursor, cursor);
      if (!keepOpen) { emojiOpen = false; element.focus(); }
    });
  }
  function closeEmoji() { emojiOpen = false; composerElement?.focus(); }
  function persistSettings() {
    settings = appearance(settings);
    try { localStorage.setItem("nuschl.appearance.v1", JSON.stringify(settings)); } catch { setError("Appearance preferences could not be saved in this browser."); }
  }
  function chooseVoiceBitrate() {
    let stored = false;
    try { stored = storeVoiceBitrate(localStorage, voiceBitrate); } catch { /* reported below */ }
    if (!stored) setError("The voice bitrate could not be saved in this browser; it applies until you reload.");
  }
  $effect(() => {
    document.documentElement.dataset.theme = settings.theme;
    document.documentElement.dataset.density = settings.density;
    document.documentElement.style.setProperty("--font-size", `${settings.fontSize}px`);
  });
  onMount(() => {
    loadEmoji();
    const stopTimeLocale=observeMessageTimeLocale(window);
    let contentBridge;
    try { settings = appearance(JSON.parse(localStorage.getItem("nuschl.appearance.v1") || "null")); } catch { /* Defaults remain usable in private browsing. */ }
    const media = matchMedia("(max-width: 760px)");
    const updateMobile = () => { mobile = media.matches; if (!mobile) drawer = false; };
    updateMobile(); media.addEventListener("change", updateMobile);
    const motion = matchMedia("(prefers-reduced-motion: reduce)");
    const updateMotion = () => { reducedMotion = motion.matches; };
    updateMotion(); motion.addEventListener("change", updateMotion);
    const escapeOrder = event => {
      if (event.key === "Escape" && (orderDrag || arrangeServers)) {
        cancelOrder(); arrangeServers = false; event.preventDefault(); event.stopPropagation();
      } else if (event.key === "Escape" && userMove && !userMove.sent) {
        cancelMove(); event.preventDefault(); event.stopPropagation();
      }
    };
    window.addEventListener("keydown", escapeOrder, true);
    window.addEventListener("resize", cancelOrder);
    const viewport = window.visualViewport;
    const resizeViewport = () => { if (orderDrag) cancelOrder(); document.documentElement.style.setProperty("--app-height", `${viewport?.height || window.innerHeight}px`); };
    resizeViewport(); viewport?.addEventListener("resize", resizeViewport);
    callSession.attach();
    window.addEventListener("pagehide", closeVideo);
    const cleanup = () => { closeVideo(); window.removeEventListener("pagehide", closeVideo); callSession.detach(); stopTimeLocale(); contentBridge?.dispose(); cancelOrder(); window.removeEventListener("keydown", escapeOrder, true); window.removeEventListener("resize", cancelOrder); motion.removeEventListener("change", updateMotion); media.removeEventListener("change", updateMobile); viewport?.removeEventListener("resize", resizeViewport); rpc?.close("Client closed.",{notify:false}); worker = null; disposeAudio(); };
    const unavailable = reason => {
      contentBridge?.dispose();
      worker = null; clientUnavailable = true; loaded = false; cancelOrder(); arrangeServers = false;
      // Keep conversation text, unsent drafts, bookmarks and history. Only
      // transport-derived presence is invalid once its owner is retired.
      audio?.syncVoice(null); voice = null; audioActionPending = null;
      servers = servers.map(server => ({...server,status:"unavailable",muted:true,deafened:true,
        users:server.users.map(user => ({...user,talking:false}))}));
      if (timeline?.sending) timeline = {...timeline,sending:false};
      setError(reason);
    };
    try { worker = new Worker(new URL("./sdk.worker.js", import.meta.url), { type:"module" }); }
    catch { unavailable("The browser client could not be created. Reload to try again."); return cleanup; }
    audio = new BrowserAudio({
      onPacket: packet => command("voice_packet",packet),
      onReady: ready => command("audio_ready",ready),
      onState: state => { audioState = state; },
      onStopped: () => { if (!microphoneMuted) toggleMicrophone(); },
    });
    const ownedWorker = worker;
    rpc = new SdkRpc(ownedWorker,{onClose:reason => {
      if (worker === ownedWorker) unavailable(reason);
    }});
    contentBridge = new ContentBridge((method,...args) => command(method,...args));
    activateAudio = options => audio.activateGesture(options);
    disposeAudio = () => audio.dispose();
    ownedWorker.onmessage = async ({ data }) => {
      if (worker !== ownedWorker || rpc.closed || !data || typeof data !== "object") return;
      if (data.kind !== "ready" && data.kind !== "error" && data.kind !== "fatal" && rpc.phase !== "ready") return;
      if(data.kind === "fatal"){rpc.close(String(data.message||"The browser client stopped. Reload to reconnect."));return;}
      if (contentBridge.handle(data)) return;
      if (data.kind === "voice_packet") {
        // Audio packets never replace reactive chat/server state.
        audio.receive(data.packet); return;
      }
      if (data.kind === "result") {
        rpc.accept(data);
      } else if (data.kind === "error") setError(data.message);
      else if (data.kind === "ready") {
        if (!rpc.acceptReady()) return;
        loaded = true; capabilities = data.capabilities || [];
        if (link.host && link.username && !link.error) await connect();
      } else if (data.kind === "state") {
        if(!rpc.acceptState(data.publication))return;
        try {
        const prior = timeline, priorKey = conversationKey(timeline, selected);
        rememberDraft();
        selectedFallback = selected?.id || null;
        try { servers = serverIndices(data.servers, capabilities.includes("server_order")); }
        catch (reason) { audio.setLocalPolicy({muted:true,deafened:true}); audio.syncVoice(null); voice = null; loaded = false; rpc.close("The server list could not be read safely. Reload to reconnect."); return; }
        contentBridge.state(servers,data.updates);
        welcomes = welcomeDocuments(welcomes, servers, data.updates);
        if (orderDrag && !sameOrder(orderDrag.baseline, servers.map(server => server.id))) cancelOrder();
        timeline = data.timeline;
        voice = data.voice || null;
        audio.syncVoice(voice);
        if (audioActionPending) {
          const state=servers.find(server=>server.id===audioActionPending.server);
          if (!voice || !state || (state.muted===audioActionPending.muted && state.deafened===audioActionPending.deafened)) audioActionPending=null;
        }
        for (const update of data.updates || []) {
          if (update.kind === "error") setError(update.message);
          if (update.kind === "sent") {
            const target = servers.find(server => server.id === update.server);
            const key = conversationKey({server:update.server,channel:update.channel},target);
            if (drafts.get(key) === update.body) drafts.delete(key);
            if (key === conversationKey(timeline,selected) && body === update.body) body = "";
          }
        }
        if (initialState) { initialState = false; if (!servers.length && !link.host) showAdd = true; }
        const changed = priorKey !== conversationKey(timeline, selected);
        if (changed) {
          body = drafts.get(conversationKey(timeline, selected)) || ""; search = ""; follow = true;
          if (messagesElement) delete messagesElement.dataset.previousHeight;
        }
        await tick();
        if (worker !== ownedWorker || rpc.closed) return;
        if (messagesElement?.dataset.previousHeight && prior?.loading && !timeline?.loading) {
          const difference = messagesElement.scrollHeight - Number(messagesElement.dataset.previousHeight);
          messagesElement.scrollTop = Number(messagesElement.dataset.previousTop || 0) + difference;
          delete messagesElement.dataset.previousHeight; delete messagesElement.dataset.previousTop;
        } else if (messagesElement && follow && !search) messagesElement.scrollTop = messagesElement.scrollHeight;
        updateUnread();
        rpc.ackState(data.publication);
        } catch { rpc.close("The browser state could not be applied safely. Reload to reconnect."); }
      }
    };
    ownedWorker.onerror = event => {
      if (worker !== ownedWorker) return;
      event.preventDefault(); rpc.close("The browser worker stopped. Reload to reconnect.");
    };
    ownedWorker.onmessageerror = () => {
      if (worker === ownedWorker) rpc.close("The browser worker sent an unreadable response. Reload to reconnect.");
    };
    // Saved servers connect on load; a server link connects only its server.
    rpc.initialize(relay, !link.host);
    return cleanup;

  });
  async function connect(event) {
    event?.preventDefault();
    if (link.error) { setError(link.error); return; }
    try {
      setError("");
      const fields = [serverName.trim(),host.trim(),Number(port),username.trim(),pin.trim() || undefined];
      const created = editingIndex === null ? await command("add_server",...fields) : await commandFrom(editingReference,"edit_server",editingIndex,...fields);
      const {index,reference}=created;
      if (password) await commandFrom(reference,"connect_with_password",index,password); else await commandFrom(reference,"connect", index);
      password = ""; await commandFrom(reference,"browse_server",index); showAdd = false;
    } catch (reason) { setError(String(reason?.message || reason)); }
  }
  function addServer() { editingIndex = null; editingReference=null; password = ""; serverName = ""; host = ""; port = 64738; username = selected?.username || ""; pin = ""; showAdd = true; drawer = false; }
  function editServer() { if (!selected) return; try{editingReference=serverReference(selected);}catch(reason){setError(String(reason?.message||reason));return;} editingIndex = selectedIndex; password = ""; serverName = selected.name; host = selected.host; port = selected.port; username = selected.username; pin = selected.pin || ""; showAdd = true; drawer = false; }
  async function send(event) {
    event?.preventDefault();
    const sent = body;
    if (!sent.trim() || !canSend) return;
    // Shared SDK Sent identifies both the original conversation and body.
    // A command being queued never clears text typed during an earlier send.
    const conversation = conversationKey(timeline, selected);
    const previousError = errorLifetime.capture(conversation);
    try {
      await command("send_text", sent);
      if (errorLifetime.accepts(previousError)) setError("");
    } catch (reason) { setError(String(reason?.message || reason), conversation); }
  }
  async function openChannel(id) { rememberDraft(); await act("open", selectedIndex, id); if (mobile) drawer = false; }
  function joinVoice(channel = timeline?.channel, server = selected) {
    if (!server || server.status !== "connected" || channel == null) return;
    // Revoke the old destination before touching an asynchronous command.
    audio?.syncVoice(null); voice = null;
    void activateAudio({microphone:false}); callSession.prime(); void act("join", server.runtime_index, channel);
  }
  // Ctrl+K (Cmd+K on macOS): the quick switcher; the SDK lists and ranks its
  // results as on the desktop. Ctrl+F: search this conversation's messages.
  const mac = /Mac|iPhone|iPad/.test(navigator.platform), primary = mac ? "Cmd" : "Ctrl";
  let finder = $state(null), finding = Promise.resolve(), findRequest = 0;
  function shortcut(event) {
    if (!(mac ? event.metaKey : event.ctrlKey) || event.altKey || event.shiftKey) return;
    const key = event.key.toLowerCase(), dialog = document.querySelector("dialog[open]:not(.finder)");
    if (key === "k" && !dialog) { event.preventDefault(); if (finder) finder = null; else { finder = {query:"",items:[],selected:0}; find(""); } }
    // A second Ctrl+F in the search field is the browser's own find.
    else if (key === "f" && timeline && !dialog && !finder && document.activeElement !== searchElement) { event.preventDefault(); if (showSearch) searchElement?.focus(); else void searchMessages(); }
  }
  function find(query) {
    const request = ++findRequest; finder.query = query;
    finding = command("find", query).then(items => { if (finder && request === findRequest) { finder.items = items; finder.selected = 0; } }, reason => setError(String(reason?.message || reason)));
  }
  async function findKey(event) {
    const step = {ArrowDown:1, ArrowUp:-1, PageDown:8, PageUp:-8}[event.key];
    if (step) { event.preventDefault(); finder.selected = Math.max(0, Math.min(finder.items.length - 1, finder.selected + step)); document.getElementById(`finder-${finder.selected}`)?.scrollIntoView({block:"nearest"}); }
    else if (event.key === "Enter") { event.preventDefault(); const shift = event.shiftKey; await finding; const item = finder?.items[finder.selected]; if (item) await go(item, shift); }
  }
  // Enter opens; Shift+Enter joins a channel's voice, or shows the channel a person is in.
  async function go(item, secondary) {
    finder = null;
    const server = servers.find(server => server.id === item.server);
    if (!server) return;
    rememberDraft();
    const user = item.kind === "person" ? server.users.find(user => user.session === item.id) : null;
    if (item.kind !== "person") await act("open", server.runtime_index, item.id);
    else if (secondary && user) await act("open", server.runtime_index, user.channel);
    else await act("open_direct", server.runtime_index, item.id);
    if (secondary && item.kind === "channel") joinVoice(item.id, server);
    await tick(); composerElement?.focus();
  }
  function findLabel(item) {
    const place = item.kind === "channel" && item.path ? ` in ${item.path}` : item.kind === "person" ? ` ${item.path}` : "";
    const unread = item.unread === 1 ? ", 1 unread message" : item.unread ? `, ${item.unread} unread messages` : "";
    return `${item.name}, ${item.kind}${place}, ${servers.find(server => server.id === item.server)?.name || ""}${unread}`;
  }
  function findParts(item) {
    const parts = []; let at = 0;
    for (const [start, end] of item.highlights) { parts.push([item.name.slice(at, start), false], [item.name.slice(start, end), true]); at = end; }
    return [...parts, [item.name.slice(at), false]].filter(([text]) => text);
  }
  function toggleMicrophone() {
    if (voiceIndex < 0 || !voice?.joined || audioActionPending) return;
    audioActionPending={server:voice.server_id,muted:!microphoneMuted,deafened:false};
    if (microphoneMuted) void activateAudio({microphone:true});
    else audio?.setLocalPolicy({muted:true});
    void command("toggle_mute",voiceIndex).catch(reason=>{audioActionPending=null;setError(String(reason?.message || reason));});
  }
  function togglePlayback() {
    if (voiceIndex < 0 || !voice?.joined || audioActionPending) return;
    audioActionPending={server:voice.server_id,muted:true,deafened:!playbackDeafened};
    if (playbackDeafened) void activateAudio({microphone:false});
    else audio?.setLocalPolicy({muted:true,deafened:true});
    void command("toggle_deaf",voiceIndex).catch(reason=>{audioActionPending=null;setError(String(reason?.message || reason));});
  }
  function stopAudio() {
    audio?.setLocalPolicy({muted:true,deafened:true}); audio?.syncVoice(null);
    voice = null; audioActionPending=null; void act("stop_audio");
  }
  function toggle(row) { const next = new Map(expanded); next.set(`${selected.id}:${row.id}`, !row.open); expanded = next; }
  async function older() {
    follow = false;
    if (messagesElement) { messagesElement.dataset.previousHeight = String(messagesElement.scrollHeight); messagesElement.dataset.previousTop = String(messagesElement.scrollTop); }
    await act("older");
  }
  function modal(node) { node.showModal(); return { destroy() { node.close(); } }; }
  // The mobile drawer slides in and out (instantly with reduced motion). A right
  // swipe on the chat opens it; a left swipe on it closes it.
  let drawerFocusReturn = false, swipe = null;
  function closeDrawer() { if (!drawer) return; drawerFocusReturn = true; drawer = false; }
  // Closed after the slide-out; focus returns once the page is interactive again.
  function drawerModal(node) {
    node.showModal();
    return { destroy() { node.close(); if (drawerFocusReturn) { drawerFocusReturn = false; navigationButton?.focus(); } } };
  }
  const drawerSlide = () => ({ duration: reducedMotion ? 0 : 220, easing: cubicOut,
    css: t => `transform:translateX(${(t - 1) * 100}%);--drawer-shown:${t}` });
  const swipePoint = event => ({ x:event.clientX, y:event.clientY, t:event.timeStamp });
  function beginSwipe(event, direction) {
    const allowed = mobile && event.isPrimary && (event.pointerType === "touch" || event.pointerType === "pen")
      && document.getSelection()?.isCollapsed !== false && !swipeExempt(event.target);
    swipe = allowed ? { pointer:event.pointerId, gesture:startSwipe(swipePoint(event), direction) } : null;
  }
  function followSwipe(event) { if (swipe?.pointer === event.pointerId) swipe.gesture = moveSwipe(swipe.gesture, swipePoint(event)); }
  // Whether the touch ended a swipe. Scrolling, a long press or a text selection keeps the gesture.
  function finishSwipe(event) {
    const done = swipe?.pointer === event.pointerId && document.getSelection()?.isCollapsed !== false
      && endSwipe(moveSwipe(swipe.gesture, swipePoint(event)));
    swipe = null;
    return done;
  }
  function unreadCount(item) { return Number(item?.unread_count || item?.unread || 0); }
  function unreadPosition(node) {
    if (!node) return { above:false, below:false };
    const bounds = node.getBoundingClientRect();
    const markers = [...node.querySelectorAll('[data-unread="true"]')].map((element) => element.getBoundingClientRect());
    return { above:markers.some((rect) => rect.top < bounds.top), below:markers.some((rect) => rect.bottom > bounds.bottom) };
  }
  function updateUnread() { serverUnread = unreadPosition(serverScroll); channelUnread = unreadPosition(channelScroll); }
  function scrollUnread(node, direction) {
    const bounds = node?.getBoundingClientRect();
    const candidates = [...(node?.querySelectorAll('[data-unread="true"]') || [])].filter((item) => direction === "above" ? item.getBoundingClientRect().top < bounds.top : item.getBoundingClientRect().bottom > bounds.bottom);
    (direction === "above" ? candidates.at(-1) : candidates[0])?.scrollIntoView({block:"nearest"});
  }
  function observeScroll(node) { const observer = new ResizeObserver(updateUnread); observer.observe(node); node.addEventListener("scroll", updateUnread, {passive:true}); return {destroy() { observer.disconnect(); node.removeEventListener("scroll", updateUnread); }}; }
  function cancelOrder() {
    if (orderFrame) cancelAnimationFrame(orderFrame);
    orderFrame = 0; orderTime = 0; orderDrag = null; previewIds = null; draggingId = null; pointerGhost = null;
  }
  function beginOrder(event, id, pointer = false) {
    if (orderPending || !capabilities.includes("server_order") || !serverScroll) { event.preventDefault(); return; }
    if (pointer && event.button !== 0) return;
    const baseline = servers.map(server => server.id), slot = serverScroll.querySelector(".server-order-slot");
    if (!baseline.includes(id) || !slot) return;
    cancelOrder();
    const style = getComputedStyle(serverScroll);
    orderDrag = {id, baseline, rect:serverScroll.getBoundingClientRect(), pitch:slot.offsetHeight + (parseFloat(style.rowGap) || 0), padding:parseFloat(style.paddingTop) || 0, pointer, pointerId:event.pointerId, point:{x:event.clientX,y:event.clientY}};
    previewIds = baseline.slice(); draggingId = id;
    if (pointer) {
      event.preventDefault(); event.stopPropagation(); serverScroll.setPointerCapture(event.pointerId);
      pointerGhost = {...orderDrag.point};
    } else { event.dataTransfer.effectAllowed = "move"; event.dataTransfer.setData("application/x-nuschl-server-order", "1"); }
    orderStatus = "Move the server, then release to save. Escape cancels.";
  }
  function orderPosition() {
    if (!orderDrag) return;
    const {rect,point,pitch,padding,id} = orderDrag;
    if (point.x < rect.left || point.x > rect.right || point.y < rect.top || point.y > rect.bottom) return;
    const target = Math.floor((point.y - rect.top + serverScroll.scrollTop - padding) / pitch);
    const next = moveServer(previewIds,id,target);
    if (next !== previewIds) previewIds = next;
  }
  function scheduleOrderScroll() {
    if (orderFrame || !orderDrag) return;
    const step = now => {
      orderFrame = 0;
      if (!orderDrag || !serverScroll) return;
      const {rect,point} = orderDrag;
      const speed = edgeSpeed(point.y - rect.top,rect.height,point.x >= rect.left && point.x <= rect.right);
      const before = serverScroll.scrollTop;
      serverScroll.scrollTop += speed * Math.min(.05,orderTime ? (now-orderTime)/1000 : .016);
      orderTime = now;
      orderPosition();
      if (speed && serverScroll.scrollTop !== before) orderFrame = requestAnimationFrame(step);
    };
    orderFrame = requestAnimationFrame(step);
  }
  function moveOrder(event, pointer = false) {
    if (!orderDrag || orderDrag.pointer !== pointer || (pointer && event.pointerId !== orderDrag.pointerId)) return;
    event.preventDefault();
    if (!pointer && event.dataTransfer) event.dataTransfer.dropEffect = "move";
    orderDrag.point = {x:event.clientX,y:event.clientY};
    if (pointer) pointerGhost = {...orderDrag.point};
    orderPosition(); scheduleOrderScroll();
  }
  function cancelPointerOrder(event) {
    // Native HTML dragging cancels its mouse pointer stream. Only our captured
    // touch/handle gesture owns these cancellation/capture-loss events.
    if (!orderDrag?.pointer || event.pointerId !== orderDrag.pointerId) return;
    if (event.type === "lostpointercapture" && event.target !== serverScroll) return;
    cancelOrder();
  }
  async function saveOrder(ids) {
    if (orderPending || !capabilities.includes("server_order") || sameOrder(ids,servers.map(server => server.id))) return;
    orderPending = true; orderStatus = "Saving server order…";
    try { await command("set_server_order",ids); orderStatus = "Server order saved in this browser."; }
    catch (reason) { setError(String(reason?.message || reason)); orderStatus = "Server order was not saved."; }
    finally { orderPending = false; await tick(); updateUnread(); }
  }
  function dropOrder(event, pointer = false) {
    if (!orderDrag || orderDrag.pointer !== pointer || (pointer && event.pointerId !== orderDrag.pointerId)) return;
    event.preventDefault(); event.stopPropagation();
    const {point,rect,baseline} = orderDrag, ids = previewIds.slice();
    const valid = point.x >= rect.left && point.x <= rect.right && point.y >= rect.top && point.y <= rect.bottom && sameOrder(baseline,servers.map(server=>server.id));
    cancelOrder(); suppressBrowseUntil = performance.now() + 250;
    if (pointer && serverScroll?.hasPointerCapture(event.pointerId)) serverScroll.releasePointerCapture(event.pointerId);
    if (valid) void saveOrder(ids);
  }
  async function moveOrderKey(id, delta) {
    const current = servers.map(server => server.id), from = current.indexOf(id);
    if (from < 0) return;
    cancelOrder(); await saveOrder(moveServer(current,id,from+delta));
    await tick();
    const slot = [...(serverScroll?.querySelectorAll(".server-order-slot") || [])].find(node=>node.dataset.serverId===id);
    slot?.scrollIntoView({block:"nearest",behavior:reducedMotion ? "instant" : "smooth"});
  }
  function orderKey(event,id,handle = false) {
    if ((handle || event.ctrlKey && event.shiftKey) && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
      event.preventDefault(); event.stopPropagation(); void moveOrderKey(id,event.key === "ArrowUp" ? -1 : 1);
    }
  }
  function cancelMove() {
    if (moveFrame) cancelAnimationFrame(moveFrame);
    moveFrame = 0; moveTime = 0; userMove = null; moveGhost = null;
  }
  // Where we may move people: ask for the channels' permissions we lack.
  // Once per channel and connection; the SDK bounds retries.
  const permissionsAsked = new Set();
  function askMovePermissions(channels) {
    const key = id => `${selected.id}\n${selected.generation}\n${id}`;
    const unknown = channels.filter(channel => !channel.direct && selected.permissions?.[channel.id] === undefined && !permissionsAsked.has(key(channel.id))).map(channel => channel.id).slice(0, 128);
    if (permissionsAsked.size > 4096) permissionsAsked.clear();
    for (const id of unknown) permissionsAsked.add(key(id));
    if (unknown.length) void command("query_permissions", selectedIndex, unknown).catch(() => {});
  }
  function beginMove(event, user, pointer = false) {
    if (!selected || !channelScroll || user.session === selected.own_session || !may(selected, user.channel, 0x20)) { if (!pointer) event.preventDefault(); return; }
    if (pointer && event.button !== 0) return;
    cancelMove();
    userMove = {server:selected.id, index:selectedIndex, session:user.session, name:user.name, source:user.channel, target:null, pointer, pointerId:event.pointerId, point:{x:event.clientX,y:event.clientY}, sent:false, notice:null};
    askMovePermissions(rows);
    if (pointer) { event.preventDefault(); event.stopPropagation(); channelScroll.setPointerCapture(event.pointerId); moveGhost = {...userMove.point}; }
    else { event.dataTransfer.effectAllowed = "move"; event.dataTransfer.setData("application/x-nuschl-user", "1"); }
    moveStatus = `Moving ${user.name}. Release over a channel; Escape cancels.`;
  }
  // The channel under the pointer they may go to; where they are is no move.
  function moveTargetAt(point) {
    const node = document.elementFromPoint(point.x, point.y)?.closest?.("[data-tree-channel]");
    if (!node || !channelScroll?.contains(node)) return userMove.target;
    const channel = Number(node.dataset.treeChannel), user = selected.users.find(user => user.session === userMove.session);
    return channel !== userMove.source && mayMove(selected, user, channel) ? channel : null;
  }
  function retargetMove() { const target = moveTargetAt(userMove.point); if (target !== userMove.target) userMove.target = target; }
  function scheduleMoveScroll() {
    if (moveFrame || !userMove?.pointer) return;
    const step = now => {
      moveFrame = 0;
      if (!userMove?.pointer || userMove.sent || !channelScroll) return;
      const rect = channelScroll.getBoundingClientRect(), {x,y} = userMove.point;
      const speed = edgeSpeed(y - rect.top, rect.height, x >= rect.left && x <= rect.right), before = channelScroll.scrollTop;
      channelScroll.scrollBy({top:speed * Math.min(.05, moveTime ? (now - moveTime) / 1000 : .016), behavior:"instant"});
      moveTime = now;
      if (speed && channelScroll.scrollTop !== before) { retargetMove(); moveFrame = requestAnimationFrame(step); }
    };
    moveFrame = requestAnimationFrame(step);
  }
  function dragMove(event, pointer = false) {
    if (!userMove || userMove.sent || userMove.pointer !== pointer || (pointer && event.pointerId !== userMove.pointerId)) return;
    event.preventDefault();
    userMove.point = {x:event.clientX, y:event.clientY};
    if (pointer) moveGhost = {...userMove.point};
    retargetMove();
    if (!pointer && event.dataTransfer) event.dataTransfer.dropEffect = userMove.target == null ? "none" : "move";
    scheduleMoveScroll();
  }
  function dropMove(event, pointer = false) {
    if (!userMove || userMove.sent || userMove.pointer !== pointer || (pointer && event.pointerId !== userMove.pointerId)) return;
    event.preventDefault(); event.stopPropagation();
    if (pointer && channelScroll?.hasPointerCapture(event.pointerId)) channelScroll.releasePointerCapture(event.pointerId);
    moveGhost = null; suppressDirectUntil = performance.now() + 250;
    void sendMove();
  }
  function cancelPointerMove(event) {
    if (!userMove?.pointer || userMove.sent || event.pointerId !== userMove.pointerId) return;
    if (event.type === "lostpointercapture" && event.target !== channelScroll) return;
    cancelMove();
  }
  // The preview holds until the server shows the move or refuses it.
  async function sendMove() {
    const move = userMove;
    if (!move || move.target == null) { cancelMove(); moveStatus = ""; return; }
    move.sent = true; move.notice = selected?.notice ?? null;
    moveStatus = `Moving ${move.name}…`;
    try { await command("move_user", move.index, move.session, move.target); }
    catch (reason) { setError(String(reason?.message || reason)); if (userMove === move) cancelMove(); return; }
    setTimeout(() => { if (userMove === move) cancelMove(); }, 3000);
  }
  function openMoveChooser(user) {
    if (!selected || user.session === selected.own_session || !may(selected, user.channel, 0x20)) return;
    moveChooser = {server:selected.id, session:user.session, name:user.name};
    askMovePermissions(selected.channels);
  }
  function chooseMove(channel) {
    const user = selected?.users.find(user => user.session === moveChooser?.session);
    moveChooser = null;
    if (!user || !mayMove(selected, user, channel)) return;
    cancelMove();
    userMove = {server:selected.id, index:selectedIndex, session:user.session, name:user.name, source:user.channel, target:channel, pointer:false, sent:false, notice:null};
    void sendMove();
  }
  async function searchMessages() { showSearch = !showSearch; if (!showSearch) search = ""; else { await tick(); searchElement?.focus(); } }
</script>

<!-- With `struck` (true or false) the icon carries a strike line that draws in and out. -->
{#snippet icon(name, struck = null)}<svg class="icon" class:struck width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d={icons[name] || icons.hash}/>{#if struck !== null}<line class="strike" x1="3" y1="3" x2="21" y2="21" pathLength="1"/>{/if}</svg>{/snippet}
{#snippet navigation()}
  {#if moveGhost && userMove}<div class="user-move-ghost" aria-hidden="true" style:left={`${moveGhost.x}px`} style:top={`${moveGhost.y}px`}><span class={`mini-avatar ${avatarColor(userMove.name)}`}>{initials(userMove.name)}</span><span>{userMove.name}</span></div>{/if}
  {#if pointerGhost && draggingId}<div class="server-order-ghost" aria-hidden="true" style:left={`${pointerGhost.x - 23}px`} style:top={`${pointerGhost.y - 23}px`}>{initials(servers.find(server=>server.id===draggingId)?.name || "")}</div>{/if}
  <nav class="servers" class:server-order-enabled={capabilities.includes("server_order")} aria-label="Servers">
    <div class="server-scroll" role="presentation" bind:this={serverScroll} use:observeScroll ondragover={event => moveOrder(event)} ondrop={event => dropOrder(event)} onpointermove={event=>moveOrder(event,true)} onpointerup={event=>dropOrder(event,true)} onpointercancel={cancelPointerOrder} onlostpointercapture={cancelPointerOrder}>
      {#each railServers as server (server.id)}
        <div class="server-order-slot" data-server-id={server.id} class:server-order-source={draggingId === server.id} animate:flip={{duration:reducedMotion ? 0 : 180}}>
          <button class:active={selected?.id === server.id} class="server-icon" data-unread={unreadCount(server) > 0} title={server.name} aria-label={`Browse ${server.name}`} aria-current={selected?.id === server.id ? "true" : undefined} draggable={capabilities.includes("server_order") && !arrangeServers && !orderPending} ondragstart={event => beginOrder(event,server.id)} ondragend={cancelOrder} onkeydown={event=>orderKey(event,server.id)} onclick={() => { if (performance.now() >= suppressBrowseUntil && !orderDrag) void act("browse_server",server.runtime_index); }}><span>{initials(server.name)}</span>{#if unreadCount(server)}<span class="badge">{Math.min(99, unreadCount(server))}</span>{/if}<i class:connected={server.status === "connected"} class="connection-dot" aria-label={labels[server.status]}></i></button>
          {#if arrangeServers}<button class="server-order-handle" aria-label={`Move ${server.name}; use arrow keys or drag`} title="Drag to move; arrow keys also work" disabled={orderPending} onpointerdown={event=>beginOrder(event,server.id,true)} onkeydown={event=>orderKey(event,server.id,true)} onclick={()=>orderStatus="Use Up and Down to move this server, or drag its handle."}>⋮</button>{/if}
        </div>
      {/each}
    </div>
    {#if serverUnread.above}<button class="unread-jump rail-jump rail-up" aria-label="Unread servers above" onclick={() => scrollUnread(serverScroll,"above")}>↑</button>{/if}
    {#if serverUnread.below}<button class="unread-jump rail-jump" aria-label="Unread servers below" onclick={() => scrollUnread(serverScroll,"below")}>↓</button>{/if}
    {#if capabilities.includes("server_order")}<button class="icon-button server-order-toggle" aria-label={arrangeServers ? "Finish arranging servers" : "Reorder servers"} aria-pressed={arrangeServers} title="Reorder servers" disabled={orderPending} onclick={() => { cancelOrder(); arrangeServers = !arrangeServers; orderStatus = arrangeServers ? "Drag a handle, or focus it and use arrow keys." : ""; }}>↕</button>{/if}
    <span class="sr-only" role="status" aria-live="polite">{orderStatus}</span>
    <button class="add server-icon" aria-label="Add server" title="Add server" onclick={addServer}>{@render icon("plus")}</button>
    <button class="icon-button settings-rail" aria-label="Settings" title="Settings" onclick={() => { showSettings = true; drawer = false; }}>{@render icon("settings")}</button>
  </nav>
  <aside class="channels" aria-label="Server and channels">
    <header class="server-header"><div><span class="eyebrow">YOUR SERVER</span><h1>{selected?.name || "Nuschl"}</h1></div>{#if mobile}<button class="icon-button" aria-label="Close navigation" onclick={closeDrawer}>{@render icon("close")}</button>{:else}<button class="icon-button" aria-label="Server settings" title="Server settings" disabled={!selected} onclick={editServer}>{@render icon("settings")}</button>{/if}</header>
    {#if selected}
      <div class="server-connection"><span class:connected={selected.status === "connected"} class="connection-dot"></span><span class="status" role="status">{labels[selected.status]}</span>{#if !clientUnavailable}<button class="control-button" class:emphasis={selected.status === "disconnected"} disabled={!loaded} onclick={() => act(selected.status === "disconnected" ? "connect" : "disconnect", selectedIndex)}>{selected.status === "disconnected" ? "Connect" : "Disconnect"}</button>{/if}</div>
      <p class="security" title="TLS protects the connection; this server can read legacy chat.">{@render icon("unlock")}<span>Server can read chat</span><span class="transport">TLS</span></p>
      {#if selected.notice}<div class="notice" role="status"><span>{selected.notice}</span>{#if capabilities.includes("dismiss_notice")}<button class="icon-button" aria-label="Dismiss notice" onclick={() => act("dismiss_notice",selectedIndex)}>{@render icon("close")}</button>{/if}</div>{/if}
      {#if historyHealth}<div class="notice" role="status"><span>{historyHealth}</span></div>{/if}
      {#if selected.certificate}<div class="certificate" role="alert"><strong>{selected.pin ? "Certificate changed" : "Verify server identity"}</strong><p>Compare this fingerprint with the server owner before trusting it.</p><code>{selected.certificate}</code><button class="primary" onclick={() => act("accept_certificate",selectedIndex,selected.certificate)}>Trust this certificate</button></div>{/if}
      <div class="section-label"><span>CHANNELS</span><span>{selected.channels.length}</span></div>
      {#if channelUnread.above}<button class="unread-jump" onclick={() => scrollUnread(channelScroll,"above")}>↑ Unread channels above</button>{/if}
      <nav class="channel-tree" aria-label="Channels" bind:this={channelScroll} use:observeScroll ondragover={event => dragMove(event)} ondrop={event => dropMove(event)} onpointermove={event => dragMove(event,true)} onpointerup={event => dropMove(event,true)} onpointercancel={cancelPointerMove} onlostpointercapture={cancelPointerMove}>
        {#each treeEntries as entry (entry.key)}
          {@const row = entry.row}
          <div class="tree-entry" data-tree-channel={row.id} animate:flip={{duration:reducedMotion ? 0 : 180}}>
          {#if !entry.user}
          <div class="channel-row" class:move-takes={userMove && !userMove.sent && userMove.server === selected.id && mayMove(selected, selected.users.find(user => user.session === userMove.session), row.id)} class:move-target={userMove?.server === selected.id && userMove.target === row.id} style:--depth={Math.min(row.depth,8)}>
            {#if row.hasChildren}<button class="expand" aria-label={`${row.open ? "Collapse" : "Expand"} ${row.name}`} aria-expanded={row.open} onclick={() => toggle(row)}>{row.open ? "▾" : "▸"}</button>{:else}<span class="spacer"></span>{/if}
            <button class:current={timeline?.channel === row.id} class:voice-current={ownUser?.channel === row.id && selected.status === "connected"} class="channel" onpointerdown={(event) => lastChannelPointer = event.pointerType} data-unread={unreadCount(row) > 0} title={`${row.name || "Root"} — click for chat, double-click to join voice`} aria-label={row.name || "Root"} aria-current={timeline?.channel === row.id ? "page" : undefined} onclick={() => openChannel(row.id)} ondblclick={() => lastChannelPointer !== "touch" && !row.direct && joinVoice(row.id)}><span class="channel-symbol" aria-hidden="true">{row.direct ? "@" : "#"}</span><span class="channel-name">{row.name || "Root"}</span>{#if ownUser?.channel === row.id && !row.direct && selected.status === "connected"}<span class="voice-marker" title="Your voice channel">{@render icon("voice")}</span>{/if}{#if unreadCount(row)}<span class="badge">{Math.min(99,unreadCount(row))}</span>{/if}</button>
            {#if row.hiddenUsers}{@const people = `${row.hiddenUsers} ${row.hiddenUsers === 1 ? "person" : "people"}`}<span class="member-count" class:talking={row.hiddenTalking} role="img" aria-label={`${people} inside`} title={`${people} in collapsed channels`}>{@render icon("user")}{row.hiddenUsers}</span>{/if}
          </div>
          {:else}
          {@const user = entry.user}{@const movable = user.session !== selected.own_session && may(selected, user.channel, 0x20)}
          <div class="user-slot" class:movable class:moving={userMove?.server === selected.id && userMove.session === user.session} class:sent={userMove?.sent}><button class:talking={user.talking} class:own-user={user.session === selected.own_session} class="user" title={user.name} aria-label={user.session === selected.own_session ? `${user.name} (you)` : `Private message ${user.name}`} style:--depth={Math.min(row.depth,8)} disabled={user.session === selected.own_session} draggable={movable} ondragstart={event => beginMove(event,user)} ondragend={() => { if (userMove && !userMove.sent) cancelMove(); }} oncontextmenu={event => { if (movable) { event.preventDefault(); openMoveChooser(user); } }} onclick={async () => { if (performance.now() < suppressDirectUntil || userMove) return; rememberDraft(); await act("open_direct",selectedIndex,user.session); if (mobile) drawer = false; }}><span class={`mini-avatar ${avatarColor(user.name)}`}>{initials(user.name)}</span><span class="user-name">{user.name}</span>{#if user.self_deaf || user.deafened}<span title="Deafened">{@render icon("headphones", true)}</span>{:else if user.self_mute || user.muted}<span title="Muted">{@render icon("mic", true)}</span>{/if}</button>{#if movable}<button class="user-move-handle" aria-label={`Move ${user.name}`} title="Drag to move, or choose a channel" onpointerdown={event => beginMove(event,user,true)} onclick={() => { if (!userMove) openMoveChooser(user); }}>⋮⋮</button>{/if}</div>
          {/if}
          </div>
        {/each}
      </nav>
      <span class="sr-only" role="status" aria-live="polite">{moveStatus}</span>
      {#if channelUnread.below}<button class="unread-jump" onclick={() => scrollUnread(channelScroll,"below")}>↓ Unread channels below</button>{/if}
      <p class="navigation-hint">Click to read · double-click for voice</p>
      <div class="profile"><span class={`avatar ${avatarColor(selected.username)}`} aria-hidden="true">{initials(selected.username)}</span><div><strong>{selected.username}</strong><small>{selected.status === "connected" ? selectedVoiceChannel?.name || "Connected" : "Offline"}</small></div><button class="icon-button" aria-label="Settings" onclick={() => { showSettings = true; drawer = false; }}>{@render icon("settings")}</button></div>
    {:else}<div class="nav-empty"><h2>A place for your people.</h2><p>Bring your Mumble server. Keep your conversations.</p><button class="primary" onclick={addServer}>Add your first server</button></div>{/if}
  </aside>
{/snippet}
{#snippet voiceProcessingSettings()}
  <section class="settings-section voice-processing" aria-labelledby="voice-processing-title"><h3 id="voice-processing-title">Microphone processing</h3>
    {#if processingRows.reported}<dl class="processing-list">{#each processingRows.rows as row (row.field)}<div><dt>{row.label}</dt><dd data-state={row.state}>{row.text}</dd></div>{/each}</dl>
    {#if processingRows.hint}<p class="processing-hint" role="note">{processingRows.hint}</p>{/if}
    <p>What the browser reported for the most recent microphone use. It does not measure how much echo is removed.</p>
    {:else}<p>Unmute once to see which echo cancellation, noise suppression and automatic gain your browser applied.</p>{/if}
  </section>
{/snippet}

<main>
  {#if !mobile}<div class="navigation" class:server-order-layout={capabilities.includes("server_order")}>{@render navigation()}</div>{/if}
  <section class="chat" aria-label="Chat" onpointerdown={event => { if (!drawer) beginSwipe(event, 1); }} onpointermove={followSwipe} onpointerup={event => { if (finishSwipe(event)) drawer = true; }} onpointercancel={() => swipe = null}>
    <header class="chat-header">
      {#if mobile}<button class="icon-button menu-button" bind:this={navigationButton} aria-label="Open navigation" aria-expanded={drawer} onclick={() => drawer = true}>{@render icon("menu")}</button>{/if}
      <span class="header-hash" aria-hidden="true">{currentChannel?.direct ? "@" : "#"}</span><div class="channel-title"><h2>{timeline?.name || "Welcome to Nuschl"}</h2><span>{currentChannel?.direct ? "Private conversation" : selected?.name || "Your voice. Your space."}</span></div>
      <div class="header-actions">
        {#if canStartVideo && !video}<button class="icon-button" title="Start call" aria-label="Start call" onclick={() => openVideo()}>{@render icon("video")}</button>{/if}
        {#if timeline && !currentChannel?.direct}<button class="join-button" disabled={selected?.status !== "connected"} onclick={() => joinVoice()} title="Join this voice channel" aria-label="Join voice">{@render icon("voice")}<span>Join voice</span></button>{/if}
        <button class:pressed={showSearch} class="icon-button" aria-label="Search messages" aria-expanded={showSearch} title={`Search loaded messages (${primary}+F)`} disabled={!timeline} onclick={searchMessages}>{@render icon("search")}</button>
        <button class:pressed={membersOpen} class="icon-button members-toggle" aria-label="Show members" aria-expanded={membersOpen} disabled={!timeline} onclick={() => membersOpen = !membersOpen}>{@render icon("people")}</button>
        {#if !mobile}<button class="icon-button" aria-label="Add server" title="Add server" onclick={addServer}>{@render icon("plus")}</button>{/if}
      </div>
    </header>
    {#if showSearch}<form class="search-bar" onsubmit={(event) => event.preventDefault()}><label for="search-messages">Search this loaded history</label><input id="search-messages" bind:this={searchElement} bind:value={search} type="search" placeholder="Message text or person…" onkeydown={(event) => { if (event.key === "Escape") { showSearch = false; search = ""; } }}><small>{displayedMessages.length} results</small><button class="icon-button" aria-label="Close search" onclick={() => { showSearch = false; search = ""; }}>{@render icon("close")}</button></form>{/if}
    {#each video ? [video] : [] as active (active.call)}
      <VideoCallPanel call={active.call} owner={active.creating} url={active.link} minimized={videoMinimized}
        ontoggle={() => videoMinimized = !videoMinimized} onleave={closeVideo}/>
    {/each}
    {#if clientUnavailable}<div class="error" role="alert"><span>{error || "The client is unavailable. Reload to reconnect."}</span><button class="control-button" onclick={() => location.reload()}>Reload client</button></div>{:else if error}<div class="error" role="alert"><span>{error}</span><button class="icon-button" aria-label="Dismiss error" onclick={() => setError("")}>{@render icon("close")}</button></div>{/if}
    {#if welcome}
      {#key welcome.token}<section class="server-welcome" aria-label="Server welcome">
        <header><h3 title={`Welcome to ${selected.name}`}>Welcome to {selected.name}</h3><button onclick={() => act("dismiss_welcome", selectedIndex, welcome.token)} aria-label="Dismiss server welcome">Dismiss</button></header>
        <div class="server-welcome-body" tabindex="0" role="region" aria-label="Welcome message"><TypedDocument document={welcome.document} /></div>
      </section>{/key}
    {/if}
    <div class="conversation">
      <div class="messages" bind:this={messagesElement} onscroll={() => { if (messagesElement) follow = messagesElement.scrollHeight - messagesElement.scrollTop - messagesElement.clientHeight < 80; }}>
        {#if timeline?.has_older}<div class="history-start"><button class="text-button" disabled={timeline.loading} onclick={older}>{timeline.loading ? "Loading history…" : "Load older messages"}</button></div>{/if}
        {#if !timeline}<div class="welcome"><div class="welcome-logo"><img src="/nuschl.svg" alt="" width="74" height="74"></div><p class="eyebrow">A LITTLE CLOSER, FROM ANYWHERE</p><h2>Your voice. Your space.</h2><p>Drop into your server, find your people, and pick up the conversation.</p><button class="primary" onclick={addServer}>Add a server</button><small>Compatible with Mumble · history saved in this browser</small></div>
        {:else if !displayedMessages.length}<div class="channel-welcome"><span class="welcome-hash" aria-hidden="true">{currentChannel?.direct ? "@" : "#"}</span><h2>{search ? "No messages found" : `Welcome to ${timeline.name}`}</h2><p>{search ? "Try another word, or load older history." : timeline.loading ? "Loading your conversations…" : "This is the start of the history saved in this browser. Say hello."}</p></div>{/if}
        {#each unmatchedVideoCalls as call (call.sender)}
          <div class="video-invitation"><strong>{call.author} started a video call</strong>
            <button class="control-button" disabled={selected?.status !== "connected"} onclick={() => openVideo(call.link)}>Join call</button></div>
        {/each}
        {#each displayedMessages as message (message.serial)}
          <Message {message} callAction={configuredCallLink(message.call?.link, videoHost)
            ? () => openVideo(message.call.link) : null} canJoinCall={selected?.status === "connected"}/>
        {/each}
      </div>
      {#if membersOpen && timeline}<aside class="members" aria-label="Channel members"><div class="section-label"><span>IN THIS CHANNEL</span><span>{selectedUsers.length}</span></div>{#each selectedUsers as user (user.session)}<button class="member" disabled={user.session === selected.own_session} onclick={() => act("open_direct",selectedIndex,user.session)}><span class={`mini-avatar ${avatarColor(user.name)}`}>{initials(user.name)}</span><span>{user.name}{user.session === selected.own_session ? " (you)" : ""}</span></button>{/each}{#if !selectedUsers.length}<p class="muted">No one here yet.</p>{/if}</aside>{/if}
    </div>
    {#if voice}
      <section class="voice-bar" aria-label="Voice controls">
        <div class="voice-description"><strong>{voiceChannel?.name || "Voice channel"}</strong><small>{voiceServer?.name} · {voiceStatus}</small>{#if voice.bitrate_line}<small class="voice-bitrate" title={voice.bitrate_why}>{voice.bitrate_line}</small>{/if}</div>
        <div class="voice-actions">
          <button class="icon-button voice-toggle" disabled={!voice.joined || !!audioActionPending} aria-label={microphoneMuted ? "Unmute microphone" : "Mute microphone"} aria-pressed={microphoneMuted} title={microphoneMuted ? "Unmute microphone" : "Mute microphone"} onclick={toggleMicrophone}>{@render icon("mic", microphoneMuted)}</button>
          <button class="icon-button voice-toggle" disabled={!voice.joined || !!audioActionPending} aria-label={playbackDeafened ? "Enable sound" : "Deafen"} aria-pressed={playbackDeafened} title={playbackDeafened ? "Enable sound" : "Deafen (also mutes microphone)"} onclick={togglePlayback}>{@render icon("headphones", playbackDeafened)}</button>
          {#if !voice.deafened && (audioState?.contextState !== "running" || audioState?.phase === "deafened")}<button class="control-button" onclick={() => activateAudio({microphone:false})}>Enable audio</button>{/if}
          <button class="icon-button" aria-label="Stop audio" title="Stop audio; stay connected for chat" onclick={stopAudio}>{@render icon("leave")}</button>
        </div>
      </section>
    {/if}
    {#if audioState?.message && voice}<div class="audio-notice" role="status">{audioState.message}</div>{/if}
    <form class="composer" onsubmit={send}>
      <div class="composer-input"><label class="sr-only" for="message">Message</label><textarea id="message" bind:this={composerElement} bind:value={body} rows="2" maxlength="65536" oninput={rememberDraft} placeholder={timeline ? `Message ${timeline.name}` : "Choose a channel to start chatting"} readonly={clientUnavailable} disabled={!timeline || (selected?.status !== "connected" && !clientUnavailable)} onkeydown={(event) => { if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); send(); } else if (event.ctrlKey && !event.altKey && (event.key === "." || event.key === ";")) { event.preventDefault(); emojiOpen = !emojiOpen; } }}></textarea><button type="button" class="icon-button emoji-toggle" aria-label="Emoji, stickers and GIFs" aria-haspopup="dialog" aria-expanded={emojiOpen} class:pressed={emojiOpen} title="Emoji, stickers and GIFs (Ctrl+.)" disabled={!timeline || clientUnavailable} onclick={() => emojiOpen ? closeEmoji() : emojiOpen = true}>{@render icon("smile")}</button><button class="send-button" type="submit" aria-label="Send" title="Send (Ctrl/⌘ Enter)" disabled={!body.trim() || !canSend}>{@render icon("send")}</button></div>
      {#if emojiOpen}<EmojiPicker onpick={insertEmoji} onclose={closeEmoji} recent={emojiRecent} />{/if}
      <div class="composer-footer"><small>Enter adds a newline · Ctrl/⌘ Enter sends</small><span class="local-history">History stays in this browser</span></div>
    </form>
  </section>
</main>

{#if mobile && drawer}<dialog class="navigation-drawer" use:drawerModal transition:drawerSlide onclose={(event) => { if (event.currentTarget.isConnected) closeDrawer(); }} oncancel={(event) => { event.preventDefault(); closeDrawer(); }} aria-label="Server navigation" onpointerdown={event => { if (event.target === event.currentTarget) closeDrawer(); else beginSwipe(event, -1); }} onpointermove={followSwipe} onpointerup={event => { if (finishSwipe(event)) closeDrawer(); }} onpointercancel={() => swipe = null}><div class="navigation" class:server-order-layout={capabilities.includes("server_order")}>{@render navigation()}</div></dialog>{/if}
{#if showAdd}<dialog class="modal server-modal" use:modal onclose={() => showAdd = false} oncancel={() => showAdd = false} aria-labelledby="server-form-title"><header class="modal-header"><div><p class="eyebrow">MAKE YOURSELF AT HOME</p><h2 id="server-form-title">{editingIndex === null ? "Add a server" : "Edit server"}</h2></div><button class="icon-button" aria-label="Close server form" onclick={() => showAdd = false}>{@render icon("close")}</button></header><form class="connection" onsubmit={connect}><p class="muted">Connect to your Mumble server. Your bookmarks and observed chat history are saved in this browser.</p><label>Display name <input bind:value={serverName} maxlength="128" autocomplete="off" placeholder="Friday night"></label><div class="form-row"><label>Server <input bind:value={host} required autocomplete="off" placeholder="voice.example.org"></label><label class="port-field">Port <input bind:value={port} type="number" min="1" max="65535" required></label></div><label>Your name <input bind:value={username} required maxlength="128" autocomplete="nickname" placeholder="How your friends know you"></label><label for="server-password">Server password (optional)</label><input id="server-password" type="password" bind:value={password} maxlength="1024" autocomplete="current-password"><details><summary>Certificate and connection</summary><label>Certificate SHA-256 (optional)<input bind:value={pin} autocomplete="off" spellcheck="false" placeholder="64 hexadecimal characters"></label><small class="relay">Relay: {relay}. TLS runs in this browser.</small></details><div class="modal-actions"><button type="button" onclick={() => showAdd = false}>Cancel</button><button class="primary" type="submit" disabled={!loaded || !!link.error}>Connect</button></div></form></dialog>{/if}
<svelte:window onkeydown={shortcut} />
{#if finder}{@const item = finder.items[finder.selected]}{@const count = finder.items.length}<dialog class="modal finder" use:modal aria-label="Quick switcher" oncancel={() => finder = null} onclose={event => { if (event.currentTarget.isConnected) finder = null; }} onclick={event => { if (event.target === event.currentTarget) finder = null; }}>
  <!-- svelte-ignore a11y_autofocus (The switcher opens on a keystroke to take typing at once, as on the desktop.) -->
  <input class="finder-input" autofocus role="combobox" aria-expanded="true" aria-controls="finder-results" aria-activedescendant={item ? `finder-${finder.selected}` : undefined} aria-label={item ? `Find a channel or person: ${count} ${count === 1 ? "result" : "results"}, ${item.name} (${item.kind}) highlighted` : finder.query.trim() ? "Find a channel or person: no results" : "Find a channel or person"} placeholder="Where to? A channel, person or conversation" value={finder.query} oninput={event => find(event.currentTarget.value)} onkeydown={findKey}>
  <div id="finder-results" class="finder-results" role="listbox" aria-label={count === 1 ? "1 result" : `${count} results`}>
    {#each finder.items as found, index (`${found.server} ${found.kind} ${found.id}`)}<!-- svelte-ignore a11y_click_events_have_key_events (The field above takes the keys: arrows, Enter and Shift+Enter.) --><div id={`finder-${index}`} class="finder-row" class:selected={index === finder.selected} role="option" tabindex="-1" aria-selected={index === finder.selected} aria-label={findLabel(found)} onclick={event => go(found, event.shiftKey)}><span class="finder-name">{#each findParts(found) as [text, matched]}{#if matched}<mark>{text}</mark>{:else}{text}{/if}{/each}</span><span class="finder-path">{found.path}</span>{#if found.unread}<span class="badge">{Math.min(999, found.unread)}</span>{/if}<span class="finder-server">{servers.find(server => server.id === found.server)?.name}</span></div>{/each}
    {#if !count}<p class="finder-empty">{finder.query.trim() ? `Nothing matches. Messages are found with ${primary}+F.` : "Type to find a channel, person or conversation on any server."}</p>{/if}
  </div>
  <p class="finder-hints" aria-hidden="true"><span>↑↓ choose</span><span>Enter open</span><span>Shift+Enter join voice</span><span>Esc close</span></p>
</dialog>{/if}
{#if moveChooser}{@const user = selected?.users.find(user => user.session === moveChooser.session)}<dialog class="modal move-chooser" use:modal aria-labelledby="move-title" oncancel={() => moveChooser = null} onclose={() => moveChooser = null}><header class="modal-header"><h2 id="move-title">Move {moveChooser.name}</h2><button class="icon-button" aria-label="Close" onclick={() => moveChooser = null}>{@render icon("close")}</button></header><div class="move-choices">{#each (selected?.channels || []).filter(channel => !channel.direct && mayMove(selected, user, channel.id)) as channel (channel.id)}<button onclick={() => chooseMove(channel.id)}><span class="channel-symbol" aria-hidden="true">#</span>{channel.name || "Root"}</button>{:else}<p class="muted">No channel to move them to.</p>{/each}</div></dialog>{/if}
{#if showSettings}<dialog class="modal settings-modal" use:modal onclose={() => showSettings = false} oncancel={() => showSettings = false} aria-labelledby="settings-title"><header class="modal-header"><div><p class="eyebrow">MAKE IT YOURS</p><h2 id="settings-title">Settings</h2></div><button class="icon-button" aria-label="Close settings" onclick={() => showSettings = false}>{@render icon("close")}</button></header><section class="settings-section"><h3>Appearance</h3><label for="theme-choice">Theme</label><select id="theme-choice" bind:value={settings.theme} onchange={persistSettings}><option value="system">Match device</option><option value="dark">Dark</option><option value="light">Light</option></select><label for="density-choice">Message spacing</label><select id="density-choice" bind:value={settings.density} onchange={persistSettings}><option value="comfortable">Comfortable</option><option value="compact">Compact</option></select><label for="size-choice">Text size</label><select id="size-choice" bind:value={settings.fontSize} onchange={persistSettings}><option value={14}>Small · 14px</option><option value={16}>Default · 16px</option><option value={18}>Large · 18px</option><option value={20}>Larger · 20px</option></select></section><section class="settings-section voice-quality"><h3>Voice quality</h3><label for="voice-bitrate-choice">Voice bitrate</label><select id="voice-bitrate-choice" bind:value={voiceBitrate} onchange={chooseVoiceBitrate}>{#each VOICE_BITRATES as choice (choice)}<option value={choice}>{kbps(choice)}{choice === 128000 ? " · default" : ""}</option>{/each}</select><p>Mono Opus, 64 to 256 kb/s. A server's bandwidth limit can lower it, never raise it.</p>{#if voice}<p class="voice-bitrate" role="status">{voiceServer?.name}: {voice.bitrate_why}</p>{/if}</section><section class="settings-section"><h3>Privacy & storage</h3><p>Bookmarks, trusted certificates and observed chat history are stored locally in this browser. Legacy servers can read chat; the relay only forwards encrypted traffic.</p><p>Links and received images never cause automatic requests to another site.</p></section>{@render voiceProcessingSettings()}{#if selected}<section class="settings-section"><h3>{selected.name}</h3><p class="muted">{selected.host}:{selected.port} · {selected.username}</p><button onclick={() => { showSettings = false; editServer(); }}>Edit server bookmark</button>{#if capabilities.includes("server_order")}<div class="server-order-actions"><button disabled={orderPending || servers[0]?.id === selected.id} onclick={()=>moveOrderKey(selected.id,-1)}>Move server earlier</button><button disabled={orderPending || servers.at(-1)?.id === selected.id} onclick={()=>moveOrderKey(selected.id,1)}>Move server later</button></div>{/if}{#if capabilities.includes("remove_server")}<button class="danger" onclick={async () => { await act("remove_server",selectedIndex); showSettings = false; }}>Remove server</button>{/if}</section>{/if}<footer class="settings-footer"><span class="brand-word">nuschl<span>web</span></span><small>Built for conversations.</small></footer></dialog>{/if}
