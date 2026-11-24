// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {BlockRing,CaptureFrames,CAPTURE_BLOCKS,AUDIO_FRAME} from "../web/app/src/audio-worklet.js";
import {BrowserAudio} from "../web/app/src/audio.js";
import {context,MAX_CAPTURE} from "../web/app/src/audio-protocol.js";

const policyBuffer=()=>new Int32Array(new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT));

function captureFixture() {
  const buffer=new SharedArrayBuffer(BlockRing.bytes(CAPTURE_BLOCKS,AUDIO_FRAME));
  const policy=policyBuffer();
  const capture=new CaptureFrames(new BlockRing(buffer,CAPTURE_BLOCKS,AUDIO_FRAME),policy);
  return {capture,reader:new BlockRing(buffer,CAPTURE_BLOCKS,AUDIO_FRAME),
    configure:epoch=>Atomics.store(policy,0,epoch)};
}

test("the capture ring stays bounded and keeps the sequence across a full buffer",()=>{
  const {capture,reader,configure}=captureFixture();
  configure(1);
  const input=new Float32Array(AUDIO_FRAME).fill(.25);
  for(let index=0;index<CAPTURE_BLOCKS+3;index++)capture.append(input);
  assert.equal(reader.size(),CAPTURE_BLOCKS);
  for(let index=0;index<CAPTURE_BLOCKS;index++) {
    const at=reader.head();
    assert.equal(reader.tags[at*2],1);
    assert.equal(reader.tags[at*2+1],index);
    assert.ok(reader.data.subarray(at*AUDIO_FRAME,(at+1)*AUDIO_FRAME).every(value=>value===.25));
    reader.pop();
  }
  capture.append(input);
  assert.equal(reader.tags[reader.head()*2+1],CAPTURE_BLOCKS+3);
  reader.pop();
  configure(0);
  capture.append(input);
  assert.equal(reader.size(),0);
});

test("partial capture from before a mute or route change cannot enter the next frame",()=>{
  const {capture,reader,configure}=captureFixture();
  configure(1);
  capture.append(new Float32Array(500).fill(.75));
  configure(2);
  capture.append(new Float32Array(AUDIO_FRAME).fill(-.25));
  assert.equal(reader.size(),1);
  const at=reader.head();
  assert.equal(reader.tags[at*2],2);
  assert.equal(reader.tags[at*2+1],0);
  assert.ok(reader.data.subarray(at*AUDIO_FRAME,(at+1)*AUDIO_FRAME).every(value=>value===-.25));
});

function voice(generation="1") {
  return {server_id:"web:"+"u".repeat(128)+"@"+"a".repeat(253)+":64738",
    connection_generation:generation,voice_generation:"9007199254740993",channel_id:0,
    joined:true,muted:false,deafened:false,can_transmit:true,bitrate:128000};
}
const turn=()=>new Promise(resolve=>setImmediate(resolve));

test("valid long bookmark identities and exact u64 generations survive the audio boundary",()=>{
  const value=voice();assert.ok(context(value));assert.equal(context(value).voice_generation,"9007199254740993");
  assert.equal(context({...value,server_id:"x".repeat(1025)}),null);
  assert.equal(context({...value,voice_generation:"18446744073709551616"}),null);
});

test("old transport promises cannot consume or release the next connection's audio credits",async()=>{
  const completions=[];const audio=new BrowserAudio({onPacket:()=>new Promise(resolve=>completions.push(resolve))});
  const send=sequence=>audio.sendPacket(sequence,new Uint8Array([0xf8,0xff,0xfe]),false);
  audio.syncVoice(voice("1"));for(let index=0;index<=MAX_CAPTURE;index++)send(BigInt(2*index));
  assert.equal(completions.length,MAX_CAPTURE);
  audio.syncVoice(voice("2"));send(0n);
  assert.equal(audio.txJobs.size,MAX_CAPTURE);assert.equal(completions.length,MAX_CAPTURE,"epoch change cannot acknowledge old SDK inbox work");
  completions[0]();await turn();assert.equal(audio.txJobs.size,MAX_CAPTURE-1);
  send(0n);assert.equal(completions.length,MAX_CAPTURE+1);assert.equal(audio.txJobs.size,MAX_CAPTURE);
  for(const complete of completions.slice(1))complete();await turn();assert.equal(audio.txJobs.size,0);await audio.dispose();
});

