// SPDX-License-Identifier: EUPL-1.2
import {context,sameContext,sameDestination,validPacket,FRAME,RATE,MAX_PENDING,MAX_CAPTURE,OUTPUT_CHANNELS} from "./audio-protocol.js";
import {BlockRing,PLAYOUT_BLOCKS,PLAYOUT_FRAME,CAPTURE_BLOCKS} from "./audio-worklet.js";

// The UI owns this adapter; the codec worker receives only PCM/Opus, local
// epochs and speaker numbers. No SDK client, identity, endpoint or key crosses.
export class BrowserAudio {
  constructor({onPacket=()=>{},onReady=()=>{},onState=()=>{},onStopped=()=>{}}={}) {
    this.onPacket=onPacket;this.onReady=onReady;this.onState=onState;this.onStopped=onStopped;
    this.voice=null;this.ctx=null;this.worker=null;this.stream=null;this.source=null;
    this.workerReady=false;this.nodesReady=false;this.disposed=false;
    this.epoch=0;this.receiveEpoch=0;this.job=0;this.intent=0;this.consent=null;
    this.localBlocked=false;this.localDeaf=false;this.pendingCapture=null;this.captureFailed=-1;
    this.decodeJobs=new Map();this.txJobs=new Map();
    this.sequence=0n;this.baseSequence=0n;this.lastReady="";this.transportStalled=false;
    this.lastProgress=0;this.initializing=null;this.applied={};this.phase="idle";
  }
  status(phase,message="") {
    this.phase=phase;
    this.onState({phase,message,processing:{...this.applied},microphone:Boolean(this.stream&&this.captureRunning),
      playback:Boolean(this.playbackEnabled()),contextState:this.ctx?.state||"closed"});
  }
  captureEnabled() {
    return Boolean(this.voice?.joined&&this.voice.bitrate>=6000&&!this.voice.muted&&!this.voice.deafened&&!this.localBlocked&&!this.localDeaf&&
      this.consent&&sameDestination(this.consent,this.voice)&&!this.awaitingUnmute&&
      !this.transportStalled&&this.workerReady&&this.nodesReady&&this.ctx?.state==="running");
  }
  playbackEnabled() {
    return Boolean(this.voice?.joined&&!this.voice.deafened&&!this.localDeaf&&this.workerReady&&this.nodesReady&&this.ctx?.state==="running");
  }
  // MUST be invoked directly by a trusted Join/Enable audio/Unmute gesture,
  // before awaiting a server command. Permission acquisition is never implicit.
  activateGesture({microphone=false}={}) {
    if(this.disposed)return Promise.reject(new Error("Audio is closed."));
    if(!globalThis.isSecureContext||!globalThis.AudioContext||!globalThis.AudioWorkletNode) {
      this.status("unavailable","Voice requires HTTPS, Web Audio and AudioWorklet support.");
      return Promise.resolve(false);
    }
    // The codec worker and the worklets share memory (nix/web-module.nix).
    if(!globalThis.crossOriginIsolated||!globalThis.SharedArrayBuffer) {
      this.status("unavailable","Voice requires a browser and web server with cross-origin isolation.");
      return Promise.resolve(false);
    }
    try {
      this.status("starting","Enabling browser audio.");
      if(!this.ctx) {
        this.ctx=new AudioContext({sampleRate:RATE,latencyHint:"interactive"});
        if(this.ctx.sampleRate!==RATE)throw new Error("This browser cannot provide a 48 kHz audio context.");
        this.ctx.onstatechange=()=>{
          if(this.ctx?.state==="running"){this.applyPolicy();return;}
          const capturing=Boolean(this.stream);
          this.stopCapture(true);this.resetMedia();
          if(capturing)this.stopped();else this.status("suspended","Enable audio to resume playback.");
        };
      }
      // Keep this call synchronous with the gesture for Safari/mobile autoplay.
      const resumed=this.ctx.resume();
      // Explicit playback consent can reopen the local deafen gate. The SDK's
      // authoritative deafened flag still blocks sound until its acknowledgment.
      // This never reopens capture or restores a microphone permission intent.
      this.localDeaf=false;
      if(microphone&&this.voice?.joined) {
        this.transportStalled=false;
        this.intent++;this.captureFailed=-1;this.consent=context(this.voice);
        this.awaitingUnmute=Boolean(this.localBlocked||this.voice.muted||this.voice.deafened);
        this.unmuteGeneration=this.voice.voice_generation;
        this.localBlocked=false;this.localDeaf=false;
        // Start permission acquisition in the same gesture, but never enable
        // captured PCM until the authoritative join/unmute state also permits it.
        this.acquireCapture();
      }
      if(!this.initializing)this.initializing=this.initialize().catch(error=>{
        this.fail(error);throw error;
      });
      return Promise.all([resumed,this.initializing]).then(()=>{
        this.applyPolicy();return true;
      }).catch(error=>{this.status("unavailable",String(error?.message||error));return false;});
    } catch(error) {this.fail(error);return Promise.resolve(false);}
  }
  async initialize() {
    if(!this.workletLoaded) {
      await this.ctx.audioWorklet.addModule(new URL("./audio-worklet.js",import.meta.url));
      this.workletLoaded=true;
    }
    if(this.disposed)return;
    const playout = new SharedArrayBuffer(BlockRing.bytes(PLAYOUT_BLOCKS, PLAYOUT_FRAME * OUTPUT_CHANNELS, Float64Array));
    const capture = new SharedArrayBuffer(BlockRing.bytes(CAPTURE_BLOCKS, FRAME));
    this.playoutPolicy = new Int32Array(new SharedArrayBuffer(4));
    this.capturePolicy = new Int32Array(new SharedArrayBuffer(4));
    this.captureNode = new AudioWorkletNode(this.ctx, "nuschl-capture", {
      numberOfInputs:1, numberOfOutputs:1, outputChannelCount:[1], channelCount:1, channelCountMode:"explicit",
      processorOptions:{buffer:capture, policy:this.capturePolicy},
    });
    this.playbackNode = new AudioWorkletNode(this.ctx, "nuschl-playback", {
      numberOfInputs:0, numberOfOutputs:1, outputChannelCount:[OUTPUT_CHANNELS],
      processorOptions:{buffer:playout, policy:this.playoutPolicy},
    });
    this.output=this.ctx.createGain();this.output.gain.value=0;
    this.captureNode.connect(this.ctx.destination); // processor emits zeros only
    this.playbackNode.connect(this.output).connect(this.ctx.destination);
    this.nodesReady=true;
    const worker = this.worker = new Worker(new URL("./audio-codec.worker.js", import.meta.url), {type:"module", name:"nuschl-audio-codecs"});
    worker.onerror=()=>{if(this.worker===worker)this.fail(new Error("The isolated audio codec worker failed. Enable audio to retry."));};
    worker.onmessage=({data})=>{if(this.worker===worker)this.workerMessage(data);};
    const channel = new MessageChannel();
    worker.postMessage({kind:"rings", playout, capture});
    worker.postMessage({kind:"playout-port", port:channel.port1}, [channel.port1]);
    this.playbackNode.port.postMessage({kind:"wake-port", port:channel.port2}, [channel.port2]);
    this.lastProgress=performance.now();
    this.watchdog=setInterval(()=>this.watch(),500);
    this.resetMedia();
  }
  // The codec worker posts "ready" once its WASM is compiled, then a heartbeat
  // every 500 ms. A loaded device delays both by seconds while the worker
  // still works, and frames it is late for expire. Silent for 30 s, it has hung.
  watch() {
    if(this.worker&&performance.now()-this.lastProgress>30000)this.fail(new Error("The audio codec worker stopped responding."));
  }
  syncVoice(value) {
    const next=context(value);
    const old=this.voice;
    const incoming=next?{...next,joined:value.joined===true,muted:value.muted!==false,
      deafened:value.deafened!==false,can_transmit:value.can_transmit===true,
      bitrate:Number.isInteger(value.bitrate)&&value.bitrate>=6000&&value.bitrate<=256000?value.bitrate:0}:null;
    if(!sameDestination(old,incoming)) {
      this.stopCapture(true);this.localBlocked=false;this.localDeaf=false;this.sequence=0n;this.transportStalled=false;
    }
    if(this.awaitingUnmute&&incoming&&!incoming.muted&&!incoming.deafened&&incoming.voice_generation!==this.unmuteGeneration)this.awaitingUnmute=false;
    this.voice=incoming;
    if(!incoming?.joined||incoming.muted||incoming.deafened||!incoming.bitrate) {
      // A permission request initiated by this explicit unmute may finish while
      // waiting for its acknowledgment. It remains disabled and time bounded.
      if(!this.awaitingUnmute||!incoming?.bitrate)this.stopCapture(true);
    }
    const changed=old?.bitrate!==incoming?.bitrate||!sameContext(old,incoming)||old?.channel_id!==incoming?.channel_id||old?.joined!==incoming?.joined||old?.muted!==incoming?.muted||old?.deafened!==incoming?.deafened;
    if(changed)this.resetMedia({preservePlayback:sameDestination(old,incoming)&&old?.joined&&incoming?.joined&&
      !old.deafened&&!incoming.deafened&&!this.localDeaf&&this.ctx?.state==="running"});
    this.applyPolicy();
    if(!incoming)this.status("idle");
  }
  setLocalPolicy({muted=false,deafened=false}={}) {
    if(!muted&&!deafened)return; // only authoritative state/gesture can reopen
    if(this.voice?.can_transmit&&!this.localBlocked)this.sendTerminator();
    this.localBlocked=true;this.localDeaf=Boolean(deafened);
    this.stopCapture(true);this.resetMedia({preservePlayback:!deafened});this.applyPolicy();
    this.status(deafened?"deafened":"muted");
  }
  async acquireCapture() {
    if(this.pendingCapture||!this.consent||this.captureFailed===this.intent||!this.voice?.joined)return;
    if(!navigator.mediaDevices?.getUserMedia){this.status("unavailable","Microphone capture is unavailable.");return;}
    const intent=this.intent,destination={...this.consent};
    this.status("permission-pending","Allow microphone access to speak.");
    this.pendingCapture=navigator.mediaDevices.getUserMedia({video:false,audio:{sampleRate:{ideal:RATE},channelCount:{ideal:1},echoCancellation:true,noiseSuppression:true,autoGainControl:true}});
    try {
      const stream=await this.pendingCapture;
      if(this.disposed||intent!==this.intent||!sameDestination(destination,this.voice)||!this.consent) {
        for(const track of stream.getTracks())track.stop();return;
      }
      this.stream=stream;
      for(const track of stream.getAudioTracks()) {
        track.enabled=false;
        track.onended=track.onmute=()=>{this.stopCapture(true);this.resetMedia({preservePlayback:true});this.reportReady();this.stopped();};
      }
      const settings=stream.getAudioTracks()[0]?.getSettings()||{};
      this.applied={echoCancellation:settings.echoCancellation??"unknown",noiseSuppression:settings.noiseSuppression??"unknown",autoGainControl:settings.autoGainControl??"unknown",sampleRate:settings.sampleRate??"unknown"};
      this.applyPolicy();
      clearTimeout(this.permissionHold);
      this.permissionHold=setTimeout(()=>{
        if(this.stream===stream&&!this.captureEnabled()){this.stopCapture(true);this.reportReady();this.status("muted","The microphone is muted. Unmute to retry.");}
      },5000);
    } catch(error) {
      if(intent===this.intent){this.captureFailed=intent;this.status(error?.name==="NotAllowedError"?"denied":"unavailable",String(error?.message||error));}
    } finally {
      this.pendingCapture=null;
      if(this.consent&&!this.stream&&intent!==this.intent)this.acquireCapture();
    }
  }
  // The browser stopped or muted capture (a hidden page, another app, an
  // unplugged device), so Nuschl mutes. Only the user's Unmute opens it again.
  stopped() {this.onStopped();this.status("stopped","Microphone stopped");}
  stopCapture(clearConsent) {
    if(clearConsent){this.intent++;this.consent=null;this.awaitingUnmute=false;}
    clearTimeout(this.permissionHold);
    this.source?.disconnect();this.source=null;
    if(this.stream)for(const track of this.stream.getTracks()){track.onended=null;track.onmute=null;track.stop();}
    this.stream=null;
    this.captureRunning=false;
    this.configureCapture(false);
  }
  configureWorker(capture=false) {
    this.worker?.postMessage({kind:"configure",epoch:this.epoch,capture,bitrate:this.voice?.bitrate||0,
      receive_epoch:this.receiveEpoch,playback:this.playbackEnabled()});
  }
  configurePlayback() {
    if (this.playoutPolicy) Atomics.store(this.playoutPolicy, 0, this.playbackEnabled() ? this.receiveEpoch : 0);
  }
  configureCapture(enabled) {
    if (this.capturePolicy) Atomics.store(this.capturePolicy, 0, enabled ? this.epoch : 0);
  }
  resetMedia({preservePlayback=false}={}) {
    ++this.epoch;this.baseSequence=this.sequence;
    // SDK send promises settle only on a real reply or after the SDK owner
    // terminates its worker. TX epoch changes do not acknowledge its inbox.
    if(!preservePlayback||!this.playbackEnabled()) {
      ++this.receiveEpoch;
      // Credits survive revocation until the worker actually acknowledges old
      // inbox entries. Rapid revoke/reopen cannot enqueue another 16 per epoch.
      if(!this.worker)this.decodeJobs.clear();
      if(this.output)this.output.gain.setValueAtTime(0,this.ctx.currentTime);
      this.configurePlayback();
      this.playbackRunning=this.playbackEnabled();
    }
    // Even a TX-only reset invalidates all captured frames. Authorized
    // RX keeps its independent epoch, encoded queue and Worklet audio clock.
    this.configureWorker(false);
    this.configureCapture(false);
    this.captureRunning=false;
  }
  applyPolicy() {
    if(this.disposed)return;
    const playback=this.playbackEnabled();
    const playbackChanged=playback!==this.playbackRunning;
    // Every re-enable starts a fresh authorization. A disabled barrier from an
    // earlier suspended/listening state may still be queued on the other port.
    if(playbackChanged&&playback)++this.receiveEpoch;
    const capture=this.captureEnabled()&&Boolean(this.stream)&&!this.stream.getAudioTracks().some(t=>t.muted||t.readyState!=="live");
    if(capture!==Boolean(this.captureRunning)) {
      this.captureRunning=capture;
      for(const track of this.stream?.getAudioTracks()||[])track.enabled=capture;
      if(capture&&!this.source){this.source=this.ctx.createMediaStreamSource(this.stream);this.source.connect(this.captureNode);}
      this.configureWorker(capture);
      this.configureCapture(capture);
    }
    if(playbackChanged) {
      this.playbackRunning=playback;
      this.configureWorker(capture);this.configurePlayback();
    }
    if(this.output)this.output.gain.setValueAtTime(playback?1:0,this.ctx.currentTime);
    this.reportReady();
    if(this.consent&&!this.stream&&!this.pendingCapture&&!this.awaitingUnmute)this.acquireCapture();
    if(capture)this.status("capturing");
    else if(this.voice?.deafened||this.localDeaf)this.status("deafened");
    else if(this.pendingCapture)this.status("permission-pending","Allow microphone access to speak.");
    else if(!["denied","unavailable","error","transport-error","stopped"].includes(this.phase)&&this.voice?.joined)
      this.status(this.voice.muted||this.localBlocked?"muted":"listening");
  }
  reportReady() {
    if(!this.voice)return;
    const ready={server_id:this.voice.server_id,connection_generation:this.voice.connection_generation,
      voice_generation:this.voice.voice_generation,capture:Boolean(this.captureRunning),playback:this.playbackEnabled()};
    const encoded=JSON.stringify(ready);if(encoded===this.lastReady)return;this.lastReady=encoded;
    const epoch=this.epoch,expected={...this.voice};
    let result;try{result=this.onReady(ready);}catch(error){result=Promise.reject(error);}
    Promise.resolve(result).catch(()=>{if(this.epoch===epoch&&sameContext(expected,this.voice))this.status("transport-error","The voice connection is unavailable.");});
  }
  receive(packet) {
    if(!this.playbackEnabled()||!validPacket(packet,this.voice)||this.decodeJobs.size>=MAX_PENDING)return false;
    const job=++this.job;this.decodeJobs.set(job,{epoch:this.receiveEpoch});
    // Never transfer a buffer owned by the session/TLS worker: the SDK bridge
    // already supplies a copy, and this final slice makes ownership explicit.
    const arrival = performance.now(), opus = packet.opus.slice();
    this.worker.postMessage({kind:"decode", epoch:this.receiveEpoch, job, speaker:packet.speaker,
      sequence:packet.sequence, opus, end:packet.end, volume:packet.volume, arrival}, [opus.buffer]);
    return true;
  }
  workerMessage(data) {
    this.lastProgress=performance.now();
    if(data?.kind==="ready"){this.workerReady=true;this.resetMedia();this.applyPolicy();return;}
    if(!data)return;
    if(data.kind==="packet-queued"||data.kind==="media-error"&&data.operation==="decode") {
      const pending=this.decodeJobs.get(data.job);
      if(pending?.epoch===data.epoch)this.decodeJobs.delete(data.job);
      return;
    }
    if(data.epoch===this.receiveEpoch&&data.kind==="playout-error") {
      this.fail(new Error(String(data.message||"Audio playout failed.").slice(0,200)));return;
    }
    if (data.kind === "encoded") {
      this.worker?.postMessage({kind:"encoded-ack"});
      if (data.epoch === this.epoch && this.captureRunning && this.voice?.can_transmit && !this.localBlocked &&
          Number.isSafeInteger(data.frame) && data.frame >= 0 && data.opus instanceof Uint8Array &&
          data.opus.length > 0 && data.opus.length <= 960 && Number.isFinite(data.deadline) &&
          data.deadline >= performance.timeOrigin + performance.now())
        this.sendPacket(this.baseSequence + BigInt(data.frame) * 2n, data.opus, false);
    }
  }

