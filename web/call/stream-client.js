// SPDX-License-Identifier: EUPL-1.2
// The page's side of the stream worker (stream.worker.js): requests in the message
// interface of crates/stream-wasm, answered in order. The page sees the worker's
// replies only: room ids, proofs, encrypted key messages and key events.

const WORKER = "/stream.worker.js";
// The stream host requires Trusted Types for script URLs; this policy admits the
// stream worker only.
const policy = globalThis.trustedTypes?.createPolicy("nuschl-stream-worker", {
  createScriptURL(url) {
    if (url !== WORKER) throw new TypeError("not the stream worker");
    return url;
  },
});

export class StreamClient {
  #pending = new Map();
  #next = 0;
  #closed = false;

  /** `onfatal` runs once when the worker ends. */
  constructor(onfatal = () => {}, create = () =>
    new Worker(policy ? policy.createScriptURL(WORKER) : WORKER, { type: "module" })) {
    this.worker = create();
    const fail = () => {
      if (this.#closed) return;
      this.close();
      onfatal();
    };
    this.worker.addEventListener("message", ({ data }) => {
      if ("fatal" in data) return fail();
      const pending = this.#pending.get(data.id);
      if (!pending) return;
      this.#pending.delete(data.id);
      if ("error" in data) pending.reject(data.error);
      else pending.resolve(data.reply);
    });
    this.worker.addEventListener("error", fail);
  }

  /** The reply to `request` (`{kind, …}`); rejects with the worker's error. */
  request(request) {
    if (this.#closed) return Promise.reject(new Error("Stream worker ended"));
    const id = ++this.#next;
    this.worker.postMessage({ id, request: JSON.stringify({ v: 1, ...request }) });
    return new Promise((resolve, reject) => this.#pending.set(id, { resolve, reject }));
  }

  close() {
    if (this.#closed) return;
    this.#closed = true;
    for (const { reject } of this.#pending.values()) reject(new Error("Stream worker ended"));
    this.#pending.clear();
    this.worker.terminate();
  }
}