test("a stalled transport closes capture instead of growing an unlimited RPC backlog",async()=>{
  let reply;const audio=new BrowserAudio({onPacket:()=>new Promise(resolve=>{reply=resolve;})});audio.syncVoice(voice());
  audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);
  await new Promise(resolve=>setTimeout(resolve,300));
  assert.equal(audio.transportStalled,true);assert.equal(audio.txJobs.size,1,"timeout is not an SDK acknowledgment");assert.equal(audio.phase,"transport-error");
  reply();await turn();assert.equal(audio.txJobs.size,0);await audio.dispose();
});

test("a codec worker that misses heartbeats for seconds keeps audio; one silent for 30 s has hung",async()=>{
  let terminated=false;const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.worker={postMessage:()=>{},terminate:()=>{terminated=true;}};audio.workerReady=true;
  audio.lastProgress=performance.now()-20000;audio.watch();
  assert.equal(terminated,false);assert.notEqual(audio.phase,"error");
  audio.lastProgress=performance.now()-30001;audio.watch();
  assert.equal(terminated,true);assert.equal(audio.phase,"error");
  await audio.dispose();
});

test("packet inbox stays bounded across stalls and rapid RX revoke/reopen until exact ACK",async()=>{
  const output=[],sent=[];const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:message=>sent.push(message),terminate:()=>{}};
  audio.playbackNode={port:{postMessage:message=>output.push(message)},disconnect:()=>{}};
  const packet=()=>({...audio.voice,speaker:7,sequence:"0",opus:new Uint8Array([0xf8,0xff,0xfe]),end:false,volume:1});
  for(let index=0;index<16;index++)assert.equal(audio.receive(packet()),true);
  const first=sent.find(message=>message.kind==="decode");
  assert.equal(audio.receive(packet()),false);
  await new Promise(resolve=>setTimeout(resolve,150));
  assert.equal(audio.receive(packet()),false,"elapsed time cannot release inbox credit");
  for(let index=0;index<20;index++) {
    audio.syncVoice({...voice(),voice_generation:String(100+index*2),deafened:true});
    audio.syncVoice({...voice(),voice_generation:String(101+index*2)});
    assert.equal(audio.receive(packet()),false);
  }
  assert.equal(audio.decodeJobs.size,16);
  audio.workerMessage({kind:"packet-queued",epoch:audio.receiveEpoch,job:first.job});
  assert.equal(audio.decodeJobs.size,16,"wrong RX cannot ACK an old entry");
  audio.workerMessage({kind:"packet-queued",epoch:first.epoch,job:first.job});
  assert.equal(audio.decodeJobs.size,15);assert.equal(audio.receive(packet()),true);
  audio.workerMessage({kind:"packet-queued",epoch:first.epoch,job:first.job});
  assert.equal(audio.decodeJobs.size,16,"duplicate cannot ACK a newer job");
  assert.deepEqual(output,[],"PCM and policy updates bypass the UI-side worklet port");
  audio.ctx=null;await audio.dispose();
});

test("a 90 ms codec worker stall keeps all captured frames in order",()=>{
  const {capture,reader,configure}=captureFixture();
  configure(1);
  const input=new Float32Array(AUDIO_FRAME/2).fill(.1);
  const frames=[];
  // Nine 10 ms callbacks run without a consumer.
  for(let step=0;step<100;step++) {
    capture.append(input);
    if(step>=20&&step<29)continue;
    while(reader.size()) {
      frames.push(reader.tags[reader.head()*2+1]);
      reader.pop();
    }
  }
  assert.deepEqual(frames,Array.from({length:50},(_,index)=>index));
});

test("expired and old-route encode results release their worker credits without transmitting",async()=>{
  const messages=[],packets=[];
  const audio=new BrowserAudio({onPacket:packet=>{packets.push(packet);return Promise.resolve();}});
  audio.syncVoice(voice("1"));
  audio.worker={postMessage:message=>messages.push(message),terminate:()=>{}};
  const old=audio.epoch;
  audio.syncVoice(voice("2"));
  audio.captureRunning=true;
  const deadline = performance.timeOrigin + performance.now() + 120;
  const result=(epoch, expiry=deadline)=>({kind:"encoded",epoch,frame:0,deadline:expiry,opus:new Uint8Array([0xf8,0xff,0xfe])});
  audio.workerMessage(result(old));
  audio.workerMessage(result(audio.epoch, deadline - 1000));
  assert.equal(packets.length,0);
  audio.workerMessage(result(audio.epoch));
  assert.equal(packets.length,1);
  assert.equal(packets[0].connection_generation,"2");
  assert.deepEqual(messages.filter(message=>message.kind==="encoded-ack"),Array(3).fill({kind:"encoded-ack"}));
  await turn();await audio.dispose();
});

