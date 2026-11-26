// SPDX-License-Identifier: EUPL-1.2
// The call's stream worker (crates/stream-wasm, SECURITY.md): the one place that
// holds the call link's secrets and every key. The page posts `{id, request}` and
// receives `{id, reply}` or `{id, error}`; a trap posts `{fatal}` and ends the worker.
// The page's encoded transforms run here: `{side: "receive", peer, track}` decrypts a
// publisher's frames, `{side: "send", encoding}` encrypts this participant's. For each
// video keyframe whose Dependency Descriptor names its picture's size, a receive
// transform posts `{picture: {track, width, height}}` (view.js crops to it).
import init, { StreamWorker } from "./nuschl_stream_wasm.js";
import { unsized } from "./frames.js";

const ready = init().then(() => new StreamWorker());
const TAB_AUDIO = 3;
const MAX_OPUS_PACKET = 1275;
const SFRAME_OVERHEAD = 34; // envelope, header and authentication tag
/** Each publisher's receive transformers, which ask for a key frame once its keys arrive. */
const receivers = new Map();

function fatal(error) {
  postMessage({ fatal: String(error) });
  close();
}

/** Runs `call` on the worker; a thrown string is an error, anything else a trap. */
function run(worker, call) {
  try {
    return { value: call(worker) };
  } catch (error) {
    if (typeof error === "string") return { error };
    fatal(error);
    return { trapped: true };
  }
}

addEventListener("message", async ({ data: { id, request } }) => {
  const worker = await ready.catch(fatal);
  if (!worker) return;
  const { value: reply, error, trapped } = run(worker, (w) => JSON.parse(w.handle(request, Date.now())));
  if (trapped) return;
  if (error !== undefined) return postMessage({ id, error });
  postMessage({ id, reply });
  for (const event of reply.events ?? []) {
    if (event.kind !== "keyed") continue;
    for (const transformer of receivers.get(event.peer) ?? []) {
      Promise.resolve().then(() => transformer.sendKeyFrameRequest()).catch(() => {});
    }
  }
});

addEventListener("rtctransform", async ({ transformer }) => {
  const worker = await ready.catch(fatal);
  if (!worker) return;
  const { side, peer, encoding, audio, track } = transformer.options;
  let code;
  if (side === "receive" && audio) {
    code = (frame) => frame.data.byteLength <= MAX_OPUS_PACKET + SFRAME_OVERHEAD ?
      worker.decrypt_audio(peer, new Uint8Array(frame.data), Date.now()) : null;
  } else if (side === "receive") {
    if (!receivers.has(peer)) receivers.set(peer, new Set());
    receivers.get(peer).add(transformer);
    code = (frame) => {
      if (frame.type === "key") {
        const { width, height } = frame.getMetadata();
        if (width && height) postMessage({ picture: { track, width, height } });
      }
      return worker.decrypt(peer, unsized(new Uint8Array(frame.data)), Date.now());
    };
  } else {
    code = (frame) => encoding === TAB_AUDIO && frame.data.byteLength > MAX_OPUS_PACKET ? null :
      worker.encrypt(encoding, frame.type === "key", new Uint8Array(frame.data));
  }
  // A frame that fails to decrypt or encrypt is dropped.
  const frames = new TransformStream({
    transform(frame, controller) {
      const { value } = run(worker, () => code(frame));
      if (!value) return;
      frame.data = value.buffer;
      controller.enqueue(frame);
    },
  });
  await transformer.readable.pipeThrough(frames).pipeTo(transformer.writable).catch(() => {});
  receivers.get(peer)?.delete(transformer);
  if (receivers.get(peer)?.size === 0) receivers.delete(peer);
});
