// SPDX-License-Identifier: EUPL-1.2
// A voice toggle waits until the adapter's view moved past the one the click
// carried: the echo then names the view the next click is sent with. The wait
// ends with its route, at `until`, and for a view that predates `settling`.
const PENDING_MS = 2000;
export function audioPending(voice, now) {
  return {server: voice.server_id, generation: voice.voice_generation, until: now + PENDING_MS};
}
export function audioPendingSettled(pending, voice, now) {
  if (!voice || voice.server_id !== pending.server) return true;
  if (voice.settling === undefined) return true;
  if (voice.settling === true) return false;
  if (now >= pending.until) return true;
  return voice.voice_generation !== pending.generation;
}