test("mic mute and bitrate reset TX only, but deafen/route changes revoke RX",async()=>{
  const messages=[];const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:message=>messages.push(message),terminate:()=>{}};
  audio.playoutPolicy=policyBuffer();
  audio.applyPolicy();messages.length=0;
  const rx=audio.receiveEpoch,tx=audio.epoch;
  audio.setLocalPolicy({muted:true});assert.equal(audio.receiveEpoch,rx);assert.notEqual(audio.epoch,tx);
  audio.syncVoice({...voice(),voice_generation:"10",muted:true,bitrate:24000});
  assert.equal(audio.receiveEpoch,rx);assert.equal(Atomics.load(audio.playoutPolicy,0),rx);
  assert.ok(messages.every(message=>message.kind!=="configure"||message.receive_epoch===rx));
  audio.syncVoice({...voice(),voice_generation:"11",deafened:true});assert.notEqual(audio.receiveEpoch,rx);
  assert.equal(Atomics.load(audio.playoutPolicy,0),0);
  const deaf=audio.receiveEpoch;audio.syncVoice({...voice(),voice_generation:"12"});assert.notEqual(audio.receiveEpoch,deaf);
  const before=audio.receiveEpoch;audio.syncVoice({...voice(),channel_id:1});assert.notEqual(audio.receiveEpoch,before);
  assert.equal(Atomics.load(audio.playoutPolicy,0),audio.receiveEpoch);
  audio.ctx=null;await audio.dispose();
});

test("every playback reenable has a new RX authorization even when old disable messages are delayed",async()=>{
  const messages=[],audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:m=>messages.push(m),terminate:()=>{}};
  audio.playbackNode={port:{postMessage:()=>{}},disconnect:()=>{}};
  audio.applyPolicy();const initial=audio.receiveEpoch;
  audio.ctx.state="suspended";audio.applyPolicy();
  audio.ctx.state="running";audio.applyPolicy();
  assert.notEqual(audio.receiveEpoch,initial);
  const latest=messages.filter(m=>m.kind==="configure").at(-1);
  assert.equal(latest.playback,true);assert.equal(latest.receive_epoch,audio.receiveEpoch);
  audio.ctx=null;await audio.dispose();
});

test("explicit undeafen restores acknowledged playback while microphone capture stays closed",async()=>{
  const originals=new Map(["isSecureContext","crossOriginIsolated","AudioContext","AudioWorkletNode"].map(key=>[key,Object.getOwnPropertyDescriptor(globalThis,key)]));
  Object.defineProperty(globalThis,"isSecureContext",{value:true,configurable:true});
  Object.defineProperty(globalThis,"crossOriginIsolated",{value:true,configurable:true});
  Object.defineProperty(globalThis,"AudioContext",{value:class{},configurable:true});
  Object.defineProperty(globalThis,"AudioWorkletNode",{value:class{},configurable:true});
  const ready=[];const audio=new BrowserAudio({onReady:value=>ready.push(value)});
  try {
    audio.syncVoice({...voice(),muted:true,deafened:true,can_transmit:false});
    audio.ctx={state:"running",currentTime:0,resume:()=>Promise.resolve(),close:()=>Promise.resolve()};
    audio.workerReady=true;audio.nodesReady=true;audio.initializing=Promise.resolve();
    audio.worker={postMessage:()=>{},terminate:()=>{}};
    audio.captureNode={port:{postMessage:()=>{}},disconnect:()=>{}};
    audio.playbackNode={port:{postMessage:()=>{}},disconnect:()=>{}};
    audio.setLocalPolicy({muted:true,deafened:true});
    assert.equal(audio.playbackEnabled(),false);
    await audio.activateGesture();
    assert.equal(audio.playbackEnabled(),false,"SDK deafen acknowledgment still gates playback");
    audio.syncVoice({...voice(),voice_generation:"9007199254740994",muted:true,deafened:false,can_transmit:false});
    assert.equal(audio.playbackEnabled(),true);assert.equal(audio.captureEnabled(),false);
    assert.equal(audio.consent,null);assert.equal(ready.at(-1).capture,false);assert.equal(ready.at(-1).playback,true);
  } finally {
    await audio.dispose();
    for(const [key,descriptor] of originals)if(descriptor)Object.defineProperty(globalThis,key,descriptor);else delete globalThis[key];
  }
});


