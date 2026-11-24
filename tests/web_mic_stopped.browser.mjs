// SPDX-License-Identifier: EUPL-1.2
// The packaged web app in nix/vm-web.nix with Chromium's fake microphone playing
// a synthetic tone: when the browser ends or mutes the capture track, Nuschl
// mutes, says "Microphone stopped" and asks for no new capture. A second profile
// measures received PCM from the deployed app before the capture track is stopped.
// Usage: node tests/web_mic_stopped.browser.mjs <chromium> <playwright-core> <tone.wav>
import assert from "node:assert/strict";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [chromiumPath,playwright,tone]=process.argv.slice(2);
assert.ok(chromiumPath&&playwright&&tone,"usage: <chromium> <playwright-core> <tone.wav>");
const {chromium}=await import(pathToFileURL(join(playwright,"index.mjs")).href);
// Plain HTTP in the VM: treat the site as the HTTPS origin it is in production.
const browser=await chromium.launch({headless:true,executablePath:chromiumPath,
  args:["--unsafely-treat-insecure-origin-as-secure=http://web","--use-fake-device-for-media-stream",
    `--use-file-for-fake-audio-capture=${tone}`,"--disable-audio-output"]});
// Hang guards sized for a loaded host: within them, slowness is timing (#376).
const HANG=180000;
// The open pages, to show their voice status and audio notice after a failure.
const pages=new Map();
const shown=page=>page.evaluate(()=>[".voice-description small",".audio-notice"]
  .map(selector=>document.querySelector(selector)?.textContent).join(" | ")).catch(error=>String(error));
async function openVoice(context,page,user) {
  context.setDefaultTimeout(HANG);context.setDefaultNavigationTimeout(HANG);
  await page.goto(`http://web/app/#s=murmur:64738&u=${user}`);
  assert.equal(await page.evaluate(()=>crossOriginIsolated),true);
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected");
  await page.getByRole("button",{name:"Join voice",exact:true}).click();
}
try {
  for(const event of ["ended","mute"]) {
    const context=await browser.newContext({viewport:{width:1280,height:800},permissions:["microphone"]});
    const page=await context.newPage(),errors=[];
    const receiver=event==="ended"?await browser.newContext({viewport:{width:1280,height:800}}):null;
    const listener=await receiver?.newPage();
    pages.set(`mic-${event}`,page);
    if(listener) {
      pages.set("mic-listener",listener);
      listener.on("pageerror",error=>errors.push(error.message));
      await listener.addInitScript(()=>{
        const RealWorklet=AudioWorkletNode;
        globalThis.AudioWorkletNode=class extends RealWorklet {
          constructor(context,name,options) {
            super(context,name,options);
            if(name!=="nuschl-playback")return;
            // A loaded sender drops late frames, so sound arrives in short
            // bursts; the longest window (0.68 s) keeps one visible between polls.
            const analyser=context.createAnalyser();analyser.fftSize=32768;
            this.connect(analyser);
            const samples=new Float32Array(analyser.fftSize);
            globalThis.receivedVoice=()=>{
              analyser.getFloatTimeDomainData(samples);
              const power=samples.reduce((sum,value)=>sum+value*value,0);
              return power/samples.length>1e-6;
            };
          }
        };
      });
      await openVoice(receiver,listener,"mic-listener");
    }
    page.on("pageerror",error=>errors.push(error.message));
    // Counts capture requests and keeps the tracks, to end them as the browser would.
    await page.addInitScript(()=>{
      const getUserMedia=navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
      window.capture={calls:0,tracks:[]};
      navigator.mediaDevices.getUserMedia=async constraints=>{
        capture.calls++;const stream=await getUserMedia(constraints);capture.tracks.push(...stream.getAudioTracks());return stream;
      };
    });
    await openVoice(context,page,`mic-${event}`);
    const status=page.locator(".voice-description small").first();
    await page.getByRole("button",{name:"Unmute microphone",exact:true}).click();
    const unmuted=Date.now();
    await status.filter({hasText:"Microphone on"}).waitFor();
    if(listener) {
      await listener.waitForFunction(()=>globalThis.receivedVoice?.());
      console.log(`synthetic microphone reached the second packaged app's playback worklet ${((Date.now()-unmuted)/1000).toFixed(1)} s after Unmute`);
      await receiver.close();
    }
    await page.evaluate(event=>capture.tracks[0].dispatchEvent(new Event(event)),event);
    await status.filter({hasText:"Microphone stopped"}).waitFor();
    // The server acknowledged the mute.
    await page.getByRole("button",{name:"Unmute microphone",exact:true}).waitFor();
    assert.equal(await page.evaluate(()=>capture.tracks[0].readyState),"ended");
    // The page coming back opens nothing.
    await page.evaluate(()=>document.dispatchEvent(new Event("visibilitychange")));
    await page.waitForTimeout(1000);
    assert.equal(await page.evaluate(()=>capture.calls),1);
    assert.match(await status.textContent(),/Microphone stopped/);
    assert.deepEqual(errors,[]);
    await context.close();
    console.log(`capture track ${event}: muted, "Microphone stopped", one getUserMedia call`);
  }
} catch(error) {
  for(const [name,page] of pages)if(!page.isClosed())console.error(`${name}: ${await shown(page)}`);
  throw error;
} finally {
  await browser.close();
}