  // Six unacknowledged sends bound the queue; frames beyond them are dropped.
  // A loaded SDK worker answers late and catches up, and one that stops
  // answering retires after the SDK request deadline, revoking audio.
  sendPacket(sequence,opus,end) {
    if(!this.voice||this.txJobs.size>=MAX_CAPTURE||sequence<0n||sequence>=(1n<<64n))return;
    this.sequence=sequence+2n;
    const job=++this.job,epoch=this.epoch;
    const pending={epoch};this.txJobs.set(job,pending);
    const packet={server_id:this.voice.server_id,connection_generation:this.voice.connection_generation,
      voice_generation:this.voice.voice_generation,sequence:String(sequence),opus,end};
    let result;try{result=this.onPacket(packet);}catch(error){result=Promise.reject(error);}
    if(!result||typeof result.then!=="function") {
      this.transportStalled=true;
      this.stopCapture(true);this.resetMedia({preservePlayback:true});this.applyPolicy();
      this.status("transport-error","The voice bridge must acknowledge queued audio packets.");return;
    }
    Promise.resolve(result).catch(()=>{
      if(this.epoch===epoch)this.status("transport-error","Voice could not be sent.");
    }).finally(()=>{
      if(this.txJobs.get(job)===pending)this.txJobs.delete(job);
    });
  }
  // The native Disconnected sound (apps/media-worker/src/sounds.rs): falling
  // bell notes A5, F5 and D5 at about -25 LUFS. It runs on the context's clock,
  // so a page frozen right after plays it when it resumes.
  playDisconnected() {
    const ctx=this.ctx;
    if(!ctx||ctx.state==="closed"||!this.voice||this.voice.deafened||this.localDeaf)return;
    for(const [onset,note,level] of [[0,880,1],[0.11,698.46,0.9],[0.22,587.33,0.85]])
      for(const [ratio,amplitude,decay] of [[1,1,0.5],[2,0.28,0.3],[3,0.08,0.18]]) {
        const start=ctx.currentTime+onset,tone=new OscillatorNode(ctx,{frequency:note*ratio}),gain=new GainNode(ctx,{gain:0});
        gain.gain.setValueAtTime(0,start);gain.gain.linearRampToValueAtTime(0.08*level*amplitude,start+0.008);
        gain.gain.setTargetAtTime(0,start+0.008,decay);gain.gain.setTargetAtTime(0,ctx.currentTime+0.6,0.03);
        tone.connect(gain).connect(ctx.destination);tone.start(start);tone.stop(ctx.currentTime+0.75);
      }
  }
  sendTerminator(){this.sendPacket(this.sequence,new Uint8Array([0xf8,0xff,0xfe]),true);}
  fail(error) {
    this.stopCapture(true);this.workerReady=false;this.worker?.terminate();this.worker=null;
    clearInterval(this.watchdog);this.resetMedia();this.reportReady();
    this.playbackNode?.port.postMessage({kind:"close"});
    this.captureNode?.disconnect();this.playbackNode?.disconnect();this.output?.disconnect();
    this.captureNode=null;this.playbackNode=null;this.output=null;this.nodesReady=false;this.initializing=null;
    this.status("error",String(error?.message||error));
  }
  async dispose() {
    this.stopCapture(true);this.workerReady=false;this.resetMedia();this.reportReady();this.disposed=true;
    clearInterval(this.watchdog);this.worker?.terminate();this.worker=null;
    this.decodeJobs.clear();
    this.playbackNode?.port.postMessage({kind:"close"});
    this.captureNode?.disconnect();this.playbackNode?.disconnect();this.output?.disconnect();
    if(this.ctx){this.ctx.onstatechange=null;await this.ctx.close();}this.ctx=null;
  }
}