test("authenticated route targets reach only the codec and budget updates invalidate queued media",async()=>{
  const messages=[];const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:message=>messages.push(message),terminate:()=>{}};
  audio.captureNode={port:{postMessage:()=>{}},disconnect:()=>{}};
  audio.consent=context(audio.voice);
  audio.stream={getAudioTracks:()=>[{muted:false,readyState:"live"}],getTracks:()=>[]};
  audio.source={disconnect:()=>{}};
  audio.applyPolicy();
  assert.equal(messages.at(-1).bitrate,128000);
  const oldEpoch=audio.epoch;
  audio.syncVoice({...voice(),voice_generation:"9007199254740994",bitrate:24000});
  assert.notEqual(audio.epoch,oldEpoch);
  assert.equal(messages.at(-1).bitrate,24000);
  assert.ok(audio.captureEnabled());
  audio.syncVoice({...voice(),voice_generation:"9007199254740995",bitrate:0,can_transmit:false});
  assert.equal(audio.captureEnabled(),false);assert.equal(audio.stream,null);assert.equal(audio.consent,null);
  assert.equal(audio.playbackEnabled(),true,"a tiny upload budget does not disable listening");
  audio.ctx=null;await audio.dispose();
});

test("missing and malformed encoder targets cannot open capture",async()=>{
  const audio=new BrowserAudio();
  for(const bitrate of [undefined,-1,5999,256001,384000,510000,Infinity,"128000"]) {
    audio.syncVoice({...voice(),bitrate});assert.equal(audio.voice.bitrate,0);assert.equal(audio.captureEnabled(),false);
  }
  // Every requested choice the SDK can resolve to is accepted as is.
  for(const bitrate of [64000,96000,128000,192000,256000]) {
    audio.syncVoice({...voice(),bitrate});assert.equal(audio.voice.bitrate,bitrate);
  }
  await audio.dispose();
});

test("capture the browser ends, mutes or suspends mutes Nuschl and stays closed",async()=>{
  const keys=["isSecureContext","crossOriginIsolated","AudioContext","AudioWorkletNode"];
  const originals=new Map([...keys.map(key=>[key,Object.getOwnPropertyDescriptor(globalThis,key)]),["mediaDevices",Object.getOwnPropertyDescriptor(navigator,"mediaDevices")]]);
  Object.defineProperty(globalThis,"isSecureContext",{value:true,configurable:true});
  Object.defineProperty(globalThis,"crossOriginIsolated",{value:true,configurable:true});
  Object.defineProperty(globalThis,"AudioContext",{value:class{constructor(){this.state="running";this.sampleRate=48000;this.currentTime=0;}resume(){return Promise.resolve();}close(){return Promise.resolve();}},configurable:true});
  Object.defineProperty(globalThis,"AudioWorkletNode",{value:class{},configurable:true});
  let calls=0,track;
  Object.defineProperty(navigator,"mediaDevices",{configurable:true,value:{getUserMedia:async()=>{
    calls++;track={muted:false,readyState:"live",enabled:false,stop(){this.readyState="ended";},getSettings:()=>({})};
    return {getAudioTracks:()=>[track],getTracks:()=>[track]};
  }}});
  try {
    for(const stop of ["ended","mute","suspended"]) {
      calls=0;const stops=[];const audio=new BrowserAudio({onStopped:()=>stops.push(stop)});
      audio.syncVoice(voice());audio.initializing=Promise.resolve();
      audio.workerReady=true;audio.nodesReady=true;
      audio.worker={postMessage:()=>{},terminate:()=>{}};
      audio.captureNode={port:{postMessage:()=>{}},connect:()=>{},disconnect:()=>{}};
      audio.playbackNode={port:{postMessage:()=>{}},disconnect:()=>{}};
      await audio.activateGesture({microphone:true});
      audio.ctx.createMediaStreamSource=()=>({connect:()=>{},disconnect:()=>{}});
      await turn();audio.applyPolicy();
      assert.equal(audio.captureRunning,true,stop);assert.equal(calls,1);
      if(stop==="suspended"){audio.ctx.state="interrupted";audio.ctx.onstatechange();}else track[`on${stop}`]();
      assert.deepEqual(stops,[stop]);assert.equal(audio.phase,"stopped");
      assert.equal(audio.stream,null);assert.equal(track.readyState,"ended");
      // The page returns with Nuschl still unmuted (the mute is in flight): capture stays closed.
      audio.ctx.state="running";audio.ctx.onstatechange();
      audio.syncVoice({...voice(),voice_generation:"9007199254740994"});
      assert.equal(calls,1);assert.equal(audio.captureRunning,false);assert.equal(audio.phase,"stopped");
      audio.ctx.onstatechange=null;await audio.dispose();
    }
  } finally {
    for(const [key,descriptor] of originals){const target=key==="mediaDevices"?navigator:globalThis;if(descriptor)Object.defineProperty(target,key,descriptor);else delete target[key];}
  }
});
